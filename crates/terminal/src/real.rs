//! `portable-pty` implementation of the terminal service (ADR-0008 §1).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use termesh_core::{
    PtyEvent, TerminalExit, TerminalGeneration, TerminalId, TerminalSize, TerminalSpec,
};

use crate::{PtyError, PtyEventSink, PtyResult, PtyService};

const OUTPUT_CHUNK: usize = 32 * 1024;
const POST_EXIT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

struct LivePty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    process_exited: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    #[cfg(unix)]
    process_group: Option<i32>,
    #[cfg(unix)]
    group_cleanup: Option<Arc<Mutex<bool>>>,
    reader: Option<JoinHandle<()>>,
    waiter: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub struct RealPtyService {
    live: BTreeMap<TerminalId, LivePty>,
}

impl RealPtyService {
    pub fn new() -> Self {
        Self::default()
    }

    fn live(&self, terminal: TerminalId) -> PtyResult<&LivePty> {
        self.live.get(&terminal).ok_or(PtyError::UnknownTerminal(terminal))
    }

    fn live_mut(&mut self, terminal: TerminalId) -> PtyResult<&mut LivePty> {
        self.live.get_mut(&terminal).ok_or(PtyError::UnknownTerminal(terminal))
    }
}

impl PtyService for RealPtyService {
    fn spawn(
        &mut self,
        terminal: TerminalId,
        generation: TerminalGeneration,
        spec: TerminalSpec,
        size: TerminalSize,
        sink: PtyEventSink,
    ) -> PtyResult<()> {
        if self.live.contains_key(&terminal) {
            return Err(PtyError::AlreadyExists(terminal));
        }
        if spec.program.is_empty() {
            return Err(PtyError::backend("spawn", "program is empty"));
        }

        let pair = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|error| PtyError::backend("allocation", error))?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let mut child =
            pair.slave.spawn_command(command).map_err(|error| PtyError::backend("spawn", error))?;
        let process_id = child.process_id();
        #[cfg(unix)]
        let process_group = pair
            .master
            .process_group_leader()
            .or_else(|| process_id.and_then(|id| i32::try_from(id).ok()));
        #[cfg(unix)]
        let group_cleanup = process_group.map(|_| Arc::new(Mutex::new(false)));
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let process_exited = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(true));
        let released = Arc::new(AtomicBool::new(false));
        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                return Err(PtyError::backend("reader", error));
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                return Err(PtyError::backend("writer", error));
            }
        };
        drop(pair.slave);

        let (reader_start_tx, reader_start_rx) = mpsc::channel();
        let (reader_done_tx, reader_done_rx) = mpsc::channel();
        // Reader and waiter share one publication lock. The bounded application sink
        // may block; serializing here guarantees Exited can never overtake an Output
        // callback which already started.
        let serialized_sink = Arc::new(Mutex::new(sink.clone()));
        let reader_sink = serialized_sink.clone();
        let reader_active = active.clone();
        let reader_handle = match std::thread::Builder::new()
            .name(format!("termesh-pty-reader-{}", terminal.0))
            .spawn(move || {
                if reader_start_rx.recv().is_err() {
                    let _ = reader_done_tx.send(());
                    return;
                }
                let mut bytes = [0; OUTPUT_CHUNK];
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if let Ok(sink) = reader_sink.lock() {
                                if reader_active.load(Ordering::Acquire) {
                                    sink(PtyEvent::Output {
                                        terminal,
                                        generation,
                                        bytes: bytes[..read].to_vec(),
                                    });
                                }
                            }
                        }
                    }
                }
                let _ = reader_done_tx.send(());
            }) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = child.kill();
                return Err(PtyError::backend("reader thread", error));
            }
        };

        let (wait_start_tx, wait_start_rx) = mpsc::channel();
        let wait_sink = serialized_sink;
        let wait_process_exited = process_exited.clone();
        let wait_active = active.clone();
        let wait_released = released.clone();
        #[cfg(unix)]
        let wait_group_cleanup = group_cleanup.clone();
        let waiter_handle = match std::thread::Builder::new()
            .name(format!("termesh-pty-wait-{}", terminal.0))
            .spawn(move || {
                if wait_start_rx.recv().is_err() {
                    return;
                }
                // Only the `#[cfg(unix)]` process-group cleanup below reassigns this, so on
                // Windows the binding is never mutated and `unused_mut` fires — which is an
                // error under the workspace's `-D warnings`.
                #[cfg_attr(not(unix), allow(unused_mut))]
                let mut event = match child.wait() {
                    Ok(status) => {
                        let signal = status.signal().map(str::to_owned);
                        let code = signal.is_none().then_some(status.exit_code());
                        PtyEvent::Exited {
                            terminal,
                            generation,
                            exit: TerminalExit { code, signal },
                        }
                    }
                    Err(error) => PtyEvent::Failed {
                        terminal,
                        generation,
                        message: PtyError::backend("wait", error).to_string(),
                    },
                };
                wait_process_exited.store(true, Ordering::Release);
                #[cfg(unix)]
                if let (Some(group), Some(cleanup)) = (process_group, wait_group_cleanup.as_ref()) {
                    if let Err(error) = terminate_process_group(group, cleanup) {
                        event =
                            PtyEvent::Failed { terminal, generation, message: error.to_string() };
                    }
                }
                // PTY readers can lag process wait, but a background descendant may keep
                // the slave open forever. Drain the command's final bytes for a bounded
                // interval, then suppress any later callback before publishing Exited.
                let timed_out = reader_done_rx.recv_timeout(POST_EXIT_DRAIN_TIMEOUT).is_err();
                if timed_out {
                    wait_active.store(false, Ordering::Release);
                }
                // Taking the publication lock after disabling new output waits for the
                // one callback which may already have passed the flag check.
                if let Ok(sink) = wait_sink.lock() {
                    if !wait_released.load(Ordering::Acquire) {
                        sink(event);
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                if let Ok(mut killer) = killer.lock() {
                    let _ = killer.kill();
                }
                let _ = reader_start_tx.send(());
                let _ = reader_handle.join();
                return Err(PtyError::backend("wait thread", error));
            }
        };

        self.live.insert(
            terminal,
            LivePty {
                master: pair.master,
                writer,
                killer,
                process_exited,
                active,
                released,
                #[cfg(unix)]
                process_group,
                #[cfg(unix)]
                group_cleanup,
                reader: Some(reader_handle),
                waiter: Some(waiter_handle),
            },
        );
        sink(PtyEvent::Spawned { terminal, generation, process_id });
        let _ = reader_start_tx.send(());
        let _ = wait_start_tx.send(());
        Ok(())
    }

    fn write(&mut self, terminal: TerminalId, bytes: &[u8]) -> PtyResult<()> {
        let live = self.live_mut(terminal)?;
        live.writer.write_all(bytes).map_err(|error| PtyError::backend("write", error))?;
        live.writer.flush().map_err(|error| PtyError::backend("flush", error))
    }

    fn resize(&mut self, terminal: TerminalId, size: TerminalSize) -> PtyResult<()> {
        self.live(terminal)?
            .master
            .resize(pty_size(size))
            .map_err(|error| PtyError::backend("resize", error))
    }

    fn kill(&mut self, terminal: TerminalId) -> PtyResult<()> {
        let Some(live) = self.live.get(&terminal) else {
            return Ok(());
        };
        terminate(live)
    }

    fn release(&mut self, terminal: TerminalId) -> PtyResult<()> {
        let Some(mut live) = self.live.remove(&terminal) else {
            return Ok(());
        };
        // Suppress callbacks before terminating: release invalidates the live resource,
        // while the model deliberately retains the final screen/capture.
        live.released.store(true, Ordering::Release);
        live.active.store(false, Ordering::Release);
        let termination = terminate(&live);

        // Closing the master/writer wakes the reader. JoinHandles are intentionally
        // detached: a platform or driver bug must never hang the sole PTY worker or TUI
        // shutdown. `active` guarantees detached callbacks cannot mutate released state.
        drop(live.writer);
        drop(live.master);
        drop(live.killer);
        if let Some(reader) = live.reader.take() {
            drop(reader);
        }
        if let Some(waiter) = live.waiter.take() {
            drop(waiter);
        }
        termination
    }
}

#[cfg(unix)]
fn terminate(live: &LivePty) -> PtyResult<()> {
    let (Some(group), Some(cleanup)) = (live.process_group, live.group_cleanup.as_ref()) else {
        if live.process_exited.load(Ordering::Acquire) {
            return Ok(());
        }
        return live
            .killer
            .lock()
            .map_err(|_| PtyError::backend("kill", "killer lock poisoned"))?
            .kill()
            .map_err(|error| PtyError::backend("kill", error));
    };
    terminate_process_group(group, cleanup)
}

#[cfg(unix)]
fn terminate_process_group(group: i32, cleanup: &Arc<Mutex<bool>>) -> PtyResult<()> {
    use nix::errno::Errno;
    use nix::sys::signal::{kill, killpg, Signal};
    use nix::unistd::Pid;
    use std::time::Duration;

    let mut cleaned = cleanup
        .lock()
        .map_err(|_| PtyError::backend("kill", "process-group cleanup lock poisoned"))?;
    if *cleaned {
        return Ok(());
    }
    let group = Pid::from_raw(group);
    let hup = killpg(group, Signal::SIGHUP);
    if matches!(hup, Err(Errno::ESRCH)) {
        *cleaned = true;
        return Ok(());
    }
    for _ in 0..5 {
        match kill(Pid::from_raw(-group.as_raw()), None) {
            Ok(()) | Err(Errno::EPERM) => std::thread::sleep(Duration::from_millis(50)),
            Err(Errno::ESRCH) => {
                *cleaned = true;
                return Ok(());
            }
            Err(error) => {
                *cleaned = true;
                return Err(PtyError::backend("probe process group", error));
            }
        }
    }
    let result = match killpg(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(kill_error) => match hup {
            Err(hup_error) if hup_error != Errno::ESRCH => Err(PtyError::backend(
                "kill",
                format!("SIGHUP failed: {hup_error}; SIGKILL failed: {kill_error}"),
            )),
            _ => Err(PtyError::backend("kill", kill_error)),
        },
    };
    // This PGID is never signalled again. It either no longer exists, has received
    // SIGKILL, or produced a terminal error; retrying later could target a reused id.
    *cleaned = true;
    result
}

/// KNOWN GAP (0.1.0): unverified on a real Windows host.
///
/// In CI, `child.wait()` on a ConPTY child does not return promptly after the process
/// exits, so `process_exited` is still false here and `kill()` reports ERROR_TIMEOUT
/// (1460) — `release` then fails and the caller sees an error on closing a terminal. The
/// three real-PTY tests covering this are `#[ignore]`d on Windows rather than deleted.
///
/// Deliberately not "fixed" by swallowing the error: `release` failing is the symptom
/// reachable from CI, but `Exited` never arriving is the one that is not, and treating
/// teardown as best-effort here would hide the first while leaving the second. Whoever
/// picks this up wants a Windows machine and both symptoms in view.
#[cfg(windows)]
fn terminate(live: &LivePty) -> PtyResult<()> {
    if live.process_exited.load(Ordering::Acquire) {
        return Ok(());
    }
    live.killer
        .lock()
        .map_err(|_| PtyError::backend("kill", "killer lock poisoned"))?
        .kill()
        .map_err(|error| PtyError::backend("kill", error))
}

impl Drop for RealPtyService {
    fn drop(&mut self) {
        let terminals: Vec<_> = self.live.keys().copied().collect();
        for terminal in terminals {
            let _ = self.release(terminal);
        }
    }
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize { rows: size.rows.max(1), cols: size.cols.max(1), pixel_width: 0, pixel_height: 0 }
}
