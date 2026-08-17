//! Phase 01 — the TUI shell. Synchronous Elm-style loop (draw → read event → update).
//! Async (tokio) is intentionally deferred until PTYs/LSP/ACP need it in Phase 03/04.
#![forbid(unsafe_code)]

mod cli;
mod git_state;
mod input;
mod lsp_state;
mod model;
mod search_state;
mod task_state;
mod tui;
mod view;

use crossterm::event::{self, Event};
use model::Model;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termesh_agent::{AcpAgent, AgentService, ClientCapabilities, NullAgent};
use termesh_core::AppMessage;
use termesh_filesystem::{FileSystemService, FsWorker, RealFileSystem};
use termesh_git::{GitWorker, RealGitService};
use termesh_lsp::{LanguageService, LspSession, Recipe};
use termesh_platform::{ClipboardService, Osc52Clipboard};
use termesh_search::{RealSearch, SearchWorker};
use termesh_terminal::{PtyWorker, RealPtyService};
use termesh_workspace::{FilePermissionStore, FileSessionStore, PermissionStore, SessionStore};

/// Bounds asynchronous producer pressure while leaving enough room for short bursts of
/// input, filesystem, ACP, and 32-KiB PTY output messages.
const APP_MESSAGE_CAPACITY: usize = 256;

const METRIC_NAMES: [&str; 4] =
    ["event_loop_delay", "frame_duration", "turn_duration", "queue_depth"];

#[derive(Clone)]
struct Metrics {
    counts: Arc<Mutex<HashMap<&'static str, usize>>>,
    enabled: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            counts: Arc::new(Mutex::new(METRIC_NAMES.into_iter().map(|name| (name, 0)).collect())),
            enabled: true,
        }
    }
}

impl Metrics {
    fn new(enabled: bool) -> Self {
        Self { enabled, ..Self::default() }
    }

    fn known(&self, name: &str) -> bool {
        METRIC_NAMES.contains(&name)
    }

    fn span(&self, name: &'static str) -> MetricSpan {
        debug_assert!(self.known(name));
        MetricSpan {
            metrics: self.clone(),
            name,
            started: self.enabled.then(Instant::now),
            kind: None,
        }
    }

    fn turn(&self, kind: &'static str, started: Instant) -> MetricSpan {
        MetricSpan {
            metrics: self.clone(),
            name: "turn_duration",
            started: self.enabled.then_some(started),
            kind: Some(kind),
        }
    }

    fn gauge(&self, name: &'static str, value: usize) {
        debug_assert!(self.known(name));
        if !self.enabled {
            return;
        }
        tracing::trace!(metric = name, value, "metric");
    }

    fn memory(&self, subsystem: &'static str, bytes: usize) {
        if !self.enabled {
            return;
        }
        tracing::trace!(metric = "subsystem_memory", subsystem, bytes, "metric");
    }

    #[cfg(test)]
    fn count(&self, name: &str) -> usize {
        self.counts.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

struct MetricSpan {
    metrics: Metrics,
    name: &'static str,
    started: Option<Instant>,
    kind: Option<&'static str>,
}

struct LocalFileSubscriber {
    file: Mutex<std::fs::File>,
    next_span: AtomicU64,
}

impl LocalFileSubscriber {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file: Mutex::new(file), next_span: AtomicU64::new(1) })
    }

    fn write(&self, kind: &str, metadata: &tracing::Metadata<'_>, fields: &str) {
        let Ok(mut file) = self.file.lock() else { return };
        let _ = writeln!(
            file,
            "level={} kind={kind} target={} name={} {fields}",
            metadata.level(),
            metadata.target(),
            metadata.name()
        );
    }
}

#[derive(Default)]
struct TraceFields(String);

impl tracing::field::Visit for TraceFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(field.name());
        self.0.push('=');
        self.0.push_str(&format!("{value:?}"));
    }
}

impl tracing::Subscriber for LocalFileSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut fields = TraceFields::default();
        attributes.record(&mut fields);
        self.write("span", attributes.metadata(), &fields.0);
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = TraceFields::default();
        event.record(&mut fields);
        self.write("event", event.metadata(), &fields.0);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

fn install_trace(path: &Path) -> io::Result<()> {
    let subscriber = LocalFileSubscriber::open(path)?;
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| io::Error::other(format!("could not install trace subscriber: {error}")))
}

impl Drop for MetricSpan {
    fn drop(&mut self) {
        let Some(started) = self.started else { return };
        let duration = started.elapsed();
        if let Some(count) = self.metrics.counts.lock().unwrap().get_mut(self.name) {
            *count += 1;
        }
        let duration_micros = duration.as_micros();
        tracing::trace!(
            metric = self.name,
            kind = self.kind.unwrap_or(""),
            duration_micros,
            "metric"
        );
    }
}

#[derive(Clone)]
struct AppSender {
    inner: mpsc::SyncSender<AppMessage>,
    depth: Arc<AtomicUsize>,
}

impl AppSender {
    fn send(&self, message: AppMessage) -> bool {
        self.depth.fetch_add(1, Ordering::Relaxed);
        if self.inner.send(message).is_err() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    #[cfg(test)]
    fn try_send(&self, message: AppMessage) -> bool {
        self.depth.fetch_add(1, Ordering::Relaxed);
        if self.inner.try_send(message).is_err() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

struct AppReceiver {
    inner: mpsc::Receiver<AppMessage>,
    depth: Arc<AtomicUsize>,
}

impl AppReceiver {
    fn received(&self) {
        let _ = self.depth.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |depth| {
            Some(depth.saturating_sub(1))
        });
    }

    fn recv(&self) -> Result<AppMessage, mpsc::RecvError> {
        let result = self.inner.recv();
        if result.is_ok() {
            self.received();
        }
        result
    }

    fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<AppMessage, mpsc::RecvTimeoutError> {
        let result = self.inner.recv_timeout(timeout);
        if result.is_ok() {
            self.received();
        }
        result
    }

    fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }
}

fn app_message_channel() -> (AppSender, AppReceiver) {
    let (inner_tx, inner_rx) = mpsc::sync_channel(APP_MESSAGE_CAPACITY);
    let depth = Arc::new(AtomicUsize::new(0));
    (AppSender { inner: inner_tx, depth: depth.clone() }, AppReceiver { inner: inner_rx, depth })
}

fn apply_color_choice(
    model: &mut Model,
    choice: cli::ColorChoice,
    detected: termesh_platform::ColorDepth,
) {
    model.theme = termesh_ui::Theme::for_depth(choice.resolve(detected));
}

fn main() -> io::Result<()> {
    let args = cli::Cli::parse(std::env::args().skip(1));
    if let Some(path) = &args.trace {
        install_trace(path)?;
        tracing::trace!(path = %path.display(), "local tracing enabled");
    }
    let detected_color = termesh_platform::current_color_depth();

    match &args.mode {
        cli::Mode::Help => {
            print!("{}", cli::HELP);
            Ok(())
        }
        cli::Mode::Version => {
            println!("{}", cli::version_line());
            Ok(())
        }
        // CI/smoke: construct nothing that needs a real TTY.
        cli::Mode::ProbeOnly => Ok(()),
        cli::Mode::DumpFrame {
            palette,
            open,
            agent_demo,
            terminal_demo,
            search_task_demo,
            git_demo,
            lsp_demo,
            polyglot_demo,
            java_demo,
        } => {
            // Headless: render one frame to memory and print it. No TTY required.
            // The workspace loads synchronously here so the frame is complete and
            // deterministic rather than catching the tree mid-load.
            let fs = RealFileSystem::new();
            let mut model = Model::new();
            apply_color_choice(&mut model, args.color, detected_color);
            if !git_demo && !lsp_demo && !polyglot_demo && !java_demo {
                if let Some(path) = &args.path {
                    model.open_workspace_sync(&fs, path);
                }
            }
            if let Some(file) = open {
                model.open_file_sync(&fs, file.clone());
            }
            if *agent_demo {
                run_agent_demo(&mut model);
            }
            if *terminal_demo {
                run_terminal_demo(&mut model);
            }
            if *search_task_demo {
                run_search_task_demo(&mut model);
            }
            if *git_demo {
                run_git_demo(&mut model);
            }
            if *lsp_demo {
                run_lsp_demo(&mut model);
            }
            if *polyglot_demo {
                run_polyglot_demo(&mut model);
            }
            if *java_demo {
                run_java_demo(&mut model);
            }
            if let Some(query) = palette {
                model.dispatch(termesh_core::Command::OpenPalette);
                for c in query.chars() {
                    input::on_chord(
                        &mut model,
                        termesh_core::input::KeyChord::plain(termesh_core::input::Key::Char(c)),
                    );
                }
            }
            print!("{}", view::snapshot(&mut model, 96, 28));
            Ok(())
        }
        cli::Mode::Run => {
            let mut tui = tui::Tui::new()?;
            tui.enter()?;
            let result = run(
                &mut tui,
                args.path.as_deref(),
                args.color,
                detected_color,
                args.trace.is_some(),
            );
            tui.exit()?;
            result
        }
    }
}

fn run(
    tui: &mut tui::Tui,
    path: Option<&Path>,
    color: cli::ColorChoice,
    detected_color: termesh_platform::ColorDepth,
    trace_enabled: bool,
) -> io::Result<()> {
    let metrics = Metrics::new(trace_enabled);
    let mut model = Model::new();
    apply_color_choice(&mut model, color, detected_color);

    // A missing config.toml/keymap.toml is the normal case (ADR-0014 §3): nothing to
    // load, nothing to say. Only a file that exists and fails to parse reports anything.
    if let Some(config_path) = termesh_platform::config_file() {
        let fs = RealFileSystem::new();
        match fs.read_file(&config_path) {
            Ok(bytes) => model.apply_settings_bytes(bytes, &config_path),
            Err(termesh_core::FsError::NotFound(_)) => {}
            Err(error) => model.apply_settings_read_error(&config_path, error),
        }
    }
    if let Some(keymap_path) = termesh_platform::keymap_file() {
        let fs = RealFileSystem::new();
        match fs.read_file(&keymap_path) {
            Ok(bytes) => model.apply_keymap_bytes(bytes, &keymap_path),
            Err(termesh_core::FsError::NotFound(_)) => {}
            Err(error) => model.apply_keymap_read_error(&keymap_path, error),
        }
    }

    // One channel, many producers (ADR-0005 §1). The input pump and the filesystem
    // worker both feed it, which is the whole reason the loop no longer blocks on
    // `event::read()` directly.
    let (tx, rx) = app_message_channel();
    spawn_input_pump(tx.clone());
    let tx_agent = tx.clone();
    let tx_fs = tx.clone();
    let tx_git = tx.clone();
    let tx_pty = tx.clone();
    let tx_search = tx.clone();
    let tx_lsp = tx.clone();

    let worker = FsWorker::spawn(RealFileSystem::new(), model.ignore_options, move |event| {
        let _ = tx_fs.send(AppMessage::Fs(event));
    });
    let git_worker = GitWorker::spawn(RealGitService::new(), move |event| {
        let _ = tx_git.send(AppMessage::Git(event));
    });
    let pty_worker = PtyWorker::spawn(RealPtyService::new(), move |event| {
        let _ = tx_pty.send(AppMessage::Pty(event));
    });
    let search_worker = SearchWorker::spawn(RealSearch::default(), move |event| {
        let _ = tx_search.send(AppMessage::Search(event));
    });
    let mut clipboard = Osc52Clipboard::new(io::stdout());
    drop(tx);

    // A path on the command line wins; otherwise reopen whatever was open last.
    let fs = RealFileSystem::new();
    let store = termesh_platform::session_file().map(|p| FileSessionStore::new(&fs, p));
    let (mut session, session_diagnostics) =
        store.as_ref().map(|s| s.load_with_diagnostics()).unwrap_or_default();
    if !session_diagnostics.is_empty() {
        model.notification = Some(
            session_diagnostics
                .iter()
                .map(|diagnostic| {
                    format!("session.toml: {} ({})", diagnostic.problem, diagnostic.fallback)
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    // Drop workspaces that have been deleted since we recorded them, so a stale entry
    // neither gets reopened nor lingers in the list forever.
    session.prune_missing(&fs);
    model.set_prior_session_present(session.workspace.is_some() || !session.recent.is_empty());

    let explicit_path = path.map(PathBuf::from);
    let to_open = explicit_path.clone().or_else(|| {
        session
            .workspace
            .as_ref()
            .map(|workspace| workspace.root.clone())
            .or_else(|| session.last_root().map(Path::to_path_buf))
    });
    let mut agent_cwd = path.unwrap_or(Path::new(".")).to_path_buf();

    if let Some(path_to_open) = to_open {
        let root = termesh_workspace::detect_root(&fs, &path_to_open);
        agent_cwd = root.path.clone();

        match FilePermissionStore::new(&fs).load(&root.path) {
            Ok(policy) => model.set_permission_policy(policy),
            Err(error) => {
                model.notification = Some(format!("could not load command permissions: {error}"));
            }
        }

        let restoring = explicit_path.is_none()
            && session.workspace.as_ref().is_some_and(|workspace| workspace.root == root.path);
        if restoring {
            model.restore_session(&fs, &session);
        } else {
            model.open_workspace_configured(&fs, root);
        }

        // An explicit CLI path is a workspace change. Automatic restoration is only a
        // read and must not rewrite the file as a side effect of startup (ADR-0014 §2).
        if explicit_path.is_some() {
            model.persist_session(&mut session);
            if let Some(store) = &store {
                if let Err(error) = store.save(&session) {
                    model.notification = Some(format!("could not save session: {error}"));
                }
            }
        }
    }
    model.restore_drafts(&fs);

    // Tier 0 unless an ACP agent is configured (ADR-0003): the editor works with none,
    // and no vendor is assumed. A configured agent is spawned behind the same trait.
    // Terminal support is advertised only here, after the PTY worker and permission
    // policy have both been installed.
    let mut agent = start_agent(&fs, &agent_cwd, tx_agent, &mut model);
    model.start_fresh_agent_after_restore();
    let mut lsp_sessions: HashMap<termesh_core::LspServerId, LspSession> = HashMap::new();
    let mut agent_turns: HashMap<termesh_core::SessionId, Instant> = HashMap::new();
    let mut task_turns: HashMap<termesh_core::TerminalId, Instant> = HashMap::new();

    while model.running {
        model.queue_due_drafts(std::time::Instant::now());
        // The model queues filesystem work rather than performing it; forward whatever
        // the last update produced before we go back to sleep.
        for request in model.take_fs_requests() {
            worker.request(request);
        }
        for request in model.take_search_requests() {
            search_worker.request(request);
        }
        for request in model.take_git_requests() {
            git_worker.request(request);
        }
        if model.take_search_cancel() {
            search_worker.cancel();
        }
        for request in model.take_agent_requests() {
            if let termesh_core::AgentRequest::Prompt { session, .. } = &request {
                agent_turns.insert(*session, Instant::now());
            }
            agent.send(request);
        }
        for request in model.take_pty_requests() {
            pty_worker.request(request);
        }
        for (routed_server, request) in model.take_lsp_requests() {
            match request {
                termesh_core::LspRequest::Start {
                    server,
                    root,
                    command,
                    language,
                    initialization_options,
                } => {
                    debug_assert_eq!(server, routed_server);
                    if let Some(mut previous) = lsp_sessions.remove(&server) {
                        previous.send(termesh_core::LspRequest::Shutdown);
                    }
                    let recipe = Recipe {
                        language_id: language,
                        command: command.clone(),
                        extensions: Vec::new(),
                        initialization_options,
                    };
                    let events = tx_lsp.clone();
                    match LspSession::spawn(&recipe, &root, move |event| {
                        let _ = events.send(AppMessage::Lsp(server, event));
                    }) {
                        Ok(session) => {
                            lsp_sessions.insert(server, session);
                        }
                        Err(error) => {
                            let failure = if error.kind() == io::ErrorKind::NotFound {
                                command
                                    .first()
                                    .map(|program| termesh_lsp::missing_server(program))
                                    .unwrap_or(termesh_core::LspFailure {
                                        kind: termesh_core::LspFailureKind::NotInstalled,
                                        message: "no language-server program configured".into(),
                                    })
                            } else {
                                termesh_core::LspFailure {
                                    kind: termesh_core::LspFailureKind::Transport,
                                    message: format!("could not start language server: {error}"),
                                }
                            };
                            model.on_lsp_event(
                                server,
                                termesh_core::LspEvent::Failed { id: None, failure },
                            );
                        }
                    }
                }
                termesh_core::LspRequest::Shutdown => {
                    if let Some(mut session) = lsp_sessions.remove(&routed_server) {
                        session.send(termesh_core::LspRequest::Shutdown);
                    }
                }
                request => {
                    if let Some(session) = lsp_sessions.get_mut(&routed_server) {
                        session.send(request);
                    }
                }
            }
        }
        for text in model.take_clipboard_text() {
            if let Err(error) = clipboard.set_text(&text) {
                model.notification = Some(error.to_string());
            }
        }
        // In-process agents (the Tier 0 null agent, and the scripted one in tests) answer
        // immediately; the real ACP client will feed the channel from its worker thread
        // instead, which is why both routes end at `on_agent_event`.
        for event in agent.poll() {
            model.on_agent_event(event);
        }

        for run in &model.task_runs {
            if matches!(
                run.status,
                termesh_core::TaskStatus::Starting | termesh_core::TaskStatus::Running
            ) {
                task_turns.entry(run.terminal).or_insert_with(Instant::now);
            }
        }

        // The editor needs to know its own height so cursor commands can scroll; compute
        // it here rather than during render, which stays a pure function of the model.
        {
            let _frame = metrics.span("frame_duration");
            let size = tui.terminal.size()?;
            model.set_editor_height(view::editor_rows(size.width, size.height, &model));
            model.set_terminal_size(view::terminal_size(size.width, size.height, &model));
            model.agent_scroll_max = view::agent_scrollback(size.width, size.height, &model);
            tui.terminal.draw(|f| view::render(f, &model))?;
        }
        metrics.gauge("queue_depth", rx.depth());
        sample_subsystem_memory(&metrics, &model);

        // Sleep until a producer has work or the crash-draft debounce expires. With no
        // dirty buffer there is no timer, so an idle editor still costs nothing.
        let message = {
            let _delay = metrics.span("event_loop_delay");
            if let Some(deadline) = model.next_draft_deadline() {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(message) => Some(message),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                }
            } else {
                rx.recv().ok()
            }
        };
        let Some(message) = message else { break };
        match &message {
            AppMessage::Agent(
                termesh_core::AgentEvent::TurnEnded { session, .. }
                | termesh_core::AgentEvent::Failed { session, .. },
            ) => {
                if let Some(started) = agent_turns.remove(session) {
                    drop(metrics.turn("agent", started));
                }
            }
            AppMessage::Pty(
                termesh_core::PtyEvent::Exited { terminal, .. }
                | termesh_core::PtyEvent::Failed { terminal, .. },
            ) => {
                if let Some(started) = task_turns.remove(terminal) {
                    drop(metrics.turn("task", started));
                }
            }
            _ => {}
        }
        match message {
            AppMessage::Input(chord) => input::on_chord(&mut model, chord),
            AppMessage::Resize => {
                // Queue PTY resizes immediately; waiting until the next draw would leave
                // them stranded while the loop blocks for another input event.
                let size = tui.terminal.size()?;
                model.set_editor_height(view::editor_rows(size.width, size.height, &model));
                model.set_terminal_size(view::terminal_size(size.width, size.height, &model));
                model.agent_scroll_max = view::agent_scrollback(size.width, size.height, &model);
            }
            AppMessage::Fs(event) => model.on_fs_event(event),
            AppMessage::Search(event) => model.on_search_event(event),
            AppMessage::Git(event) => model.on_git_event(event),
            AppMessage::Lsp(server, event) => model.on_lsp_event(server, event),
            AppMessage::Pty(event) => model.on_pty_event(event),
            AppMessage::Agent(event) => model.on_agent_event(event),
        }
    }
    // A clean exit is the durable checkpoint. Editing never rewrites session.toml.
    model.persist_session(&mut session);
    if let Some(store) = &store {
        if let Err(error) = store.save(&session) {
            model.notification = Some(format!("could not save session: {error}"));
        }
    }
    model.shutdown_terminals();
    for request in model.take_agent_requests() {
        agent.send(request);
    }
    for request in model.take_pty_requests() {
        pty_worker.request(request);
    }
    for session in lsp_sessions.values_mut() {
        session.send(termesh_core::LspRequest::Shutdown);
    }
    if model.permission_policy().is_dirty() {
        if let Some(root) = model.explorer.as_ref().map(|explorer| explorer.root.path.clone()) {
            if let Err(error) = FilePermissionStore::new(&fs).save(&root, model.permission_policy())
            {
                model.notification = Some(format!("could not save command permissions: {error}"));
            }
        }
    }
    // Wake any bounded producer before its owning worker is dropped and joined.
    drop(rx);
    Ok(())
}

fn sample_subsystem_memory(metrics: &Metrics, model: &Model) {
    if !metrics.enabled {
        return;
    }
    let buffer_bytes = model.buffers.iter().map(|buffer| buffer.text().len_bytes()).sum();
    let agent_bytes = model
        .agent
        .as_ref()
        .into_iter()
        .flat_map(|session| &session.transcript)
        .chain(model.restored_agent_history.iter())
        .map(|line| line.text.len())
        .sum();
    let terminal_bytes =
        model.terminals.iter().map(|terminal| terminal.capture.as_str().len()).sum();
    metrics.memory("buffers", buffer_bytes);
    metrics.memory("agent_transcript", agent_bytes);
    metrics.memory("terminal_capture", terminal_bytes);
}

/// Run one canned agent turn against the open buffer, for `--dump-frame --agent-demo`.
///
/// The headless proof that the phase's whole point renders: a proposal arrives, becomes
/// hunks, and marks the gutter — checkable in CI without an agent installed, on any
/// machine, with no network. Uses the same `ScriptedAgent` the tests do and the same
/// `on_agent_event` the real client feeds, so a frame that looks right means the real
/// path is right.
fn run_agent_demo(model: &mut Model) {
    use termesh_test_support::{ScriptedAgent, ScriptedUpdate};

    let Some(buffer) = model.active_buffer() else {
        model.notification = Some("--agent-demo needs --open FILE".into());
        return;
    };
    let path = buffer.path().map(Path::to_path_buf).unwrap_or_default();
    let original = buffer.text().to_string();

    // A change anyone can eyeball: rewrite the first line.
    let mut proposed = original.clone();
    if let Some(end) = proposed.find('\n') {
        proposed.replace_range(..end, "// the agent proposed this line");
    }

    let mut agent = ScriptedAgent::new().with_turn(vec![
        ScriptedUpdate::Message("Proposing one change.".into()),
        ScriptedUpdate::ReadFile(path.clone()),
        ScriptedUpdate::Edit { path, old_text: Some(original), new_text: proposed },
        ScriptedUpdate::End,
    ]);

    model.agent_name = Some("demo".into());
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::AgentSessionNew));
    settle_agent(model, &mut agent);
    if let Some(session) = model.agent.as_ref().map(|a| a.id) {
        model.confirm_prompt(crate::model::Prompt {
            title: String::new(),
            input: "improve this file".into(),
            kind: crate::model::PromptKind::AgentPrompt { session },
        });
    }
    settle_agent(model, &mut agent);
}

/// Inject one recorded command run for `--dump-frame --terminal-demo`.
///
/// This follows the same model/PTy-event path as the real worker while deliberately
/// avoiding a process, shell, TTY, timing, and platform dependency.
fn run_terminal_demo(model: &mut Model) {
    model.confirm_prompt(crate::model::Prompt {
        title: String::new(),
        input: "cargo test".into(),
        kind: crate::model::PromptKind::TerminalRun,
    });
    let requests = model.take_pty_requests();
    let Some((terminal, generation)) = requests.iter().find_map(|request| match request {
        termesh_core::PtyRequest::Spawn { terminal, generation, .. } => {
            Some((*terminal, *generation))
        }
        _ => None,
    }) else {
        model.notification = Some("terminal demo could not create its session".into());
        return;
    };
    model.on_pty_event(termesh_core::PtyEvent::Spawned {
        terminal,
        generation,
        process_id: Some(4242),
    });
    model.on_pty_event(termesh_core::PtyEvent::Output {
        terminal,
        generation,
        bytes: b"\x1b[1;36m$ cargo test\x1b[0m\r\n\x1b[32mtest result: ok\x1b[0m. 154 passed; 0 failed\r\n"
            .to_vec(),
    });
    model.on_pty_event(termesh_core::PtyEvent::Exited {
        terminal,
        generation,
        exit: termesh_core::TerminalExit { code: Some(0), signal: None },
    });
}

/// Drive the real Phase-05 task picker, decoder, run state, and Problems overlay with
/// recorded events. The headless frame stays independent of Cargo, ripgrep, a PTY, and ACP.
fn run_search_task_demo(model: &mut Model) {
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::TaskRun));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Down));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Down));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Enter));

    let requests = model.take_pty_requests();
    let Some((terminal, generation)) = requests.iter().find_map(|request| match request {
        termesh_core::PtyRequest::Spawn { terminal, generation, spec, .. }
            if spec.program == "cargo" && spec.args.first().map(String::as_str) == Some("test") =>
        {
            Some((*terminal, *generation))
        }
        _ => None,
    }) else {
        model.notification = Some("search/task demo needs a detected Rust workspace".into());
        return;
    };

    model.on_pty_event(termesh_core::PtyEvent::Spawned {
        terminal,
        generation,
        process_id: Some(505),
    });
    model.on_pty_event(termesh_core::PtyEvent::Output {
        terminal,
        generation,
        bytes: br#"{"reason":"compiler-message","message":{"rendered":"error[E0425]: cannot find value `missing` in this scope\n  --> src/lib.rs:12:5\n","level":"error","message":"cannot find value `missing` in this scope","spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true}]}}
"#
        .to_vec(),
    });
    model.on_pty_event(termesh_core::PtyEvent::Exited {
        terminal,
        generation,
        exit: termesh_core::TerminalExit { code: Some(101), signal: None },
    });
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::ProblemsShow));
}

/// Render Phase-06 Git state through the production model/event/input path with no OS work.
fn run_git_demo(model: &mut Model) {
    model.open_workspace(termesh_workspace::WorkspaceRoot {
        path: PathBuf::from("/termesh-git-demo"),
        kind: termesh_workspace::ProjectKind::Rust,
        kinds: vec![termesh_workspace::ProjectKind::Rust],
        detected: true,
    });
    let Some(refresh_id) =
        model.take_git_requests().into_iter().find_map(|request| match request {
            termesh_core::GitRequest::Refresh { id, .. } => Some(id),
            _ => None,
        })
    else {
        model.notification = Some("Git demo could not queue its snapshot".into());
        return;
    };
    model.on_git_event(termesh_core::GitEvent::SnapshotLoaded {
        id: refresh_id,
        snapshot: termesh_core::GitRepositorySnapshot {
            repository_root: PathBuf::from("/termesh-git-demo"),
            workspace_root: PathBuf::from("/termesh-git-demo"),
            branch: termesh_core::GitBranchStatus {
                oid: Some("abc123456789".into()),
                head: Some("main".into()),
                upstream: Some("origin/main".into()),
                ahead: 2,
                behind: 1,
                detached: false,
            },
            files: vec![
                termesh_core::GitFileStatus {
                    path: "conflict.rs".into(),
                    index: Some(termesh_core::GitChangeKind::Conflicted),
                    worktree: Some(termesh_core::GitChangeKind::Conflicted),
                },
                termesh_core::GitFileStatus {
                    path: "src/staged.rs".into(),
                    index: Some(termesh_core::GitChangeKind::Modified),
                    worktree: None,
                },
                termesh_core::GitFileStatus {
                    path: "src/worktree.rs".into(),
                    index: None,
                    worktree: Some(termesh_core::GitChangeKind::Modified),
                },
            ],
            context_diff: termesh_core::GitContextDiff {
                index: "@@ -1 +1 @@\n-old staged\n+new staged\n".into(),
                worktree: "@@ -1 +1 @@\n-old worktree\n+new worktree\n".into(),
                index_truncated: false,
                worktree_truncated: false,
            },
        },
    });
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::GitShow));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Enter));
    let Some((id, path, target)) =
        model.take_git_requests().into_iter().find_map(|request| match request {
            termesh_core::GitRequest::Diff { id, path, target, .. } => Some((id, path, target)),
            _ => None,
        })
    else {
        model.notification = Some("Git demo could not queue its diff".into());
        return;
    };
    model.on_git_event(termesh_core::GitEvent::DiffLoaded {
        id,
        diff: termesh_core::GitFileDiff {
            path,
            target,
            text: "@@ -1,2 +1,2 @@\n-fn conflict() { old(); }\n+fn conflict() { resolved(); }\n"
                .into(),
            truncated: false,
        },
    });
    // Land on the status surface so one frame shows conflicts plus staged/unstaged paths;
    // the injected diff was still loaded through the real correlation path and is one Enter away.
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Esc));
}

/// Render Phase-07 language intelligence through production model/event/input paths
/// while deliberately avoiding an OS process or installed language server.
fn run_lsp_demo(model: &mut Model) {
    use termesh_core::{
        Diagnostic, DiagnosticOrigin, DiagnosticSeverity, HoverText, LspEvent, LspRequest,
        TextPosition, TextRange,
    };
    use termesh_test_support::FakeFileSystem;

    let fs = FakeFileSystem::with_paths(&[
        "/termesh-lsp-demo/Cargo.toml",
        "/termesh-lsp-demo/src/main.rs",
    ]);
    fs.add_file("/termesh-lsp-demo/src/main.rs", b"fn main() -> i32 { \"wrong\" }\n");
    model.open_workspace_sync(&fs, Path::new("/termesh-lsp-demo"));
    model.open_file_sync(&fs, PathBuf::from("/termesh-lsp-demo/src/main.rs"));

    let requests = model.take_lsp_requests();
    let Some(server) = requests.iter().find_map(|(_, request)| match request {
        LspRequest::Start { server, .. } => Some(*server),
        _ => None,
    }) else {
        model.notification = Some("Language demo could not create its synthetic session".into());
        return;
    };
    let Some(version) = requests.iter().find_map(|(_, request)| match request {
        LspRequest::DidOpen { path, version, .. }
            if path == Path::new("/termesh-lsp-demo/src/main.rs") =>
        {
            Some(*version)
        }
        _ => None,
    }) else {
        model.notification = Some("Language demo could not open its synthetic document".into());
        return;
    };

    model.on_lsp_event(server, LspEvent::Ready);
    model.on_lsp_event(
        server,
        LspEvent::Diagnostics {
            path: PathBuf::from("/termesh-lsp-demo/src/main.rs"),
            version: Some(version),
            items: vec![Diagnostic {
                path: PathBuf::from("/termesh-lsp-demo/src/main.rs"),
                range: TextRange {
                    start: TextPosition { line: 0, character: 19 },
                    end: TextPosition { line: 0, character: 26 },
                },
                severity: DiagnosticSeverity::Error,
                origin: DiagnosticOrigin::LanguageServer,
                source: "rust-analyzer".into(),
                code: Some("E0308".into()),
                message: "mismatched types".into(),
            }],
        },
    );

    for _ in 0..19 {
        input::on_chord(
            model,
            termesh_core::input::KeyChord::plain(termesh_core::input::Key::Right),
        );
    }
    input::on_chord(model, termesh_core::input::KeyChord::alt(termesh_core::input::Key::Char('k')));
    let Some(id) = model.take_lsp_requests().into_iter().find_map(|(_, request)| match request {
        LspRequest::Hover { id, .. } => Some(id),
        _ => None,
    }) else {
        model.notification = Some("Language demo could not request hover information".into());
        return;
    };
    model.on_lsp_event(
        server,
        LspEvent::Hover {
            id,
            hover: Some(HoverText {
                text: "mismatched types\nexpected i32, found &str".into(),
                range: None,
                truncated: false,
            }),
        },
    );
}

/// Render Phase-08's polyglot workspace through real detection, lazy routing, task
/// discovery, and correlated language events without starting a process.
fn run_polyglot_demo(model: &mut Model) {
    use termesh_core::{HoverText, LspEvent, LspRequest};
    use termesh_test_support::FakeFileSystem;

    fn opened_session(
        requests: &[(termesh_core::LspServerId, LspRequest)],
        path: &Path,
    ) -> Option<(termesh_core::LspServerId, u64)> {
        let (server, version) = requests.iter().find_map(|(server, request)| match request {
            LspRequest::DidOpen { path: opened, version, .. } if opened == path => {
                Some((*server, *version))
            }
            _ => None,
        })?;
        requests.iter().any(|(_, request)| {
            matches!(request, LspRequest::Start { server: started, .. } if *started == server)
        }).then_some((server, version))
    }

    fn answer_hover(model: &mut Model, text: &str) -> bool {
        input::on_chord(
            model,
            termesh_core::input::KeyChord::alt(termesh_core::input::Key::Char('k')),
        );
        let Some((server, id)) =
            model.take_lsp_requests().into_iter().find_map(|(server, request)| match request {
                LspRequest::Hover { id, .. } => Some((server, id)),
                _ => None,
            })
        else {
            return false;
        };
        model.on_lsp_event(
            server,
            LspEvent::Hover {
                id,
                hover: Some(HoverText { text: text.into(), range: None, truncated: false }),
            },
        );
        input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Esc));
        true
    }

    let fs = FakeFileSystem::with_paths(&[
        "/termesh-polyglot-demo/Cargo.toml",
        "/termesh-polyglot-demo/package.json",
        "/termesh-polyglot-demo/src/main.rs",
        "/termesh-polyglot-demo/web/app.ts",
    ]);
    fs.add_file("/termesh-polyglot-demo/src/main.rs", b"fn main() { println!(\"polyglot\"); }\n");
    fs.add_file("/termesh-polyglot-demo/web/app.ts", b"export const greeting: string = 'hello';\n");
    fs.add_file(
        "/termesh-polyglot-demo/package.json",
        br#"{"scripts":{"build":"tsc","test":"vitest"}}"#,
    );
    model.open_workspace_sync(&fs, Path::new("/termesh-polyglot-demo"));

    let rust_path = PathBuf::from("/termesh-polyglot-demo/src/main.rs");
    model.open_file_sync(&fs, rust_path.clone());
    let Some((rust_server, _)) = opened_session(&model.take_lsp_requests(), &rust_path) else {
        model.notification = Some("Polyglot demo could not route its Rust document".into());
        return;
    };
    model.on_lsp_event(rust_server, LspEvent::Ready);
    if !answer_hover(model, "rust: fn main()") {
        model.notification = Some("Polyglot demo could not correlate its Rust hover".into());
        return;
    }

    let node_path = PathBuf::from("/termesh-polyglot-demo/web/app.ts");
    model.open_file_sync(&fs, node_path.clone());
    let Some((node_server, _)) = opened_session(&model.take_lsp_requests(), &node_path) else {
        model.notification = Some("Polyglot demo could not route its TypeScript document".into());
        return;
    };
    model.on_lsp_event(node_server, LspEvent::Ready);
    if !answer_hover(model, "typescript: const greeting: string") {
        model.notification = Some("Polyglot demo could not correlate its TypeScript hover".into());
        return;
    }

    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::F(5)));
}

/// Render Phase-09 Java support through real detection, lazy routing, task discovery,
/// problem decoding, and correlated language events without starting a process.
fn run_java_demo(model: &mut Model) {
    use termesh_core::{
        Diagnostic, DiagnosticOrigin, DiagnosticSeverity, LspEvent, LspRequest, TextPosition,
        TextRange,
    };
    use termesh_test_support::FakeFileSystem;

    let root = Path::new("/termesh-java-demo");
    let java_path = PathBuf::from("/termesh-java-demo/src/main/java/com/example/App.java");
    let fs = FakeFileSystem::with_paths(&[
        "/termesh-java-demo/pom.xml",
        "/termesh-java-demo/mvnw",
        "/termesh-java-demo/src/main/java/com/example/App.java",
    ]);
    fs.add_file(
        "/termesh-java-demo/pom.xml",
        b"<project><modelVersion>4.0.0</modelVersion></project>\n",
    );
    fs.add_file("/termesh-java-demo/mvnw", b"#!/bin/sh\n");
    fs.add_file(
        &java_path,
        b"package com.example;\n// cannot find symbol: MissingType\nclass App { MissingType value; }\n",
    );

    model.open_workspace_sync(&fs, root);
    model.open_file_sync(&fs, java_path.clone());
    let requests = model.take_lsp_requests();
    let Some((server, version)) = requests.iter().find_map(|(server, request)| match request {
        LspRequest::DidOpen { path, version, .. } if path == &java_path => {
            Some((*server, *version))
        }
        _ => None,
    }) else {
        model.notification = Some("Java demo could not route App.java to JDT LS".into());
        return;
    };
    if !requests.iter().any(|(_, request)| {
        matches!(request, LspRequest::Start { server: started, .. } if *started == server)
    }) {
        model.notification = Some("Java demo could not create its synthetic JDT LS session".into());
        return;
    }

    model.on_lsp_event(server, LspEvent::Started);
    // This is the model event produced by JDT LS's vendor `language/status`
    // notification; keeping it active makes the import path visible in the frame.
    model.on_lsp_event(
        server,
        LspEvent::Indexing { message: "Importing Maven project".into(), percent: Some(48) },
    );
    model.on_lsp_event(
        server,
        LspEvent::Diagnostics {
            path: java_path.clone(),
            version: Some(version),
            items: vec![Diagnostic {
                path: java_path.clone(),
                range: TextRange {
                    start: TextPosition { line: 2, character: 12 },
                    end: TextPosition { line: 2, character: 23 },
                },
                severity: DiagnosticSeverity::Error,
                origin: DiagnosticOrigin::LanguageServer,
                source: "jdtls".into(),
                code: Some("Java(16777218)".into()),
                message: "MissingType cannot be resolved to a type; cannot find symbol".into(),
            }],
        },
    );

    // Select the conventional Maven test goal and replay one javac failure through the
    // production task decoder. The queued spawn is consumed here; no worker sees it.
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::TaskRun));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Down));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Down));
    input::on_chord(model, termesh_core::input::KeyChord::plain(termesh_core::input::Key::Enter));
    let Some((terminal, generation)) =
        model.take_pty_requests().into_iter().find_map(|request| match request {
            termesh_core::PtyRequest::Spawn { terminal, generation, spec, .. }
                if spec.args == ["test"] =>
            {
                Some((terminal, generation))
            }
            _ => None,
        })
    else {
        model.notification = Some("Java demo could not select its synthetic Maven task".into());
        return;
    };
    model.on_pty_event(termesh_core::PtyEvent::Spawned {
        terminal,
        generation,
        process_id: Some(909),
    });
    model.on_pty_event(termesh_core::PtyEvent::Output {
        terminal,
        generation,
        bytes: b"[ERROR] src/main/java/com/example/App.java:3: error: cannot find symbol\n"
            .to_vec(),
    });
    model.on_pty_event(termesh_core::PtyEvent::Exited {
        terminal,
        generation,
        exit: termesh_core::TerminalExit { code: Some(1), signal: None },
    });

    // Finish on the catalog: the editor and terminal remain visible behind it, while
    // every conventional Maven goal is rendered in the foreground.
    model.dispatch(termesh_core::Command::Action(termesh_core::Action::TaskRun));
}

/// Pump model and agent until neither has anything left to say.
fn settle_agent(model: &mut Model, agent: &mut termesh_test_support::ScriptedAgent) {
    use termesh_agent::AgentService;
    for _ in 0..32 {
        let requests = model.take_agent_requests();
        let events = agent.poll();
        if requests.is_empty() && events.is_empty() {
            return;
        }
        for request in requests {
            agent.send(request);
        }
        for event in events {
            model.on_agent_event(event);
        }
    }
}

/// Start the configured ACP agent, or fall back to Tier 0.
///
/// Every failure here is a degradation, never a refusal to start: an editor that will not
/// open because an agent is missing has its priorities backwards. The reason is surfaced
/// in the status bar (ARCHITECTURE.md §13 — config errors appear inside the app, with the
/// fallback taken).
fn start_agent(
    fs: &RealFileSystem,
    cwd: &Path,
    tx: AppSender,
    model: &mut Model,
) -> Box<dyn AgentService> {
    let sink = move |event| {
        let _ = tx.send(AppMessage::Agent(event));
    };

    let Some(path) = termesh_platform::agents_file() else { return Box::new(NullAgent) };
    let Ok(bytes) = fs.read_file(&path) else { return Box::new(NullAgent) };

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            model.notification = Some("agents.toml is not valid UTF-8".into());
            return Box::new(NullAgent);
        }
    };

    let config = match termesh_config::AgentsConfig::parse(&text) {
        Ok(config) => config,
        Err(e) => {
            model.notification = Some(e.to_string());
            return Box::new(NullAgent);
        }
    };
    let Some((name, agent)) = config.selected() else { return Box::new(NullAgent) };

    let capabilities = ClientCapabilities { terminal: true, ..ClientCapabilities::default() };
    match AcpAgent::spawn(&agent.command, cwd, capabilities, sink) {
        Ok(acp) => {
            model.agent_name = Some(name.to_string());
            if model.notification.is_none() {
                model.notification =
                    Some(format!("agent: {name} \u{2014} Alt+I to ask it something"));
            }
            Box::new(acp)
        }
        Err(e) => {
            model.notification = Some(format!("could not start agent '{name}': {e}"));
            Box::new(NullAgent)
        }
    }
}

/// Reads terminal events off the render loop and forwards them as [`AppMessage`]s.
///
/// The thread is detached and dies with the process: `event::read()` blocks with no
/// portable way to interrupt it, so we let the channel disconnect end it whenever the
/// receiver goes away first.
fn spawn_input_pump(tx: AppSender) {
    std::thread::spawn(move || loop {
        let msg = match event::read() {
            Ok(Event::Key(k)) => match input::translate_key(k) {
                Some(chord) => AppMessage::Input(chord),
                None => continue,
            },
            Ok(Event::Resize(_, _)) => AppMessage::Resize,
            Ok(_) => continue,
            Err(_) => break, // terminal went away
        };
        if !tx.send(msg) {
            break; // the app is shutting down
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, Prompt};
    use termesh_core::input::{Key, KeyChord};
    use termesh_core::Command;
    use termesh_ui::Pane;

    fn generation(value: u64) -> termesh_core::TerminalGeneration {
        termesh_core::TerminalGeneration::new(value)
    }

    #[test]
    fn the_metrics_names_are_the_ones_the_architecture_asks_for() {
        let metrics = Metrics::default();
        for name in ["event_loop_delay", "frame_duration", "turn_duration", "queue_depth"] {
            assert!(metrics.known(name), "ARCHITECTURE.md §19 names {name}");
        }
    }

    #[test]
    fn a_recorded_span_reports_its_duration() {
        let metrics = Metrics::default();
        {
            let _span = metrics.span("frame_duration");
        }
        assert_eq!(metrics.count("frame_duration"), 1);
    }

    #[test]
    fn application_message_bus_applies_backpressure() {
        let (tx, _rx) = app_message_channel();
        for _ in 0..APP_MESSAGE_CAPACITY {
            assert!(tx.try_send(AppMessage::Resize));
        }
        assert!(!tx.try_send(AppMessage::Resize));
    }

    fn render_to_string(model: &mut Model) -> String {
        view::snapshot(model, 96, 28)
    }

    #[test]
    fn shell_renders_all_four_panes_and_status_hints() {
        let s = render_to_string(&mut Model::new());
        for pane in ["Project", "Editor", "Terminal", "Agent"] {
            assert!(s.contains(pane), "status/pane '{pane}' should render");
        }
        assert!(s.contains("Actions"), "status bar should show the palette hint");
    }

    #[test]
    fn palette_opens_filters_and_invokes() {
        let mut m = Model::new();
        m.dispatch(Command::OpenPalette);
        assert!(m.overlay_active());
        assert!(render_to_string(&mut m).contains("Open File"));

        for c in "git".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        let filtered = render_to_string(&mut m);
        assert!(filtered.contains("Stage") || filtered.contains("Commit"));
        assert!(!filtered.contains("Open File"), "filter should exclude non-matches");

        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        assert!(!m.overlay_active(), "Enter should close the palette");
        assert!(m.notification.is_some(), "invoking should surface a notification");
    }

    #[test]
    fn help_lists_every_registered_action_with_its_real_chord() {
        let mut model = Model::new();
        model.dispatch(Command::Action(termesh_core::Action::HelpShow));

        let rows = model.help_rows();
        for action in termesh_core::ActionRegistry::with_defaults().actions() {
            assert!(
                rows.iter().any(|row| row.id == action.id()),
                "{} is missing from help",
                action.id()
            );
        }
        let frame = view::snapshot(&mut model, 120, 40);
        assert!(frame.contains("F10"), "the chords shown are the live ones: {frame}");
    }

    #[test]
    fn help_shows_a_rebound_chord_not_the_compiled_default() {
        let mut model = Model::new();
        termesh_config::apply_keymap_file(
            &mut model.keymap,
            "version = 1\n\n[global]\n\"alt+g\" = \"git.show\"\n",
        );
        model.dispatch(Command::Action(termesh_core::Action::HelpShow));

        let row = model.help_rows().into_iter().find(|row| row.id == "git.show").unwrap();
        assert_eq!(row.chord.as_deref(), Some("alt+g"));
    }

    #[test]
    fn an_action_with_no_binding_is_shown_as_palette_only_in_help() {
        let mut model = Model::new();
        model.dispatch(Command::Action(termesh_core::Action::HelpShow));

        let row = model.help_rows().into_iter().find(|row| row.id == "lsp.rename").unwrap();
        assert_eq!(row.chord, None);
        assert!(view::snapshot(&mut model, 120, 40).contains("palette"));
    }

    #[test]
    fn help_filters_with_the_same_typing_shape_as_the_palette() {
        let mut model = Model::new();
        model.dispatch(Command::Action(termesh_core::Action::HelpShow));
        for character in "rename".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(character)));
        }

        let frame = view::snapshot(&mut model, 120, 40);
        assert!(frame.contains("Rename"), "{frame}");
        assert!(!frame.contains("Open File"), "the filter should hide unrelated rows: {frame}");
    }

    #[test]
    fn closing_help_restores_the_focus_it_opened_from() {
        let mut model = Model::new();
        model.focus = Pane::Project;
        model.dispatch(Command::Action(termesh_core::Action::HelpShow));
        model.focus = Pane::Agent;

        input::on_chord(&mut model, KeyChord::plain(Key::Esc));

        assert_eq!(model.focus, Pane::Project);
        assert!(!model.overlay_active());
    }

    #[test]
    fn a_first_run_with_no_workspace_says_what_to_press() {
        let frame = view::snapshot(&mut Model::new(), 100, 30);
        assert!(frame.contains("Ctrl+P"), "{frame}");
        assert!(frame.contains("F10"), "{frame}");
        assert!(frame.contains("F11"), "{frame}");
    }

    #[test]
    fn esc_closes_the_palette() {
        let mut m = Model::new();
        m.dispatch(Command::OpenPalette);
        input::on_chord(&mut m, KeyChord::plain(Key::Esc));
        assert!(!m.overlay_active());
    }

    #[test]
    fn focus_cycles_forward_and_back() {
        let mut m = Model::new();
        m.focus = Pane::Project;
        m.dispatch(Command::FocusNext);
        assert_eq!(m.focus, Pane::Editor);
        m.dispatch(Command::FocusPrev);
        assert_eq!(m.focus, Pane::Project);
    }

    /// The Tab ring skips the Terminal on purpose. A focused shell owns Tab, so a
    /// Terminal in the ring is a one-way door: you Tab in and the next Tab goes to the
    /// process, stranding you. It has its own chord instead.
    #[test]
    fn the_tab_ring_skips_the_terminal_so_it_cannot_strand_the_user() {
        let mut model = Model::new();
        model.focus = Pane::Project;

        for expected in [Pane::Editor, Pane::Agent, Pane::Project, Pane::Editor] {
            input::on_chord(&mut model, KeyChord::plain(Key::Tab));
            assert_eq!(model.focus, expected);
        }
    }

    /// The bug this replaced: Tab out of the terminal typed a tab, F6 came back to the
    /// editor, and Tab returned to the terminal — an inescapable two-pane loop that made
    /// Project and Agent unreachable in the forward direction.
    #[test]
    fn cycling_out_of_a_terminal_does_not_bounce_straight_back_into_it() {
        let mut model = Model::new();
        model.focus = Pane::Editor;

        input::on_chord(&mut model, KeyChord::plain(Key::F(6))); // into the terminal
        assert_eq!(model.focus, Pane::Terminal);
        input::on_chord(&mut model, KeyChord::plain(Key::F(6))); // back out
        assert_eq!(model.focus, Pane::Editor);

        input::on_chord(&mut model, KeyChord::plain(Key::Tab));
        assert_eq!(model.focus, Pane::Agent, "Tab must move on, not re-enter the terminal");
    }

    /// Every pane is reachable from inside a running shell, not just the way back out.
    #[test]
    fn direct_focus_keys_escape_a_focused_terminal() {
        for (chord, expected) in [
            (KeyChord::plain(Key::F(1)), Pane::Project),
            (KeyChord::plain(Key::F(2)), Pane::Editor),
            (KeyChord::plain(Key::F(7)), Pane::Agent),
        ] {
            let mut model = running_terminal();
            assert_eq!(model.focus, Pane::Terminal, "precondition");

            input::on_chord(&mut model, chord);

            assert_eq!(model.focus, expected, "{chord} should leave the shell");
            assert!(model.take_pty_requests().is_empty(), "{chord} must not reach the PTY");
        }
    }

    #[test]
    fn resize_grows_and_clamps_sidebar() {
        let mut m = Model::new();
        let start = m.layout.sidebar_pct;
        m.dispatch(Command::GrowSidebar);
        assert!(m.layout.sidebar_pct > start);
        for _ in 0..40 {
            m.dispatch(Command::GrowSidebar);
        }
        assert!(m.layout.sidebar_pct <= 45, "sidebar must stay clamped");
    }

    #[test]
    fn keymap_routes_ctrl_p_to_quick_open() {
        let (mut m, _) = opened();
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('p')));
        assert!(matches!(m.overlays.last(), Some(crate::model::Overlay::Search(_))));
    }

    // --- file explorer (Phase 02) -------------------------------------------------
    //
    // Driven entirely through the in-memory fake, so these assert real behaviour with
    // no disk, no threads, and no timing.

    use termesh_core::{
        CodeAction, Diagnostic, DiagnosticOrigin, DiagnosticSeverity, FsEvent, FsRequest,
        GitBranch, GitBranchStatus, GitChangeKind, GitContextDiff, GitDiffTarget, GitEvent,
        GitFileDiff, GitFileStatus, GitRepositorySnapshot, GitRequest, GitRequestId, LspEvent,
        LspRequest, LspRequestId, LspServerId, SearchEvent, SearchMatch, SearchMode, TextEdit,
        TextPosition, TextRange, WorkspaceEdit,
    };
    use termesh_filesystem::{DirReader, FileSystemService, IgnoreOptions};
    use termesh_test_support::FakeFileSystem;

    /// Crash teardown has two halves. The terminal half is the panic hook in `tui.rs`.
    /// The child-process half is the `Drop` impl on each worker, which kills and reaps
    /// its child — so a crash must leave no orphaned `rust-analyzer`, `jdtls`, ACP agent,
    /// or PTY behind.
    ///
    /// That second half rests on a premise no behavioural test can reach: the panic has
    /// to *unwind* through `run`. Two edits would silently remove it, and neither shows
    /// up as a failing test anywhere else, because unit tests run in the dev profile and
    /// never take an early-exit path:
    ///
    /// - `panic = "abort"` in any profile — no `Drop` runs at all, in that profile only.
    /// - `std::process::exit` on any path in this binary — `Drop` is skipped for
    ///   everything still live on the stack.
    ///
    /// So the assertions here read the build configuration and the source rather than
    /// exercising behaviour. Asserting instead that some probe value's `Drop` runs during
    /// an unwind would only restate a language guarantee: it passes whether or not this
    /// binary keeps the premise those guarantees depend on.
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/app sits two levels below the workspace root")
            .to_path_buf()
    }

    #[test]
    fn no_build_profile_aborts_on_panic_and_skips_worker_teardown() {
        let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
        let aborting: Vec<&str> = manifest
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("panic") && line.contains("abort"))
            .collect();

        assert!(
            aborting.is_empty(),
            "an aborting profile skips every worker Drop, orphaning language servers and \
             PTY children on a crash: {aborting:?}"
        );
    }

    #[test]
    fn no_path_in_the_binary_exits_the_process_instead_of_unwinding() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for file in ["cli.rs", "input.rs", "main.rs", "model.rs", "tui.rs", "view.rs"] {
            let source = std::fs::read_to_string(src.join(file)).unwrap();
            // Production code only: a `process::exit` inside a `#[cfg(test)]` module
            // cannot skip a production `Drop`, and this test's own prose lives there.
            for (number, line) in
                source.lines().take_while(|line| line.trim() != "#[cfg(test)]").enumerate()
            {
                if line.contains("process::exit(") {
                    offenders.push(format!("{file}:{}", number + 1));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "process::exit skips Drop for everything live on the stack, so workers never \
             kill their children: {offenders:?}"
        );
    }

    /// 0.1.0 shipped "Tier 0: run any AI CLI in a terminal instead (Phase 04)." in the
    /// agent pane — the first screen anyone without an agent configured sees. Both are
    /// build vocabulary: "Phase NN" is the internal plan and "Tier N" is ADR-0003's
    /// framing. Neither means anything to a user, and a released binary cannot be edited,
    /// so the guard is a test rather than a convention.
    ///
    /// It scans string literals only. Comments and doc comments are where this vocabulary
    /// belongs and are left alone — including the ones in this test.
    #[test]
    fn no_screen_text_uses_the_projects_internal_vocabulary() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for file in ["cli.rs", "input.rs", "model.rs", "view.rs"] {
            let source = std::fs::read_to_string(src.join(file)).unwrap();
            for (number, line) in
                source.lines().take_while(|line| line.trim() != "#[cfg(test)]").enumerate()
            {
                let code = line.trim_start();
                if code.starts_with("//") || !code.contains('"') {
                    continue;
                }
                for term in ["Phase 0", "Phase 1", "Tier 0", "Tier 1", "ADR-"] {
                    if code.contains(term) {
                        offenders.push(format!("{file}:{}: {term}", number + 1));
                    }
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "screen text must say what the user can do, not where the feature came from: \
             {offenders:?}"
        );
    }

    fn sample_fs() -> FakeFileSystem {
        FakeFileSystem::with_paths(&[
            "/proj/Cargo.toml",
            "/proj/README.md",
            "/proj/src/main.rs",
            "/proj/src/model.rs",
        ])
    }

    /// A model with `/proj` open and its first level loaded.
    fn opened() -> (Model, FakeFileSystem) {
        let fs = sample_fs();
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        (m, fs)
    }

    #[test]
    fn reopening_a_workspace_restores_its_buffers_and_active_tab() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/src/main.rs", b"fn main() {}\n");
        fs.add_file("/proj/src/lib.rs", b"pub fn f() {}\n");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.open = vec!["/proj/src/main.rs".into(), "/proj/src/lib.rs".into()];
        workspace.active = Some("/proj/src/lib.rs".into());
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);

        let mut model = Model::new();
        model.restore_session(&fs, &session);
        let mut reader = DirReader::new(&fs, std::path::Path::new("/proj"), model.ignore_options);
        model.settle_fs_sync(&mut reader);

        let open = model
            .buffers
            .iter()
            .filter_map(|buffer| buffer.path().map(std::path::Path::to_path_buf))
            .collect::<Vec<_>>();
        assert_eq!(
            open,
            [
                std::path::PathBuf::from("/proj/src/main.rs"),
                std::path::PathBuf::from("/proj/src/lib.rs")
            ]
        );
        assert_eq!(
            model.active_buffer().and_then(|buffer| buffer.path()),
            Some(std::path::Path::new("/proj/src/lib.rs"))
        );
    }

    #[test]
    fn restored_tabs_keep_session_order_when_reads_complete_out_of_order() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/a.rs", b"a\n");
        fs.add_file("/proj/b.rs", b"b\n");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.open = vec!["/proj/a.rs".into(), "/proj/b.rs".into()];
        workspace.active = Some("/proj/a.rs".into());
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();

        model.restore_session(&fs, &session);
        let reads = model
            .take_fs_requests()
            .into_iter()
            .filter_map(|request| match request {
                FsRequest::ReadFile { buffer, path } => Some((buffer, path)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 2);
        for (buffer, path) in reads.into_iter().rev() {
            let contents = fs.read_file(&path).unwrap();
            model.on_fs_event(FsEvent::FileLoaded { buffer, path, contents });
        }

        let open = model
            .buffers
            .iter()
            .filter_map(|buffer| buffer.path().map(std::path::Path::to_path_buf))
            .collect::<Vec<_>>();
        assert_eq!(
            open,
            [std::path::PathBuf::from("/proj/a.rs"), std::path::PathBuf::from("/proj/b.rs")]
        );
        assert_eq!(
            model.active_buffer().and_then(|buffer| buffer.path()),
            Some(std::path::Path::new("/proj/a.rs"))
        );
    }

    #[test]
    fn missing_restored_files_are_skipped_without_hiding_sibling_failures() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/a.rs", b"a\n");
        fs.add_file("/proj/b.rs", b"b\n");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.open = vec![
            "/proj/a.rs".into(),
            "/proj/gone-one.rs".into(),
            "/proj/gone-two.rs".into(),
            "/proj/b.rs".into(),
        ];
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();

        model.restore_session(&fs, &session);
        let mut reader = DirReader::new(&fs, std::path::Path::new("/proj"), model.ignore_options);
        model.settle_fs_sync(&mut reader);

        let open = model
            .buffers
            .iter()
            .filter_map(|buffer| buffer.path().map(std::path::Path::to_path_buf))
            .collect::<Vec<_>>();
        assert_eq!(
            open,
            [std::path::PathBuf::from("/proj/a.rs"), std::path::PathBuf::from("/proj/b.rs")]
        );
        let notification = model.notification.as_deref().unwrap_or_default();
        assert!(notification.contains("gone-one.rs"), "{notification}");
        assert!(notification.contains("gone-two.rs"), "{notification}");
        assert!(!model.session_restore_pending());
    }

    #[test]
    fn a_non_text_restored_buffer_is_skipped_and_finishes_the_state_machine() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/binary.dat", &[0xff, 0xfe]);
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.open = vec!["/proj/binary.dat".into()];
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();

        model.restore_session(&fs, &session);
        let mut reader = DirReader::new(&fs, std::path::Path::new("/proj"), model.ignore_options);
        model.settle_fs_sync(&mut reader);

        assert!(!model.session_restore_pending());
        assert!(model.notification.as_deref().unwrap_or_default().contains("not a text file"));
    }

    #[test]
    fn a_corrupt_session_file_still_starts_a_usable_editor() {
        let fs = FakeFileSystem::new();
        fs.add_file("/cfg/termesh/session.toml", b"recent = [[[\n");
        let store = termesh_workspace::FileSessionStore::new(&fs, "/cfg/termesh/session.toml");
        let session = store.load();
        let mut model = Model::new();

        model.restore_session(&fs, &session);

        assert!(model.buffers.is_empty());
        let frame = view::snapshot(&mut model, 100, 30);
        assert!(frame.contains("Project"), "{frame}");
        assert!(frame.contains("Editor"), "{frame}");
    }

    #[test]
    fn restored_geometry_is_applied_before_buffer_reads_complete() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/a.rs", b"a\n");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.open = vec!["/proj/a.rs".into()];
        workspace.layout = termesh_workspace::PaneGeometry::new(31, 38, 19);
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();

        model.restore_session(&fs, &session);

        assert!(model.buffers.is_empty(), "buffer reads are still queued");
        assert_eq!(model.layout.sidebar_pct, 31);
        assert_eq!(model.layout.bottom_pct, 38);
        assert_eq!(model.layout.agent_pct, 19);
        let frame = view::snapshot(&mut model, 100, 30);
        assert!(frame.contains("Project"), "the shell renders before reads complete: {frame}");
    }

    #[test]
    fn terminals_restore_as_fresh_shells_in_their_working_directories() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/proj/src");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.terminals = vec!["/proj".into(), "/proj/src".into()];
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();

        model.restore_session(&fs, &session);

        let cwds = model
            .take_pty_requests()
            .into_iter()
            .filter_map(|request| match request {
                termesh_core::PtyRequest::Spawn { spec, .. } => Some(spec.cwd),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cwds,
            [std::path::PathBuf::from("/proj"), std::path::PathBuf::from("/proj/src")]
        );
        assert_eq!(model.focus, Pane::Project, "startup restoration must not steal focus");
    }

    #[test]
    fn prior_agent_transcript_is_read_only_history_not_a_resumed_session() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/proj");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.agent_history = vec![termesh_workspace::AgentHistoryLine {
            speaker: termesh_workspace::AgentHistorySpeaker::Agent,
            text: "Prior answer".into(),
        }];
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();
        model.agent_name = Some("test-agent".into());

        model.restore_session(&fs, &session);

        let frame = view::snapshot(&mut model, 120, 40);
        assert!(frame.contains("Prior answer"), "{frame}");
        assert!(frame.contains("new session"), "{frame}");
        assert!(model.agent.is_none(), "history must not manufacture an ACP session");
        assert!(model.take_agent_requests().is_empty(), "history is never replayed to ACP");
    }

    #[test]
    fn persisting_a_live_model_captures_restart_owned_state() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/a.rs", b"a\n");
        fs.add_file("/proj/b.rs", b"b\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, "/proj/a.rs".into());
        model.open_file_sync(&fs, "/proj/b.rs".into());
        model.layout.sidebar_pct = 29;
        model.layout.bottom_pct = 34;
        model.layout.agent_pct = 21;
        model.dispatch(Command::Action(termesh_core::Action::TerminalFocus));
        let _ = model.take_pty_requests();
        let mut session = termesh_workspace::Session::default();

        model.persist_session(&mut session);

        let restored = session.workspace.expect("the open workspace is persisted");
        assert_eq!(restored.root, std::path::PathBuf::from("/proj"));
        assert_eq!(
            restored.open,
            [std::path::PathBuf::from("/proj/a.rs"), std::path::PathBuf::from("/proj/b.rs")]
        );
        assert_eq!(restored.active, Some(std::path::PathBuf::from("/proj/b.rs")));
        assert_eq!(restored.layout.sidebar_pct, 29);
        assert_eq!(restored.layout.bottom_pct, 34);
        assert_eq!(restored.layout.agent_pct, 21);
        assert_eq!(restored.terminals, [std::path::PathBuf::from("/proj")]);
        assert_eq!(session.recent, [std::path::PathBuf::from("/proj")]);
    }

    #[test]
    fn configured_agent_starts_fresh_after_history_is_restored() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/proj");
        let mut workspace = termesh_workspace::RestoredWorkspace::new("/proj".into());
        workspace.agent_history = vec![termesh_workspace::AgentHistoryLine {
            speaker: termesh_workspace::AgentHistorySpeaker::You,
            text: "old question".into(),
        }];
        let mut session = termesh_workspace::Session::default();
        session.workspace = Some(workspace);
        let mut model = Model::new();
        model.restore_session(&fs, &session);
        model.agent_name = Some("test-agent".into());

        model.start_fresh_agent_after_restore();

        assert_eq!(
            model.take_agent_requests(),
            [termesh_core::AgentRequest::NewSession { cwd: "/proj".into() }]
        );
    }

    #[test]
    fn a_dirty_buffer_writes_a_draft() {
        let (mut model, fs) = opened();
        model.open_file_sync(&fs, "/proj/src/main.rs".into());
        model.set_drafts_dir(Some("/cfg/drafts".into()));
        model.type_char('x');

        model.flush_drafts(&fs);

        let (drafts, diagnostics) = termesh_workspace::drafts::drafts_for(
            &fs,
            std::path::Path::new("/cfg/drafts"),
            std::path::Path::new("/proj"),
        )
        .unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].path, std::path::PathBuf::from("/proj/src/main.rs"));
        assert_eq!(drafts[0].text, "x");
    }

    #[test]
    fn saving_clears_the_draft() {
        let (mut model, fs) = opened();
        model.open_file_sync(&fs, "/proj/src/main.rs".into());
        model.set_drafts_dir(Some("/cfg/drafts".into()));
        model.type_char('x');
        model.flush_drafts(&fs);

        model.dispatch(Command::Action(termesh_core::Action::FileSave));
        let mut reader = DirReader::new(&fs, std::path::Path::new("/proj"), model.ignore_options);
        model.settle_fs_sync(&mut reader);
        model.flush_drafts(&fs);

        let (drafts, _) = termesh_workspace::drafts::drafts_for(
            &fs,
            std::path::Path::new("/cfg/drafts"),
            std::path::Path::new("/proj"),
        )
        .unwrap();
        assert!(drafts.is_empty());
    }

    #[test]
    fn draft_debounce_uses_the_configured_algorithmic_deadline() {
        let (mut model, fs) = opened();
        model.open_file_sync(&fs, "/proj/src/main.rs".into());
        model.set_drafts_dir(Some("/cfg/drafts".into()));
        let _ = model.take_fs_requests();
        model.type_char('x');
        let base = std::time::Instant::now();
        model.reschedule_drafts_at(base);

        assert_eq!(model.next_draft_deadline(), Some(base + std::time::Duration::from_secs(2)));
        model.queue_due_drafts(base + std::time::Duration::from_secs(1));
        assert!(model.take_fs_requests().is_empty());

        model.queue_due_drafts(base + std::time::Duration::from_secs(2));
        let requests = model.take_fs_requests();
        assert!(requests.iter().any(|request| matches!(request, FsRequest::WriteFile { .. })));
    }

    fn workspace_with_a_draft() -> (Model, FakeFileSystem) {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/src/main.rs", b"fn main() {}\n");
        termesh_workspace::drafts::write_draft(
            &fs,
            std::path::Path::new("/cfg/drafts"),
            &termesh_workspace::drafts::Draft {
                path: "/proj/src/main.rs".into(),
                saved_at: std::time::SystemTime::now(),
                text: "// unsaved\n".into(),
            },
        )
        .unwrap();
        let mut model = Model::new();
        model.set_drafts_dir(Some("/cfg/drafts".into()));
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        (model, fs)
    }

    #[test]
    fn a_draft_is_offered_rather_than_applied() {
        let (mut model, fs) = workspace_with_a_draft();

        model.restore_drafts(&fs);

        assert!(model.overlay_is_draft_recovery());
        let buffer = model
            .buffers
            .iter()
            .find(|buffer| buffer.path() == Some(std::path::Path::new("/proj/src/main.rs")))
            .unwrap();
        assert_eq!(buffer.text().to_string(), "fn main() {}\n", "disk still wins");
    }

    #[test]
    fn accepting_a_draft_applies_it_through_the_transaction_spine() {
        let (mut model, fs) = workspace_with_a_draft();
        model.restore_drafts(&fs);

        model.dispatch(Command::Action(termesh_core::Action::WorkspaceRestoreDrafts));

        assert_eq!(model.active_buffer().unwrap().text().to_string(), "// unsaved\n");
        model.dispatch(Command::EditorUndo);
        assert_eq!(model.active_buffer().unwrap().text().to_string(), "fn main() {}\n");
    }

    #[test]
    fn discarding_removes_the_draft_files() {
        let (mut model, fs) = workspace_with_a_draft();
        model.restore_drafts(&fs);

        model.discard_drafts(&fs);

        let (drafts, _) = termesh_workspace::drafts::drafts_for(
            &fs,
            std::path::Path::new("/cfg/drafts"),
            std::path::Path::new("/proj"),
        )
        .unwrap();
        assert!(drafts.is_empty());
    }

    /// Requests for Phase 07's one session, without routing noise in each assertion.
    fn lsp_requests(model: &mut Model) -> Vec<LspRequest> {
        model.take_lsp_requests().into_iter().map(|(_, request)| request).collect()
    }

    fn one_lsp_request(model: &mut Model) -> LspRequest {
        let mut requests = lsp_requests(model);
        assert_eq!(requests.len(), 1, "{requests:#?}");
        requests.pop().unwrap()
    }

    fn rust_server(model: &Model) -> LspServerId {
        *model.lsp.sessions.keys().next().expect("a Rust language session")
    }

    fn server_for_language(model: &Model, language: &str) -> LspServerId {
        model
            .lsp
            .sessions
            .iter()
            .find_map(|(server, session)| (session.language == language).then_some(*server))
            .unwrap_or_else(|| panic!("a {language} language session"))
    }

    fn polyglot_workspace() -> (Model, FakeFileSystem) {
        let fs = FakeFileSystem::with_paths(&[
            "/proj/Cargo.toml",
            "/proj/package.json",
            "/proj/src/main.rs",
            "/proj/web/app.ts",
            "/proj/README.md",
        ]);
        fs.add_file("/proj/src/main.rs", b"fn main() {}\n");
        fs.add_file("/proj/web/app.ts", b"const answer = 42;\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        (model, fs)
    }

    fn opened_rust_file(text: &[u8]) -> (Model, FakeFileSystem) {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", text);
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, "/proj/src/main.rs".into());
        (model, fs)
    }

    fn java_workspace_with_open_source_file() -> (Model, FakeFileSystem) {
        let fs = FakeFileSystem::with_paths(&["/proj/pom.xml", "/proj/src/App.java"]);
        fs.add_file("/proj/src/App.java", b"class App {}\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, "/proj/src/App.java".into());
        (model, fs)
    }

    fn hover_id(requests: &[LspRequest]) -> LspRequestId {
        requests
            .iter()
            .find_map(|request| match request {
                LspRequest::Hover { id, .. } => Some(*id),
                _ => None,
            })
            .expect("a hover request")
    }

    mod lsp_restart {
        use super::*;

        #[test]
        fn restart_relaunches_the_session_with_the_recipe_it_resolved() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(Action::LspRestart));

            let requests = lsp_requests(&mut model);
            // Same id on purpose: the main loop replaces the process bound to it, so the
            // model never routes to a session that no longer exists.
            assert!(
                requests.iter().any(|request| matches!(
                    request,
                    LspRequest::Start { server: started, command, .. }
                        if *started == server && command[0] == "rust-analyzer"
                )),
                "{requests:#?}"
            );
        }

        #[test]
        fn restart_reopens_documents_because_the_new_process_has_never_seen_them() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(Action::LspRestart));

            let requests = lsp_requests(&mut model);
            let opened = requests.iter().find_map(|request| match request {
                LspRequest::DidOpen { path, version, .. } => Some((path, *version)),
                _ => None,
            });
            let (path, version) = opened.expect("the document is reopened after a restart");
            assert_eq!(path, std::path::Path::new("/proj/src/main.rs"));
            assert_eq!(version, 1, "wire versions restart at one for a fresh process");
        }

        #[test]
        fn restart_drops_diagnostics_the_dead_process_published() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            let _ = lsp_requests(&mut model);
            let version =
                model.lsp.sessions[&server].open_docs[std::path::Path::new("/proj/src/main.rs")];
            model.on_lsp_event(
                server,
                LspEvent::Diagnostics {
                    path: "/proj/src/main.rs".into(),
                    version: Some(version),
                    items: vec![termesh_core::Diagnostic {
                        path: "/proj/src/main.rs".into(),
                        range: termesh_core::TextRange {
                            start: termesh_core::TextPosition { line: 0, character: 0 },
                            end: termesh_core::TextPosition { line: 0, character: 2 },
                        },
                        severity: termesh_core::DiagnosticSeverity::Error,
                        origin: termesh_core::DiagnosticOrigin::LanguageServer,
                        source: "rust-analyzer".into(),
                        code: None,
                        message: "mismatched types".into(),
                    }],
                },
            );
            assert!(!model.problem_rows().is_empty());

            model.dispatch(Command::Action(Action::LspRestart));

            assert!(
                model.problem_rows().is_empty(),
                "a replacement process republishes from its own analysis"
            );
        }

        #[test]
        fn restart_without_a_session_reports_rather_than_doing_nothing() {
            let mut model = Model::new();
            model.dispatch(Command::Action(Action::LspRestart));
            assert_eq!(
                model.notification.as_deref(),
                Some("No language server is configured for this workspace")
            );
        }
    }

    mod lsp_lifecycle {
        use super::*;

        #[test]
        fn opening_a_workspace_starts_no_language_server() {
            let (mut model, _) = opened();
            assert!(lsp_requests(&mut model)
                .iter()
                .all(|request| !matches!(request, LspRequest::Start { .. })));
            assert!(model.lsp.sessions.is_empty());
            assert_eq!(model.lsp.configured.len(), 1);
            assert!(matches!(model.lsp.configured[0].load, crate::lsp_state::LspLoadState::Idle));
        }

        #[test]
        fn opening_a_claimed_document_starts_exactly_one_session() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let requests = lsp_requests(&mut model);
            let starts: Vec<_> = requests
                .iter()
                .filter(|request| matches!(request, LspRequest::Start { .. }))
                .collect();

            assert_eq!(starts.len(), 1, "{requests:#?}");
            assert_eq!(model.lsp.sessions.len(), 1);
        }

        #[test]
        fn a_second_language_starts_a_second_session_without_disturbing_the_first() {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/src/main.rs".into());
            let first = rust_server(&model);
            let _ = lsp_requests(&mut model);

            model.open_file_sync(&fs, "/proj/web/app.ts".into());

            assert_eq!(model.lsp.sessions.len(), 2);
            assert!(model.lsp.sessions.contains_key(&first), "the Rust session is untouched");
            let requests = lsp_requests(&mut model);
            assert!(requests.iter().any(|request| matches!(
                request,
                LspRequest::Start { command, .. }
                    if command[0] == "typescript-language-server"
            )));
        }

        #[test]
        fn a_document_no_recipe_claims_starts_nothing() {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/README.md".into());

            assert!(model.lsp.sessions.is_empty());
            assert!(lsp_requests(&mut model).is_empty());
        }

        #[test]
        fn each_session_only_receives_its_own_documents() {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/src/main.rs".into());
            model.open_file_sync(&fs, "/proj/web/app.ts".into());
            let _ = model.take_lsp_requests();

            model.type_char('x');

            let changes: Vec<_> = model
                .take_lsp_requests()
                .into_iter()
                .filter_map(|(server, request)| match request {
                    LspRequest::DidChange { path, .. } => Some((server, path)),
                    _ => None,
                })
                .collect();
            assert_eq!(changes.len(), 1, "one edit belongs to one server: {changes:#?}");
            let (server, path) = &changes[0];
            let session = &model.lsp.sessions[server];
            assert_eq!(session.language, "typescript");
            assert!(
                session.extensions.iter().any(|extension| {
                    path.extension().and_then(|value| value.to_str()) == Some(extension.as_str())
                }),
                "{path:?} was routed to the {} session",
                session.language
            );
        }

        #[test]
        fn one_unavailable_server_leaves_its_sibling_working() {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/src/main.rs".into());
            model.open_file_sync(&fs, "/proj/web/app.ts".into());
            let node = server_for_language(&model, "typescript");

            model.on_lsp_event(node, LspEvent::Unavailable { message: "not installed".into() });

            let rust = server_for_language(&model, "rust");
            assert!(!matches!(
                model.lsp.sessions[&rust].load,
                crate::lsp_state::LspLoadState::Unavailable(_)
            ));
        }

        #[test]
        fn watched_files_are_forwarded_to_every_live_session() {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/src/main.rs".into());
            model.open_file_sync(&fs, "/proj/web/app.ts".into());
            let _ = model.take_lsp_requests();

            model.on_fs_event(FsEvent::Changed(vec!["/proj/README.md".into()]));

            let targets: std::collections::BTreeSet<_> = model
                .take_lsp_requests()
                .into_iter()
                .filter_map(|(server, request)| {
                    matches!(request, LspRequest::WatchedFilesChanged { .. }).then_some(server)
                })
                .collect();
            assert_eq!(targets.len(), 2);
        }

        #[test]
        fn editing_a_build_file_asks_the_server_to_reload_the_project() {
            // Without this, adding a dependency means restarting the server by hand —
            // the most common Java-tooling complaint (ADR-0013 Context).
            let (mut model, _) = java_workspace_with_open_source_file();
            let _ = lsp_requests(&mut model);

            model.on_fs_event(FsEvent::Changed(vec!["/proj/pom.xml".into()]));

            let requests = lsp_requests(&mut model);
            assert!(
                requests.iter().any(|request| matches!(
                    request,
                    LspRequest::ReloadProject { paths } if paths == &[std::path::PathBuf::from("/proj/pom.xml")]
                )),
                "{requests:#?}"
            );
        }

        #[test]
        fn editing_a_source_file_does_not_ask_for_a_project_reload() {
            let (mut model, _) = java_workspace_with_open_source_file();
            let _ = lsp_requests(&mut model);

            model.on_fs_event(FsEvent::Changed(vec!["/proj/src/App.java".into()]));

            assert!(!lsp_requests(&mut model)
                .iter()
                .any(|request| matches!(request, LspRequest::ReloadProject { .. })));
        }

        #[test]
        fn a_reload_only_reaches_the_session_that_claims_java() {
            // A Java+Node workspace must not send a JDT-specific request to the
            // TypeScript server, which would answer it with -32601.
            let fs = FakeFileSystem::with_paths(&[
                "/proj/pom.xml",
                "/proj/package.json",
                "/proj/src/App.java",
                "/proj/web/app.ts",
            ]);
            fs.add_file("/proj/src/App.java", b"class App {}\n");
            fs.add_file("/proj/web/app.ts", b"const answer = 42;\n");
            let mut model = Model::new();
            model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
            model.open_file_sync(&fs, "/proj/src/App.java".into());
            model.open_file_sync(&fs, "/proj/web/app.ts".into());
            let java = server_for_language(&model, "java");
            let typescript = server_for_language(&model, "typescript");
            let _ = model.take_lsp_requests();

            model.on_fs_event(FsEvent::Changed(vec!["/proj/pom.xml".into()]));

            let targets: Vec<_> = model
                .take_lsp_requests()
                .into_iter()
                .filter_map(|(server, request)| {
                    matches!(request, LspRequest::ReloadProject { .. }).then_some(server)
                })
                .collect();
            assert_eq!(targets, vec![java]);
            assert!(!targets.contains(&typescript));
        }

        #[test]
        fn restart_before_first_use_reports_that_nothing_started() {
            let (mut model, _) = opened();
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(Action::LspRestart));

            assert_eq!(model.notification.as_deref(), Some("No language server has started yet"));
            assert!(lsp_requests(&mut model)
                .iter()
                .all(|request| !matches!(request, LspRequest::Start { .. })));
        }

        #[test]
        fn an_idle_configured_recipe_is_not_reported_as_unavailable() {
            let (mut model, _) = opened();
            let frame = render_to_string(&mut model);

            assert!(!frame.contains("LSP unavailable"), "{frame}");
        }
    }

    mod polyglot_lsp_surfaces {
        use super::*;

        fn two_live_sessions() -> (Model, FakeFileSystem) {
            let (mut model, fs) = polyglot_workspace();
            model.open_file_sync(&fs, "/proj/src/main.rs".into());
            model.open_file_sync(&fs, "/proj/web/app.ts".into());
            let _ = model.take_lsp_requests();
            (model, fs)
        }

        fn publish(
            model: &mut Model,
            server: LspServerId,
            path: &str,
            source: &str,
            message: &str,
        ) {
            let version = model.lsp.sessions[&server].open_docs[std::path::Path::new(path)];
            model.on_lsp_event(
                server,
                LspEvent::Diagnostics {
                    path: path.into(),
                    version: Some(version),
                    items: vec![Diagnostic {
                        path: path.into(),
                        range: TextRange {
                            start: TextPosition { line: 0, character: 0 },
                            end: TextPosition { line: 0, character: 1 },
                        },
                        severity: DiagnosticSeverity::Error,
                        origin: DiagnosticOrigin::LanguageServer,
                        source: source.into(),
                        code: None,
                        message: message.into(),
                    }],
                },
            );
        }

        #[test]
        fn the_status_bar_names_every_detected_language_not_just_the_primary() {
            // A polyglot root that reads as one language tells the developer we found
            // less than we did — and `WorkspaceSnapshot::project_kinds` existed with no
            // consumer until it was wired here.
            let (mut model, _) = polyglot_workspace();
            let frame = render_to_string(&mut model);
            assert!(frame.contains("(rust, node)"), "{frame}");
        }

        #[test]
        fn the_agent_is_told_every_detected_language_too() {
            let (model, _) = polyglot_workspace();
            let context = model.agent_context();
            assert!(context.contains("project:"), "{context}");
            assert!(
                context
                    .lines()
                    .any(|line| line.starts_with("project:") && line.contains("rust, node")),
                "{context}"
            );
        }

        #[test]
        fn the_status_bar_reports_the_session_owning_the_active_buffer() {
            let (mut model, _) = two_live_sessions();
            let node = server_for_language(&model, "typescript");
            model.on_lsp_event(
                node,
                LspEvent::Indexing { message: "loading project".into(), percent: Some(40) },
            );
            model.open_file("/proj/src/main.rs".into());

            let frame = render_to_string(&mut model);

            assert!(
                !frame.contains("loading project"),
                "the Node state is not the Rust file's state"
            );
        }

        #[test]
        fn an_unavailable_server_is_reported_against_its_own_language() {
            let (mut model, _) = two_live_sessions();
            let node = server_for_language(&model, "typescript");
            model.on_lsp_event(node, LspEvent::Unavailable { message: "not installed".into() });

            let frame = render_to_string(&mut model);

            assert!(frame.contains("typescript"), "{frame}");
        }

        #[test]
        fn diagnostics_from_two_servers_coexist_in_the_panel() {
            let (mut model, _) = two_live_sessions();
            let rust = server_for_language(&model, "rust");
            let node = server_for_language(&model, "typescript");
            publish(&mut model, rust, "/proj/src/main.rs", "rust-analyzer", "mismatched types");
            publish(&mut model, node, "/proj/web/app.ts", "typescript", "cannot find name");

            let rows = model.problem_rows();

            assert_eq!(rows.len(), 2);
            assert!(rows.iter().any(|row| row.source == "rust-analyzer"));
            assert!(rows.iter().any(|row| row.source == "typescript"));
        }

        #[test]
        fn the_agent_context_names_every_detected_language_and_live_session() {
            let (model, _) = two_live_sessions();

            let context = model.lsp_agent_context();

            assert!(context.contains("rust"), "{context}");
            assert!(context.contains("node"), "{context}");
            assert!(context.contains("typescript"), "{context}");
        }
    }

    mod lsp_sync {
        use super::*;

        #[test]
        fn opening_a_file_sends_did_open_with_buffer_text_not_disk_bytes() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\r\n");
            let requests = lsp_requests(&mut model);
            let (text, version) = requests
                .iter()
                .find_map(|request| match request {
                    LspRequest::DidOpen { text, version, .. } => Some((text, version)),
                    _ => None,
                })
                .expect("didOpen");

            assert_eq!(text, &model.active_buffer().unwrap().text().to_string());
            assert!(!text.contains('\r'), "Buffer::from_text normalises CRLF");
            assert_eq!(*version, 1, "wire versions start at one");
        }

        #[test]
        fn typing_sends_one_incremental_change_with_the_next_wire_version() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);

            model.type_char('x');

            assert!(matches!(
                one_lsp_request(&mut model),
                LspRequest::DidChange { version: 2, change, .. } if change.range.is_some()
            ));
        }

        #[test]
        fn undo_advances_the_wire_version_rather_than_rewinding_it() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.type_char('x');
            let edit_version = match one_lsp_request(&mut model) {
                LspRequest::DidChange { version, .. } => version,
                request => panic!("expected didChange, got {request:#?}"),
            };

            model.dispatch(Command::EditorUndo);
            let undo_version = match one_lsp_request(&mut model) {
                LspRequest::DidChange { version, .. } => version,
                request => panic!("expected didChange, got {request:#?}"),
            };

            assert_eq!(undo_version, edit_version + 1);
        }

        #[test]
        fn reloading_a_file_from_disk_closes_and_reopens_the_document() {
            let (mut model, fs) = opened_rust_file(b"old\n");
            let _ = lsp_requests(&mut model);
            fs.add_file("/proj/src/main.rs", b"new\n");

            model.on_fs_event(FsEvent::Changed(vec!["/proj/src/main.rs".into()]));
            let mut reader = termesh_filesystem::DirReader::new(
                &fs,
                std::path::Path::new("/proj"),
                IgnoreOptions::default(),
            );
            model.settle_fs_sync(&mut reader);

            let requests = lsp_requests(&mut model);
            let close = requests
                .iter()
                .position(|request| matches!(request, LspRequest::DidClose { .. }))
                .expect("reload closes the old document");
            let reopen = requests
                .iter()
                .rposition(|request| matches!(request, LspRequest::DidOpen { .. }))
                .expect("reload opens the replacement document");
            assert!(close < reopen, "{requests:#?}");
        }

        #[test]
        fn changes_made_with_no_live_session_are_discarded_not_accumulated() {
            let fs = FakeFileSystem::new();
            fs.add_file("/loose.txt", b"");
            let mut model = Model::new();
            model.open_file_sync(&fs, "/loose.txt".into());

            for character in "hello world".chars() {
                model.type_char(character);
            }

            assert!(lsp_requests(&mut model).is_empty());
            assert!(model.active_buffer_mut().unwrap().take_pending_changes().is_empty());
        }

        #[test]
        fn a_file_changed_outside_the_editor_is_reported_as_a_watched_change() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);

            model.on_fs_event(FsEvent::Changed(vec!["/proj/src/main.rs".into()]));

            assert!(matches!(
                one_lsp_request(&mut model),
                LspRequest::WatchedFilesChanged { changes } if changes.len() == 1
            ));
        }

        #[test]
        fn a_superseded_request_is_cancelled_and_its_reply_ignored() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspHover));
            let first = hover_id(&lsp_requests(&mut model));

            model.dispatch(Command::Action(termesh_core::Action::LspHover));
            let requests = lsp_requests(&mut model);
            assert!(requests
                .iter()
                .any(|request| matches!(request, LspRequest::Cancel { id } if *id == first)));

            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Hover {
                    id: first,
                    hover: Some(termesh_core::HoverText {
                        text: "stale".into(),
                        range: None,
                        truncated: false,
                    }),
                },
            );
            assert!(!render_to_string(&mut model).contains("stale"));
        }

        #[test]
        fn a_document_no_session_claims_is_not_sent_anywhere() {
            let fs = sample_fs();
            fs.add_file("/proj/web/app.ts", b"const answer = 42;\n");
            let mut model = Model::new();
            model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
            let _ = lsp_requests(&mut model);

            model.open_file_sync(&fs, "/proj/web/app.ts".into());

            assert!(model
                .take_lsp_requests()
                .iter()
                .all(|(_, request)| !matches!(request, LspRequest::DidOpen { path, .. }
                if path.extension().and_then(|extension| extension.to_str()) == Some("ts"))));
        }

        #[test]
        fn a_missing_server_is_unavailable_not_merely_stale() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Failed {
                    id: None,
                    failure: termesh_core::LspFailure {
                        kind: termesh_core::LspFailureKind::NotInstalled,
                        message: "install rust-analyzer".into(),
                    },
                },
            );

            assert!(matches!(
                &model.lsp.sessions[&server].load,
                crate::lsp_state::LspLoadState::Unavailable(failure)
                    if failure.kind == termesh_core::LspFailureKind::NotInstalled
            ));
        }
    }

    mod lsp_diagnostics {
        use super::*;

        fn publish(model: &mut Model, severity: DiagnosticSeverity, line: u32, message: &str) {
            let server = rust_server(model);
            let path = std::path::PathBuf::from("/proj/src/main.rs");
            let version = model.lsp.sessions[&server].open_docs[&path];
            model.on_lsp_event(
                server,
                LspEvent::Diagnostics {
                    path: path.clone(),
                    version: Some(version),
                    items: vec![Diagnostic {
                        path,
                        range: TextRange {
                            start: TextPosition { line, character: 0 },
                            end: TextPosition { line, character: 2 },
                        },
                        severity,
                        origin: DiagnosticOrigin::LanguageServer,
                        source: "rust-analyzer".into(),
                        code: None,
                        message: message.into(),
                    }],
                },
            );
        }

        fn model_with_diagnostic(severity: DiagnosticSeverity) -> Model {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            publish(&mut model, severity, 0, "diagnostic");
            model
        }

        #[test]
        fn an_error_underlines_its_range_and_marks_the_gutter() {
            let frame = render_to_string(&mut model_with_diagnostic(DiagnosticSeverity::Error));
            assert!(frame.contains("1E"), "{frame}");
        }

        #[test]
        fn a_warning_uses_a_different_marker_from_an_error() {
            let frame = render_to_string(&mut model_with_diagnostic(DiagnosticSeverity::Warning));
            assert!(frame.contains("1W"), "{frame}");
            assert!(!frame.contains("1E"), "{frame}");
        }

        #[test]
        fn a_hunk_conflict_still_outranks_a_diagnostic_in_the_gutter() {
            let mut model = model_with_diagnostic(DiagnosticSeverity::Error);
            model.active_buffer_mut().unwrap().decorations_mut().push(
                termesh_editor::Decoration::new(
                    0,
                    2,
                    termesh_editor::DecorationClass::Hunk {
                        proposal: termesh_core::ProposalId::new(1),
                        side: termesh_editor::HunkSide::Removed,
                        state: termesh_editor::HunkState::Conflicted(
                            termesh_editor::ConflictReason::AnchorDeleted,
                        ),
                    },
                ),
            );

            let frame = render_to_string(&mut model);
            assert!(frame.contains("1!"), "{frame}");
        }

        #[test]
        fn info_and_hint_no_longer_render_as_warnings() {
            let info = render_to_string(&mut model_with_diagnostic(DiagnosticSeverity::Info));
            assert!(info.contains("1I"), "{info}");
            assert!(!info.contains("1W"), "{info}");

            let hint = render_to_string(&mut model_with_diagnostic(DiagnosticSeverity::Hint));
            assert!(hint.contains("1H"), "{hint}");
            assert!(!hint.contains("1W"), "{hint}");
        }

        #[test]
        fn editing_across_a_diagnostic_drops_it_rather_than_rebasing_it() {
            let mut model = model_with_diagnostic(DiagnosticSeverity::Error);
            model.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(1));

            model.type_char('x');

            assert_eq!(
                model
                    .active_buffer()
                    .unwrap()
                    .decorations()
                    .iter()
                    .filter(|decoration| matches!(
                        decoration.class,
                        termesh_editor::DecorationClass::Diagnostic(_)
                    ))
                    .count(),
                0
            );
        }

        #[test]
        fn diagnostics_for_a_superseded_document_version_are_ignored() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            let path = std::path::PathBuf::from("/proj/src/main.rs");
            let stale = model.lsp.sessions[&server].open_docs[&path] + 1;

            model.on_lsp_event(
                server,
                LspEvent::Diagnostics {
                    path: path.clone(),
                    version: Some(stale),
                    items: vec![Diagnostic {
                        path: path.clone(),
                        range: TextRange {
                            start: TextPosition { line: 0, character: 0 },
                            end: TextPosition { line: 0, character: 2 },
                        },
                        severity: DiagnosticSeverity::Error,
                        origin: DiagnosticOrigin::LanguageServer,
                        source: "rust-analyzer".into(),
                        code: None,
                        message: "stale".into(),
                    }],
                },
            );

            assert!(!model.lsp.diagnostics.contains_key(&path));
        }

        #[test]
        fn the_panel_lists_both_sources_and_tags_each_row() {
            let (mut model, _, _) = running_cargo_test();
            model.task_runs.last_mut().unwrap().problems = vec![problem("/proj/src/lib.rs", 7, 2)];
            model.open_file_sync(&sample_fs(), "/proj/src/main.rs".into());
            publish(&mut model, DiagnosticSeverity::Warning, 0, "server warning");

            model.dispatch(Command::Action(termesh_core::Action::ProblemsShow));

            let frame = render_to_string(&mut model);
            assert!(frame.contains("[rust-analyzer]"), "{frame}");
            assert!(frame.contains("[cargo]"), "{frame}");
        }

        #[test]
        fn the_same_diagnostic_from_both_sources_appears_once() {
            let (mut model, _, _) = running_cargo_test();
            model.task_runs.last_mut().unwrap().problems = vec![termesh_core::Problem {
                path: "/proj/src/main.rs".into(),
                line: 41,
                column: 1,
                severity: termesh_core::ProblemSeverity::Error,
                message: "mismatched   types".into(),
            }];
            model.open_file_sync(&sample_fs(), "/proj/src/main.rs".into());
            publish(&mut model, DiagnosticSeverity::Error, 40, "mismatched types");

            let rows = model.problem_rows();

            assert_eq!(rows.len(), 1, "{rows:#?}");
            assert_eq!(rows[0].origin, DiagnosticOrigin::LanguageServer);
            assert_eq!(rows[0].line, 41);
        }

        #[test]
        fn problem_navigation_still_opens_and_positions_a_buffer() {
            let mut model = model_with_diagnostic(DiagnosticSeverity::Error);
            model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
            let (request, path) = take_resolve(&mut model);
            assert_eq!(path, std::path::Path::new("/proj/src/main.rs"));

            model.on_fs_event(FsEvent::PathResolved { request, path });

            assert_eq!(model.active_buffer().unwrap().cursor_position(), (0, 0));
        }

        #[test]
        fn the_status_bar_error_count_reads_correctly_at_one() {
            let mut model = Model::new();
            assert!(render_to_string(&mut model).contains("0 errors"));

            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            let version =
                model.lsp.sessions[&server].open_docs[std::path::Path::new("/proj/src/main.rs")];
            model.on_lsp_event(
                server,
                LspEvent::Diagnostics {
                    path: "/proj/src/main.rs".into(),
                    version: Some(version),
                    items: vec![termesh_core::Diagnostic {
                        path: "/proj/src/main.rs".into(),
                        range: termesh_core::TextRange {
                            start: termesh_core::TextPosition { line: 0, character: 0 },
                            end: termesh_core::TextPosition { line: 0, character: 2 },
                        },
                        severity: termesh_core::DiagnosticSeverity::Error,
                        origin: termesh_core::DiagnosticOrigin::LanguageServer,
                        source: "rust-analyzer".into(),
                        code: None,
                        message: "mismatched types".into(),
                    }],
                },
            );
            let frame = render_to_string(&mut model);
            assert!(frame.contains("1 error"), "{frame}");
            assert!(!frame.contains("1 errors"), "{frame}");
        }

        #[test]
        fn indexing_and_unavailable_language_states_reach_the_status_bar() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Indexing { message: "indexing crates".into(), percent: Some(37) },
            );
            assert!(render_to_string(&mut model).contains("LSP indexing crates 37%"));

            model.on_lsp_event(server, LspEvent::Unavailable { message: "server missing".into() });
            assert!(render_to_string(&mut model).contains("LSP rust unavailable"));
        }
    }

    mod lsp_navigation {
        use super::*;

        fn location(path: &str, line: u32) -> termesh_core::Location {
            termesh_core::Location {
                path: path.into(),
                range: TextRange {
                    start: TextPosition { line, character: 0 },
                    end: TextPosition { line, character: 1 },
                },
            }
        }

        #[test]
        fn go_to_definition_asks_at_the_cursor_and_opens_the_result() {
            let (mut model, fs) = opened_rust_file(b"fn main() {}\n");
            fs.add_file("/proj/src/lib.rs", b"pub fn target() {}\n");
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(termesh_core::Action::EditorGotoDefinition));
            let request = one_lsp_request(&mut model);
            let LspRequest::Definition { id, path, position } = request else {
                panic!("expected definition, got {request:#?}")
            };
            assert_eq!(path, std::path::Path::new("/proj/src/main.rs"));
            assert_eq!(position, TextPosition { line: 0, character: 0 });

            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Definition { id, locations: vec![location("/proj/src/lib.rs", 0)] },
            );
            model.settle_fs_sync(&mut termesh_filesystem::DirReader::new(
                &fs,
                std::path::Path::new("/proj"),
                IgnoreOptions::default(),
            ));
            assert_eq!(
                model.active_buffer().unwrap().path(),
                Some(std::path::Path::new("/proj/src/lib.rs"))
            );
        }

        #[test]
        fn no_definition_reports_rather_than_doing_nothing() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::EditorGotoDefinition));
            let LspRequest::Definition { id, .. } = one_lsp_request(&mut model) else {
                panic!("definition request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(server, LspEvent::Definition { id, locations: Vec::new() });
            assert!(model
                .notification
                .as_deref()
                .is_some_and(|message| message.contains("definition")));
        }

        #[test]
        fn hover_renders_as_a_cursor_anchored_tooltip() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspHover));
            let id = hover_id(&lsp_requests(&mut model));
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Hover {
                    id,
                    hover: Some(termesh_core::HoverText {
                        text: "fn main() -> ()".into(),
                        range: None,
                        truncated: false,
                    }),
                },
            );
            let frame = render_to_string(&mut model);
            assert!(frame.contains("fn main() -> ()"), "{frame}");
        }

        #[test]
        fn references_open_an_overlay_and_enter_jumps_to_the_selection() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspReferences));
            let LspRequest::References { id, .. } = one_lsp_request(&mut model) else {
                panic!("references request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::References { id, locations: vec![location("/proj/src/main.rs", 0)] },
            );
            assert!(render_to_string(&mut model).contains("References"));
            input::on_chord(&mut model, KeyChord::plain(Key::Enter));
            assert_eq!(model.active_buffer().unwrap().cursor_position(), (0, 0));
        }

        #[test]
        fn document_symbols_render_their_hierarchy() {
            let (mut model, _) = opened_rust_file(b"struct Parent;\nfn child() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspDocumentSymbols));
            let LspRequest::DocumentSymbols { id, .. } = one_lsp_request(&mut model) else {
                panic!("document symbols request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::DocumentSymbols {
                    id,
                    symbols: vec![termesh_core::DocumentSymbol {
                        name: "Parent".into(),
                        kind: termesh_core::SymbolKind::Struct,
                        detail: None,
                        range: TextRange {
                            start: TextPosition { line: 0, character: 0 },
                            end: TextPosition { line: 1, character: 0 },
                        },
                        children: vec![termesh_core::DocumentSymbol {
                            name: "child".into(),
                            kind: termesh_core::SymbolKind::Function,
                            detail: None,
                            range: TextRange {
                                start: TextPosition { line: 1, character: 0 },
                                end: TextPosition { line: 1, character: 2 },
                            },
                            children: Vec::new(),
                        }],
                    }],
                },
            );
            let frame = render_to_string(&mut model);
            assert!(frame.contains("Parent"), "{frame}");
            assert!(frame.contains("  child"), "{frame}");
        }

        #[test]
        fn escaping_an_overlay_restores_the_previous_focus() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            model.focus = Pane::Agent;
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspHover));
            let id = hover_id(&lsp_requests(&mut model));
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Hover {
                    id,
                    hover: Some(termesh_core::HoverText {
                        text: "hover".into(),
                        range: None,
                        truncated: false,
                    }),
                },
            );
            model.focus = Pane::Editor;
            input::on_chord(&mut model, KeyChord::plain(Key::Esc));
            assert_eq!(model.focus, Pane::Agent);
        }

        #[test]
        fn workspace_symbols_query_every_session_not_just_the_first() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let second = LspServerId::new(2);
            model.lsp.sessions.insert(
                second,
                crate::lsp_state::LspSessionState::new(
                    second,
                    "typescript".into(),
                    vec!["ts".into()],
                    crate::lsp_state::SessionLaunch {
                        root: "/proj".into(),
                        command: vec!["typescript-language-server".into()],
                        initialization_options: None,
                    },
                ),
            );
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(termesh_core::Action::LspWorkspaceSymbols));

            let targets: Vec<_> = model
                .take_lsp_requests()
                .into_iter()
                .filter(|(_, request)| matches!(request, LspRequest::WorkspaceSymbols { .. }))
                .map(|(server, _)| server)
                .collect();
            assert_eq!(targets.len(), 2);
        }
    }

    mod lsp_edits {
        use super::*;

        fn formatting_edit(start: u32, end: u32, text: &str) -> termesh_core::TextEdit {
            termesh_core::TextEdit {
                path: "/proj/src/main.rs".into(),
                range: TextRange {
                    start: TextPosition { line: 0, character: start },
                    end: TextPosition { line: 0, character: end },
                },
                new_text: text.into(),
            }
        }

        fn formatting_request(model: &mut Model) -> LspRequestId {
            model.dispatch(Command::Action(termesh_core::Action::LspFormat));
            let requests = lsp_requests(model);
            let formatting: Vec<_> = requests
                .into_iter()
                .filter_map(|request| match request {
                    LspRequest::Formatting { id, .. } => Some(id),
                    _ => None,
                })
                .collect();
            assert_eq!(formatting.len(), 1, "{formatting:#?}");
            formatting[0]
        }

        #[test]
        fn formatting_applies_as_one_transaction_and_one_undo_step() {
            let (mut model, _) = opened_rust_file(b"fn main(){}\n");
            let _ = lsp_requests(&mut model);
            let before = model.active_buffer().unwrap().text().to_string();
            let id = formatting_request(&mut model);
            let server = rust_server(&model);

            model.on_lsp_event(
                server,
                LspEvent::Formatting { id, edits: vec![formatting_edit(9, 9, " ")] },
            );

            assert_ne!(model.active_buffer().unwrap().text().to_string(), before);
            model.dispatch(Command::EditorUndo);
            assert_eq!(model.active_buffer().unwrap().text().to_string(), before);
        }

        #[test]
        fn a_formatting_edit_never_touches_the_filesystem() {
            let (mut model, _) = opened_rust_file(b"fn main(){}\n");
            let _ = lsp_requests(&mut model);
            let id = formatting_request(&mut model);
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Formatting { id, edits: vec![formatting_edit(9, 9, " ")] },
            );
            assert!(model.active_buffer().unwrap().is_dirty());
            assert!(model.take_fs_requests().is_empty());
        }

        #[test]
        fn overlapping_edits_are_refused_without_mutating_the_buffer() {
            let (mut model, _) = opened_rust_file(b"fn main(){}\n");
            let _ = lsp_requests(&mut model);
            let before = model.active_buffer().unwrap().text().to_string();
            let id = formatting_request(&mut model);
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Formatting {
                    id,
                    edits: vec![formatting_edit(3, 8, "one"), formatting_edit(6, 9, "two")],
                },
            );
            assert_eq!(model.active_buffer().unwrap().text().to_string(), before);
            assert!(model
                .notification
                .as_deref()
                .is_some_and(|message| message.contains("overlap")));
        }

        #[test]
        fn accepting_a_completion_inserts_the_servers_text_not_the_label() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspCompletion));
            let LspRequest::Completion { id, .. } = one_lsp_request(&mut model) else {
                panic!("completion request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::Completion {
                    id,
                    items: vec![termesh_core::CompletionItem {
                        label: "display-label".into(),
                        detail: None,
                        kind: termesh_core::SymbolKind::Function,
                        insert_text: "server_text".into(),
                        edit: None,
                    }],
                },
            );

            input::on_chord(&mut model, KeyChord::plain(Key::Enter));

            let text = model.active_buffer().unwrap().text().to_string();
            assert!(text.starts_with("server_text"), "{text}");
            assert!(!text.contains("display-label"), "{text}");
        }

        #[test]
        fn a_formatting_reply_for_a_superseded_request_is_ignored() {
            let (mut model, _) = opened_rust_file(b"fn main(){}\n");
            let _ = lsp_requests(&mut model);
            let before = model.active_buffer().unwrap().text().to_string();
            let first = formatting_request(&mut model);
            let _second = formatting_request(&mut model);
            let server = rust_server(&model);

            model.on_lsp_event(
                server,
                LspEvent::Formatting { id: first, edits: vec![formatting_edit(9, 9, "stale")] },
            );

            assert_eq!(model.active_buffer().unwrap().text().to_string(), before);
        }
    }

    mod lsp_workspace_edits {
        use super::*;

        fn edit(path: &str, start: u32, end: u32, new_text: &str) -> TextEdit {
            TextEdit {
                path: path.into(),
                range: TextRange {
                    start: TextPosition { line: 0, character: start },
                    end: TextPosition { line: 0, character: end },
                },
                new_text: new_text.into(),
            }
        }

        fn two_file_edit() -> WorkspaceEdit {
            WorkspaceEdit {
                edits: vec![
                    edit("/proj/src/main.rs", 3, 7, "entry"),
                    edit("/proj/src/lib.rs", 7, 10, "new"),
                ],
                versions: Vec::new(),
            }
        }

        fn request_rename(model: &mut Model, new_name: &str) -> LspRequestId {
            model.dispatch(Command::Action(termesh_core::Action::LspRename));
            let Some(crate::model::Overlay::Prompt(mut prompt)) = model.overlays.pop() else {
                panic!("rename prompt")
            };
            prompt.input = new_name.into();
            model.confirm_prompt(prompt);
            let requests = lsp_requests(model);
            requests
                .into_iter()
                .find_map(|request| match request {
                    LspRequest::Rename { id, new_name: actual, .. } => {
                        assert_eq!(actual, new_name);
                        Some(id)
                    }
                    _ => None,
                })
                .expect("rename request")
        }

        #[test]
        fn rename_prompts_then_asks_the_server_at_the_cursor() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);

            model.dispatch(Command::Action(termesh_core::Action::LspRename));
            let Some(crate::model::Overlay::Prompt(mut prompt)) = model.overlays.pop() else {
                panic!("rename prompt")
            };
            assert_eq!(prompt.kind, crate::model::PromptKind::LspRename);
            prompt.input = "renamed".into();
            model.confirm_prompt(prompt);

            let LspRequest::Rename { new_name, position, .. } = one_lsp_request(&mut model) else {
                panic!("rename request")
            };
            assert_eq!(new_name, "renamed");
            assert_eq!(position, TextPosition { line: 0, character: 0 });
        }

        #[test]
        fn a_workspace_edit_opens_every_target_before_applying() {
            let (mut model, fs) = opened_rust_file(b"fn main() {}\n");
            fs.add_file("/proj/src/lib.rs", b"pub fn old() {}\n");
            let _ = lsp_requests(&mut model);
            let id = request_rename(&mut model, "entry");
            let server = rust_server(&model);

            model.on_lsp_event(server, LspEvent::Rename { id, edit: two_file_edit() });

            let reads = model.take_fs_requests();
            assert!(reads.iter().any(|request| matches!(request, FsRequest::ReadFile { path, .. }
                    if path == std::path::Path::new("/proj/src/lib.rs"))));
            assert_eq!(model.buffers.len(), 1, "nothing applies until every file is loaded");

            for request in reads {
                let FsRequest::ReadFile { buffer, path } = request else { continue };
                let contents = fs.read_file(&path).unwrap();
                model.on_fs_event(FsEvent::FileLoaded { buffer, path, contents });
            }
            assert_eq!(model.buffers.len(), 2);
            assert!(model
                .buffers
                .iter()
                .any(|buffer| buffer.text().to_string().contains("fn entry")));
            assert!(model
                .buffers
                .iter()
                .any(|buffer| buffer.text().to_string().contains("fn new")));
        }

        #[test]
        fn a_rename_undoes_per_file() {
            let (mut model, fs) = opened_rust_file(b"fn main() {}\n");
            fs.add_file("/proj/src/lib.rs", b"pub fn old() {}\n");
            let _ = lsp_requests(&mut model);
            model.apply_workspace_edit(two_file_edit());
            model.settle_fs_sync(&mut reader(&fs));

            for buffer in &mut model.buffers {
                assert!(buffer.undo(), "each touched file gets its own undo transaction");
            }
            assert!(model
                .buffers
                .iter()
                .any(|buffer| buffer.text().to_string().contains("fn main")));
            assert!(model
                .buffers
                .iter()
                .any(|buffer| buffer.text().to_string().contains("fn old")));
        }

        #[test]
        fn a_workspace_edit_with_a_stale_document_version_is_refused() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            let server = rust_server(&model);
            let old_version =
                model.lsp.sessions[&server].open_docs[std::path::Path::new("/proj/src/main.rs")];
            let id = request_rename(&mut model, "entry");
            model.type_char('x');
            let before_rename = model.active_buffer().unwrap().text().to_string();

            model.on_lsp_event(
                server,
                LspEvent::Rename {
                    id,
                    edit: WorkspaceEdit {
                        edits: vec![edit("/proj/src/main.rs", 3, 7, "entry")],
                        versions: vec![("/proj/src/main.rs".into(), old_version)],
                    },
                },
            );

            assert_eq!(model.active_buffer().unwrap().text().to_string(), before_rename);
            assert!(model
                .notification
                .as_deref()
                .is_some_and(|message| message.contains("changed")));
        }

        #[test]
        fn a_failed_read_abandons_the_edit_without_partly_applying_it() {
            let (mut model, fs) = opened_rust_file(b"fn main() {}\n");
            let path = std::path::PathBuf::from("/proj/src/lib.rs");
            fs.add_file(&path, b"pub fn old() {}\n");
            fs.fail(&path, termesh_core::FsError::PermissionDenied(path.clone()));
            let _ = lsp_requests(&mut model);
            let before = model.active_buffer().unwrap().text().to_string();

            model.apply_workspace_edit(two_file_edit());
            model.settle_fs_sync(&mut reader(&fs));

            assert_eq!(model.active_buffer().unwrap().text().to_string(), before);
            assert!(model.notification.is_some());
        }

        #[test]
        fn a_quick_fix_applies_the_action_the_user_selected() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspCodeAction));
            let LspRequest::CodeActions { id, .. } = one_lsp_request(&mut model) else {
                panic!("code-actions request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::CodeActions {
                    id,
                    actions: vec![
                        CodeAction { title: "No edit".into(), kind: None, edit: None },
                        CodeAction {
                            title: "Insert marker".into(),
                            kind: Some("quickfix".into()),
                            edit: Some(WorkspaceEdit {
                                edits: vec![edit("/proj/src/main.rs", 0, 0, "// fixed\n")],
                                versions: Vec::new(),
                            }),
                        },
                    ],
                },
            );
            input::on_chord(&mut model, KeyChord::plain(Key::Down));
            input::on_chord(&mut model, KeyChord::plain(Key::Enter));
            assert!(model.active_buffer().unwrap().text().to_string().starts_with("// fixed"));
        }

        #[test]
        fn the_result_reports_how_many_files_changed() {
            let (mut model, fs) = opened_rust_file(b"fn main() {}\n");
            fs.add_file("/proj/src/lib.rs", b"pub fn old() {}\n");
            let _ = lsp_requests(&mut model);
            model.apply_workspace_edit(two_file_edit());
            model.settle_fs_sync(&mut reader(&fs));
            assert!(model
                .notification
                .as_deref()
                .is_some_and(|message| message.contains("2 files")));
        }
    }

    mod lsp_agent_context {
        use super::*;

        fn diagnostic(path: &str, severity: DiagnosticSeverity, message: String) -> Diagnostic {
            Diagnostic {
                path: path.into(),
                range: TextRange {
                    start: TextPosition { line: 0, character: 0 },
                    end: TextPosition { line: 0, character: 2 },
                },
                severity,
                origin: DiagnosticOrigin::LanguageServer,
                source: "rust-analyzer".into(),
                code: None,
                message,
            }
        }

        fn publish(model: &mut Model, items: Vec<Diagnostic>) {
            let server = rust_server(model);
            let version = model.lsp.sessions[&server]
                .open_docs
                .get(std::path::Path::new("/proj/src/main.rs"))
                .copied();
            model.on_lsp_event(
                server,
                LspEvent::Diagnostics { path: "/proj/src/main.rs".into(), version, items },
            );
        }

        #[test]
        fn the_agent_sees_the_same_diagnostics_the_human_sees() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            publish(
                &mut model,
                vec![diagnostic(
                    "/proj/src/main.rs",
                    DiagnosticSeverity::Error,
                    "mismatched types".into(),
                )],
            );
            let context = model.agent_context();
            for row in model
                .problem_rows()
                .iter()
                .filter(|row| row.origin == DiagnosticOrigin::LanguageServer)
            {
                assert!(context.contains(&row.message), "{context}");
            }
        }

        #[test]
        fn the_agent_context_includes_the_active_document_outline() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let _ = lsp_requests(&mut model);
            model.dispatch(Command::Action(termesh_core::Action::LspDocumentSymbols));
            let LspRequest::DocumentSymbols { id, .. } = one_lsp_request(&mut model) else {
                panic!("document symbols request")
            };
            let server = rust_server(&model);
            model.on_lsp_event(
                server,
                LspEvent::DocumentSymbols {
                    id,
                    symbols: vec![termesh_core::DocumentSymbol {
                        name: "main".into(),
                        kind: termesh_core::SymbolKind::Function,
                        detail: None,
                        range: TextRange {
                            start: TextPosition { line: 0, character: 0 },
                            end: TextPosition { line: 0, character: 12 },
                        },
                        children: Vec::new(),
                    }],
                },
            );
            assert!(model.agent_context().contains("fn main"), "{}", model.agent_context());
        }

        #[test]
        fn language_context_is_bounded_and_marks_truncation() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            let items = (0..4_000)
                .map(|index| {
                    diagnostic(
                        "/proj/src/main.rs",
                        DiagnosticSeverity::Warning,
                        format!("warning {index}: {}", "x".repeat(48)),
                    )
                })
                .collect();
            publish(&mut model, items);
            let context = model.lsp_agent_context();
            assert!(context.len() <= 64 * 1024, "{} bytes", context.len());
            assert!(context.contains("[language context truncated]"), "{context}");
        }

        #[test]
        fn errors_are_listed_before_warnings() {
            let (mut model, _) = opened_rust_file(b"fn main() {}\n");
            publish(
                &mut model,
                vec![
                    diagnostic(
                        "/proj/src/main.rs",
                        DiagnosticSeverity::Warning,
                        "warning comes later".into(),
                    ),
                    diagnostic(
                        "/proj/src/main.rs",
                        DiagnosticSeverity::Error,
                        "error comes first".into(),
                    ),
                ],
            );
            let context = model.lsp_agent_context();
            assert!(
                context.find("error comes first") < context.find("warning comes later"),
                "{context}"
            );
        }

        #[test]
        fn a_workspace_with_no_language_server_says_so_rather_than_lying() {
            let context = Model::new().lsp_agent_context();
            assert!(context.contains("no language server"), "{context}");
        }
    }

    fn git_snapshot() -> GitRepositorySnapshot {
        GitRepositorySnapshot {
            repository_root: "/proj".into(),
            workspace_root: "/proj".into(),
            branch: GitBranchStatus::default(),
            files: Vec::new(),
            context_diff: GitContextDiff::default(),
        }
    }

    fn git_request_id(request: &GitRequest) -> GitRequestId {
        match request {
            GitRequest::Refresh { id, .. }
            | GitRequest::Diff { id, .. }
            | GitRequest::Branches { id, .. }
            | GitRequest::Execute { id, .. } => *id,
        }
    }

    #[test]
    fn opening_a_workspace_queues_one_git_refresh() {
        let (mut model, _) = opened();
        assert!(matches!(
            model.take_git_requests().as_slice(),
            [GitRequest::Refresh { root, .. }] if root == std::path::Path::new("/proj")
        ));
    }

    #[test]
    fn git_refresh_triggers_coalesce_while_one_is_active() {
        let (mut model, _) = opened();
        let first = model.take_git_requests();
        model.on_fs_event(FsEvent::Changed(vec!["/proj/src/main.rs".into()]));
        model.on_fs_event(FsEvent::Changed(vec!["/proj/README.md".into()]));
        assert!(model.take_git_requests().is_empty());

        let id = git_request_id(&first[0]);
        model.on_git_event(GitEvent::SnapshotLoaded { id, snapshot: git_snapshot() });
        assert_eq!(model.take_git_requests().len(), 1);
    }

    fn git_overlay_model() -> Model {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.branch.head = Some("main".into());
        snapshot.branch.ahead = 2;
        snapshot.branch.behind = 1;
        snapshot.files = vec![
            GitFileStatus {
                path: "conflict.rs".into(),
                index: Some(GitChangeKind::Conflicted),
                worktree: Some(GitChangeKind::Conflicted),
            },
            GitFileStatus {
                path: "staged.rs".into(),
                index: Some(GitChangeKind::Modified),
                worktree: None,
            },
            GitFileStatus {
                path: "worktree.rs".into(),
                index: None,
                worktree: Some(GitChangeKind::Modified),
            },
        ];
        model.on_git_event(GitEvent::SnapshotLoaded {
            id: git_request_id(&refresh[0]),
            snapshot: snapshot.clone(),
        });
        model.dispatch(Command::Action(termesh_core::Action::GitShow));
        // `git.show` is also the explicit-refresh surface (ADR-0010 §2). Answer that
        // request here so each test below asserts on the one it actually triggers, and so
        // later refreshes are not coalesced behind an in-flight one.
        let shown = model.take_git_requests();
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&shown[0]), snapshot });
        model
    }

    #[test]
    fn showing_git_changes_also_requests_a_fresh_snapshot() {
        let mut model = git_overlay_model();
        model.dispatch(Command::Action(termesh_core::Action::GitShow));
        assert!(matches!(model.take_git_requests().as_slice(), [GitRequest::Refresh { .. }]));
    }

    #[test]
    fn reshowing_git_changes_refreshes_in_place_and_keeps_the_selection() {
        let mut model = git_overlay_model();
        // Select `staged.rs` (rows: conflict.rs, staged.rs, worktree.rs).
        input::on_chord(&mut model, KeyChord::plain(Key::Down));
        let overlays = model.overlays.len();

        model.dispatch(Command::Action(termesh_core::Action::GitShow));
        let snapshot = model.git.snapshot.clone().unwrap();
        let id = git_request_id(&model.take_git_requests()[0]);
        model.on_git_event(GitEvent::SnapshotLoaded { id, snapshot });

        assert_eq!(model.overlays.len(), overlays, "re-showing must not stack overlays");
        input::on_chord(&mut model, KeyChord::plain(Key::Char('u')));
        assert!(matches!(
            model.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Unstage { path },
                ..
            }] if path == std::path::Path::new("staged.rs")
        ));
    }

    #[test]
    fn a_background_refresh_keeps_the_selection_on_the_same_file() {
        let mut model = git_overlay_model();
        // Select `staged.rs` (rows: conflict.rs, staged.rs, worktree.rs).
        input::on_chord(&mut model, KeyChord::plain(Key::Down));

        // A watcher-driven refresh inserts a row *above* the selection. An index-only
        // clamp would slide the cursor onto `alpha.rs` and stage the wrong path.
        let mut snapshot = git_snapshot();
        snapshot.files = vec![
            GitFileStatus {
                path: "alpha.rs".into(),
                index: Some(GitChangeKind::Modified),
                worktree: None,
            },
            GitFileStatus {
                path: "staged.rs".into(),
                index: Some(GitChangeKind::Modified),
                worktree: None,
            },
        ];
        model.request_git_refresh();
        let id = git_request_id(&model.take_git_requests()[0]);
        model.on_git_event(GitEvent::SnapshotLoaded { id, snapshot });

        input::on_chord(&mut model, KeyChord::plain(Key::Char('u')));
        assert!(matches!(
            model.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Unstage { path },
                ..
            }] if path == std::path::Path::new("staged.rs")
        ));
    }

    #[test]
    fn an_untracked_row_explains_itself_instead_of_showing_an_empty_diff() {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.files.push(GitFileStatus {
            path: "brand-new.rs".into(),
            index: None,
            worktree: Some(GitChangeKind::Untracked),
        });
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&refresh[0]), snapshot });
        model.dispatch(Command::Action(termesh_core::Action::GitShow));
        model.take_git_requests();

        input::on_chord(&mut model, KeyChord::plain(Key::Enter));

        assert!(model.take_git_requests().is_empty(), "an untracked path has nothing to diff");
        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("Untracked"), "{frame}");
        assert!(!frame.contains("binary or unchanged"), "{frame}");
    }

    #[test]
    fn conflicts_block_the_commit_prompt_with_a_reason() {
        let mut model = git_overlay_model();
        model.dispatch(Command::Action(termesh_core::Action::GitCommit));
        assert!(model.notification.as_deref().unwrap().contains("Resolve"));
        assert!(!model.overlays.iter().any(|overlay| matches!(overlay, Overlay::Prompt(_))));
    }

    #[test]
    fn a_rename_row_shows_both_paths() {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.files.push(GitFileStatus {
            path: "src/new.rs".into(),
            index: Some(GitChangeKind::Renamed { from: "src/old.rs".into() }),
            worktree: None,
        });
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&refresh[0]), snapshot });
        model.dispatch(Command::Action(termesh_core::Action::GitShow));

        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("src/new.rs ← src/old.rs"), "{frame}");
    }

    #[test]
    fn git_overlay_renders_status_groups_and_two_column_states() {
        let mut model = git_overlay_model();
        let frame = view::snapshot(&mut model, 96, 28);
        for expected in [
            "Git Changes",
            "Conflicts",
            "UU conflict.rs",
            "Staged",
            "M  staged.rs",
            "Changes",
            " M worktree.rs",
        ] {
            assert!(frame.contains(expected), "missing {expected:?}:\n{frame}");
        }
        assert!(frame.contains("branch: main ↑2 ↓1 ~3 !1"), "{frame}");
    }

    #[test]
    fn git_overlay_cached_changes_decorate_explorer_ancestors() {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.files.push(GitFileStatus {
            path: "src/main.rs".into(),
            index: None,
            worktree: Some(GitChangeKind::Modified),
        });
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&refresh[0]), snapshot });

        let frame = view::snapshot(&mut model, 96, 28);
        assert!(
            frame.contains("src  ~"),
            "directory should aggregate cached descendants:\n{frame}"
        );
    }

    #[test]
    fn git_overlay_renders_colored_diff_target_and_truncation_at_narrow_width() {
        let mut model = git_overlay_model();
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));
        let request = model.take_git_requests().pop().unwrap();
        let GitRequest::Diff { id, path, target, .. } = request else {
            panic!("expected a diff request");
        };
        model.on_git_event(GitEvent::DiffLoaded {
            id,
            diff: GitFileDiff {
                path,
                target,
                text: "@@ -1 +1 @@\n-removed\n+added\n".into(),
                truncated: true,
            },
        });

        for width in [96, 72] {
            let frame = view::snapshot(&mut model, width, 28);
            for expected in ["Git Diff", "worktree", "@@", "-removed", "+added", "truncated"] {
                assert!(frame.contains(expected), "width {width}, missing {expected:?}:\n{frame}");
            }
        }
        assert_eq!(target, GitDiffTarget::Worktree);
    }

    #[test]
    fn commit_refuses_an_empty_index() {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.files.push(GitFileStatus {
            path: "worktree.rs".into(),
            index: None,
            worktree: Some(GitChangeKind::Modified),
        });
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&refresh[0]), snapshot });

        model.dispatch(Command::Action(termesh_core::Action::GitCommit));

        assert!(model.notification.as_deref().unwrap().contains("nothing staged"));
        assert!(model.overlays.is_empty());
    }

    #[test]
    fn blank_commit_message_sends_no_git_request() {
        let mut model = git_overlay_model();
        model.confirm_prompt(Prompt {
            title: "Commit".into(),
            input: "   ".into(),
            kind: PromptKind::GitCommit,
        });
        assert!(model.take_git_requests().is_empty());
    }

    #[test]
    fn commit_request_contains_no_implicit_stage() {
        let mut model = git_overlay_model();
        model.confirm_prompt(Prompt {
            title: "Commit".into(),
            input: "ship it".into(),
            kind: PromptKind::GitCommit,
        });
        assert!(matches!(
            model.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Commit { message },
                ..
            }] if message == "ship it"
        ));
    }

    #[test]
    fn git_stage_and_unstage_use_only_the_selected_row_path() {
        let mut stage = git_overlay_model();
        input::on_chord(&mut stage, KeyChord::plain(Key::Down));
        input::on_chord(&mut stage, KeyChord::plain(Key::Down));
        input::on_chord(&mut stage, KeyChord::plain(Key::Char('s')));
        assert!(matches!(
            stage.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Stage { path },
                ..
            }] if path == std::path::Path::new("worktree.rs")
        ));

        let mut unstage = git_overlay_model();
        input::on_chord(&mut unstage, KeyChord::plain(Key::Down));
        input::on_chord(&mut unstage, KeyChord::plain(Key::Char('u')));
        assert!(matches!(
            unstage.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Unstage { path },
                ..
            }] if path == std::path::Path::new("staged.rs")
        ));
    }

    #[test]
    fn git_branch_selector_checks_out_exactly_the_selected_local_name() {
        let mut model = git_overlay_model();
        model.dispatch(Command::Action(termesh_core::Action::GitBranchCheckout));
        let request = model.take_git_requests().pop().unwrap();
        let GitRequest::Branches { id, .. } = request else {
            panic!("expected a branch-list request");
        };
        model.on_git_event(GitEvent::BranchesLoaded {
            id,
            branches: vec![
                GitBranch { name: "main".into(), current: true },
                GitBranch { name: "feature/git".into(), current: false },
            ],
        });
        input::on_chord(&mut model, KeyChord::plain(Key::Down));
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));

        assert!(matches!(
            model.take_git_requests().as_slice(),
            [GitRequest::Execute {
                operation: termesh_core::GitOperation::Checkout { branch },
                ..
            }] if branch == "feature/git"
        ));
        assert!(model.take_pty_requests().is_empty());
    }

    #[test]
    fn git_branch_remote_actions_use_the_git_worker_not_a_terminal() {
        for (action, expected) in [
            (termesh_core::Action::GitFetch, termesh_core::GitOperation::Fetch),
            (termesh_core::Action::GitPull, termesh_core::GitOperation::Pull),
            (termesh_core::Action::GitPush, termesh_core::GitOperation::Push),
        ] {
            let mut model = git_overlay_model();
            model.dispatch(Command::Action(action));
            let requests = model.take_git_requests();
            assert!(matches!(
                requests.as_slice(),
                [GitRequest::Execute { operation, .. }] if *operation == expected
            ));
            assert!(model.take_pty_requests().is_empty());
        }
    }

    #[test]
    fn git_branch_selector_and_operation_outcomes_are_visible() {
        let mut branches = git_overlay_model();
        branches.dispatch(Command::Action(termesh_core::Action::GitBranchCheckout));
        let GitRequest::Branches { id, .. } = branches.take_git_requests().pop().unwrap() else {
            panic!("expected branches request");
        };
        branches.on_git_event(GitEvent::BranchesLoaded {
            id,
            branches: vec![
                GitBranch { name: "main".into(), current: true },
                GitBranch { name: "feature/git".into(), current: false },
            ],
        });
        let frame = view::snapshot(&mut branches, 96, 28);
        assert!(frame.contains("Switch Branch"), "{frame}");
        assert!(frame.contains("* main"), "{frame}");
        assert!(frame.contains("feature/git"), "{frame}");

        for (action, message) in [
            (termesh_core::Action::GitPull, "Not possible to fast-forward"),
            (termesh_core::Action::GitPush, "Authentication failed"),
        ] {
            let mut model = git_overlay_model();
            model.dispatch(Command::Action(action));
            let id = git_request_id(&model.take_git_requests()[0]);
            model.on_git_event(GitEvent::Failed {
                id,
                operation_applied: false,
                failure: termesh_core::GitFailure {
                    kind: termesh_core::GitFailureKind::Command,
                    message: message.into(),
                },
            });
            assert!(model.notification.as_deref().is_some_and(|notice| notice.contains(message)));
        }
    }

    #[test]
    fn git_commit_success_replaces_snapshot_and_reports_summary() {
        let mut model = git_overlay_model();
        model.confirm_prompt(Prompt {
            title: "Commit".into(),
            input: "ship it".into(),
            kind: PromptKind::GitCommit,
        });
        let request = model.take_git_requests().pop().unwrap();
        let id = git_request_id(&request);
        let mut after = git_snapshot();
        after.branch.head = Some("main".into());
        model.on_git_event(GitEvent::OperationFinished {
            id,
            operation: termesh_core::GitOperation::Commit { message: "ship it".into() },
            message: "[main abc1234] ship it".into(),
            snapshot: after,
        });

        assert!(model.git.active_operation.is_none());
        assert!(model.git.snapshot.as_ref().unwrap().files.is_empty());
        assert_eq!(model.notification.as_deref(), Some("[main abc1234] ship it"));
    }

    fn model_with_git_agent_context(diff_size: usize) -> Model {
        let (mut model, _) = opened();
        let refresh = model.take_git_requests();
        let mut snapshot = git_snapshot();
        snapshot.branch.head = Some("main".into());
        snapshot.branch.upstream = Some("origin/main".into());
        snapshot.branch.ahead = 2;
        snapshot.branch.behind = 1;
        snapshot.files = vec![
            GitFileStatus {
                path: "src/staged.rs".into(),
                index: Some(GitChangeKind::Modified),
                worktree: None,
            },
            GitFileStatus {
                path: "src/worktree.rs".into(),
                index: None,
                worktree: Some(GitChangeKind::Modified),
            },
            GitFileStatus {
                path: "src/conflict.rs".into(),
                index: Some(GitChangeKind::Conflicted),
                worktree: Some(GitChangeKind::Conflicted),
            },
        ];
        snapshot.context_diff = GitContextDiff {
            index: format!("@@ staged @@\n{}", "s".repeat(diff_size)),
            worktree: format!("@@ worktree @@\n{}", "w".repeat(diff_size)),
            index_truncated: false,
            worktree_truncated: false,
        };
        model.on_git_event(GitEvent::SnapshotLoaded { id: git_request_id(&refresh[0]), snapshot });
        model
    }

    #[test]
    fn agent_context_uses_the_same_bounded_git_state_as_the_human() {
        let model = model_with_git_agent_context(8);
        let context = model.agent_context();
        assert!(
            context.contains("git: branch main; upstream origin/main; ahead 2; behind 1"),
            "{context}"
        );
        for expected in [
            "staged: src/staged.rs",
            "unstaged: src/worktree.rs",
            "conflicted: src/conflict.rs",
            "staged diff:\n@@ staged @@",
            "worktree diff:\n@@ worktree @@",
        ] {
            assert!(context.contains(expected), "missing {expected:?}:\n{context}");
        }
    }

    #[test]
    fn agent_git_context_is_bounded_and_marks_truncation() {
        let model = model_with_git_agent_context(80 * 1024);
        let context = model.git_agent_context();
        assert!(context.len() <= 64 * 1024, "{} bytes", context.len());
        assert!(context.contains("[git context truncated]"), "{context}");
    }

    #[test]
    fn file_open_action_requests_the_workspace_file_list() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::FileOpen));

        let request = model.take_search_requests().pop().unwrap();
        assert_eq!(request.mode, SearchMode::Files);
        assert_eq!(request.root, std::path::Path::new("/proj"));
        assert!(matches!(model.overlays.last(), Some(crate::model::Overlay::Search(search))
            if search.mode == SearchMode::Files));
    }

    fn deliver_quick_open(model: &mut Model, paths: &[&str]) {
        let request = model.take_search_requests().pop().unwrap();
        model.on_search_event(SearchEvent::Started { id: request.id });
        model.on_search_event(SearchEvent::Batch {
            id: request.id,
            matches: paths
                .iter()
                .map(|path| SearchMatch {
                    path: request.root.join(path),
                    line: None,
                    column: None,
                    text: None,
                })
                .collect(),
        });
        model.on_search_event(SearchEvent::Finished { id: request.id, truncated: false });
    }

    #[test]
    fn enter_on_quick_open_uses_the_normal_file_open_path() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::FileOpen));
        deliver_quick_open(&mut model, &["src/main.rs"]);

        input::on_chord(&mut model, KeyChord::plain(Key::Enter));

        assert!(matches!(model.take_fs_requests().as_slice(),
            [FsRequest::ReadFile { path, .. }] if path.ends_with("src/main.rs")));
    }

    #[test]
    fn quick_open_snapshot_is_ranked_and_discoverable() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::FileOpen));
        deliver_quick_open(&mut model, &["src/lib.rs", "src/main.rs", "README.md"]);
        for value in "sl".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }

        let frame = render_to_string(&mut model);
        assert!(frame.contains("Quick Open"), "{frame}");
        assert!(frame.contains("src/lib.rs"), "{frame}");
        assert!(frame.contains("▶"), "selected row should be marked:\n{frame}");
        assert!(frame.contains("Ctrl+P Files"), "{frame}");
    }

    fn active_search(model: &Model) -> &crate::search_state::SearchOverlay {
        match model.overlays.last() {
            Some(crate::model::Overlay::Search(search)) => search,
            _ => panic!("expected a search overlay"),
        }
    }

    #[test]
    fn open_buffer_matches_replace_disk_matches_for_that_path() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"fresh needle\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        model.dispatch(Command::Action(termesh_core::Action::WorkspaceSearch));
        for value in "needle".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }
        let request = model.take_search_requests().pop().unwrap();
        model.on_search_event(SearchEvent::Batch {
            id: request.id,
            matches: vec![SearchMatch {
                path: std::path::PathBuf::from("/proj/src/main.rs"),
                line: Some(99),
                column: Some(1),
                text: Some("stale needle".into()),
            }],
        });

        let rows = active_search(&model).visible_matches();
        assert_eq!((rows[0].line, rows[0].column), (Some(1), Some(7)));
        assert_eq!(rows.len(), 1, "the disk copy of an open buffer must be excluded");
    }

    #[test]
    fn late_preview_does_not_replace_the_newer_selection() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::WorkspaceSearch));
        for value in "needle".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }
        let request = model.take_search_requests().pop().unwrap();
        model.on_search_event(SearchEvent::Batch {
            id: request.id,
            matches: ["a.rs", "b.rs"]
                .into_iter()
                .map(|path| SearchMatch {
                    path: std::path::PathBuf::from("/proj").join(path),
                    line: Some(2),
                    column: Some(1),
                    text: Some("needle".into()),
                })
                .collect(),
        });
        let old = model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ReadPreview { request, .. } => Some(request),
                _ => None,
            })
            .unwrap();
        input::on_chord(&mut model, KeyChord::plain(Key::Down));
        let new = model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ReadPreview { request, .. } => Some(request),
                _ => None,
            })
            .unwrap();

        model.on_fs_event(FsEvent::PreviewLoaded {
            request: old,
            path: std::path::PathBuf::from("/proj/a.rs"),
            start_line: 1,
            text: "old".into(),
        });
        assert_ne!(active_search(&model).preview_text(), Some("old"));
        model.on_fs_event(FsEvent::PreviewLoaded {
            request: new,
            path: std::path::PathBuf::from("/proj/b.rs"),
            start_line: 1,
            text: "new".into(),
        });
        assert_eq!(active_search(&model).preview_text(), Some("new"));
    }

    #[test]
    fn workspace_search_enter_positions_an_open_buffer() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"one\ntwo needle\nthree\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        model.dispatch(Command::Action(termesh_core::Action::WorkspaceSearch));
        for value in "needle".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));
        assert_eq!(model.active_buffer().unwrap().cursor_position(), (1, 4));
    }

    #[test]
    fn loaded_search_result_receives_its_pending_location() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::WorkspaceSearch));
        for value in "needle".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }
        let search = model.take_search_requests().pop().unwrap();
        model.on_search_event(SearchEvent::Batch {
            id: search.id,
            matches: vec![SearchMatch {
                path: std::path::PathBuf::from("/proj/src/lib.rs"),
                line: Some(4),
                column: Some(3),
                text: Some("xxneedle".into()),
            }],
        });
        let _ = model.take_fs_requests(); // discard the preview read
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));
        let (buffer, path) = model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ReadFile { buffer, path } => Some((buffer, path)),
                _ => None,
            })
            .unwrap();
        model.on_fs_event(FsEvent::FileLoaded {
            buffer,
            path,
            contents: b"a\nb\nc\nxxneedle\n".to_vec(),
        });
        assert_eq!(model.active_buffer().unwrap().cursor_position(), (3, 2));
    }

    #[test]
    fn task_run_opens_the_cargo_catalog_for_a_rust_workspace() {
        let (mut model, _) = opened();
        input::on_chord(&mut model, KeyChord::plain(Key::F(5)));
        assert!(matches!(model.overlays.last(), Some(crate::model::Overlay::Tasks(picker))
            if picker.items().len() == 4));
        assert!(render_to_string(&mut model).contains("Run Task"));
    }

    #[test]
    fn declared_tasks_appear_alongside_cargo_tasks_in_the_picker() {
        let fs = sample_fs();
        fs.add_file(
            "/proj/.termesh/workspace.toml",
            br#"
                [tasks.smoke]
                label = "Smoke"
                program = "make"
                args = ["smoke"]
            "#,
        );
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        input::on_chord(&mut model, KeyChord::plain(Key::F(5)));

        let Some(crate::model::Overlay::Tasks(picker)) = model.overlays.last() else {
            panic!("task picker");
        };
        assert!(picker.items().iter().any(|task| task.id == "cargo.check"));
        assert!(picker.items().iter().any(|task| task.id == "workspace.smoke"));
    }

    fn running_cargo_test() -> (Model, termesh_core::TerminalId, termesh_core::TerminalGeneration) {
        let (mut model, _) = opened();
        input::on_chord(&mut model, KeyChord::plain(Key::F(5)));
        input::on_chord(&mut model, KeyChord::plain(Key::Down));
        input::on_chord(&mut model, KeyChord::plain(Key::Down));
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));
        let (terminal, generation) = model
            .take_pty_requests()
            .into_iter()
            .find_map(|request| match request {
                termesh_core::PtyRequest::Spawn { terminal, generation, spec, .. }
                    if spec.program == "cargo" && spec.args[0] == "test" =>
                {
                    Some((terminal, generation))
                }
                _ => None,
            })
            .unwrap();
        model.on_pty_event(termesh_core::PtyEvent::Spawned {
            terminal,
            generation,
            process_id: Some(42),
        });
        (model, terminal, generation)
    }

    /// Fill a running task terminal with more lines than its pane can show, so the
    /// early ones exist only in scrollback.
    fn terminal_with_scrollback(
    ) -> (Model, termesh_core::TerminalId, termesh_core::TerminalGeneration) {
        let (mut model, terminal, generation) = running_cargo_test();
        let mut output = Vec::new();
        for line in 0..120 {
            output.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        model.on_pty_event(termesh_core::PtyEvent::Output { terminal, generation, bytes: output });
        // Running a task already focuses the terminal, and `TerminalFocus` toggles —
        // dispatching it here would move focus away.
        assert_eq!(model.focus, termesh_ui::Pane::Terminal, "a task run focuses its terminal");
        (model, terminal, generation)
    }

    #[test]
    fn the_terminal_scrolls_back_while_a_task_is_still_running() {
        // Reported: `mvn test` fails and the failures have already scrolled past.
        // Copy mode is the only viewport movement, it has no default chord, and its
        // one route — the palette — is swallowed by a terminal that accepts input.
        let (mut model, ..) = terminal_with_scrollback();

        let bottom = render_to_string(&mut model);
        assert!(bottom.contains("line 119"), "the newest line is on screen:\n{bottom}");
        assert!(!bottom.contains("line 5 "), "an early line is only in scrollback:\n{bottom}");

        input::on_chord(&mut model, KeyChord::shift(Key::PageUp));

        let scrolled = render_to_string(&mut model);
        assert_ne!(scrolled, bottom, "Shift+PageUp moved nothing");
        assert!(!scrolled.contains("line 119"), "the view moved back:\n{scrolled}");
    }

    #[test]
    fn the_terminal_scrolls_back_after_the_task_has_exited() {
        // The other half of the report: once the process exits the terminal stops
        // accepting input, and arrows resolve to copy-mode commands that no-op.
        let (mut model, terminal, generation) = terminal_with_scrollback();
        model.on_pty_event(termesh_core::PtyEvent::Exited {
            terminal,
            generation,
            exit: termesh_core::TerminalExit { code: Some(1), signal: None },
        });

        let bottom = render_to_string(&mut model);
        input::on_chord(&mut model, KeyChord::shift(Key::PageUp));
        let scrolled = render_to_string(&mut model);
        assert_ne!(scrolled, bottom, "Shift+PageUp moved nothing after exit");
    }

    #[test]
    fn scrolling_back_and_forward_returns_to_the_newest_output() {
        let (mut model, ..) = terminal_with_scrollback();
        let bottom = render_to_string(&mut model);

        input::on_chord(&mut model, KeyChord::shift(Key::PageUp));
        input::on_chord(&mut model, KeyChord::shift(Key::PageDown));

        assert_eq!(render_to_string(&mut model), bottom);
    }

    #[test]
    fn a_running_shell_still_receives_the_keys_it_needs() {
        // The scroll chord must not eat input a program is waiting for: plain PageUp
        // and the arrows still reach the process.
        let (mut model, ..) = terminal_with_scrollback();
        let before = model.take_pty_requests().len();
        input::on_chord(&mut model, KeyChord::plain(Key::PageUp));
        input::on_chord(&mut model, KeyChord::plain(Key::Up));
        assert!(
            model.take_pty_requests().len() > before,
            "plain PageUp and Up still belong to the process"
        );
    }

    #[test]
    fn cargo_json_is_decoded_before_reaching_the_terminal() {
        let (mut model, terminal, generation) = running_cargo_test();
        let json = br#"{"reason":"compiler-message","message":{"rendered":"error[E0425]\n","level":"error","message":"cannot find value","spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true}]}}
"#;
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation,
            bytes: json.to_vec(),
        });
        let capture = model.active_terminal().unwrap().capture.as_str();
        assert!(capture.contains("error[E0425]"));
        assert!(!capture.contains("compiler-message"));
        assert_eq!(model.task_runs.last().unwrap().problems.len(), 1);
    }

    #[test]
    fn cancelling_a_running_task_queues_kill_and_retains_output() {
        let (mut model, terminal, generation) = running_cargo_test();
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation,
            bytes: b"running tests\n".to_vec(),
        });
        input::on_chord(&mut model, KeyChord::shift(Key::F(5)));
        assert_eq!(
            model.take_pty_requests(),
            vec![termesh_core::PtyRequest::Kill { terminal, generation }]
        );
        assert!(model.task_runs.last().unwrap().cancel_requested);
        assert!(model.active_terminal().unwrap().capture.as_str().contains("running tests"));
    }

    /// A human working through a failed build must not have the list yanked away
    /// because a *newer* run started — least of all one the agent started for itself.
    /// Problems survive until another run produces problems of its own (ADR-0009 §4).
    #[test]
    fn a_newer_problem_free_run_does_not_replace_the_failures_being_worked_through() {
        let (mut model, _, _) = running_cargo_test();
        model.task_runs.last_mut().unwrap().problems = vec![problem("/proj/src/lib.rs", 7, 2)];

        model.run_task(termesh_core::TaskSpec {
            id: "cargo.check".into(),
            label: "Check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--message-format=json-diagnostic-rendered-ansi".into()],
            cwd: "/proj".into(),
        });

        assert_eq!(model.task_runs.len(), 2, "the newer run is tracked");
        model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
        let (_, path) = take_resolve(&mut model);
        assert_eq!(path, std::path::Path::new("/proj/src/lib.rs"), "still the older failures");
    }

    /// The complement: once a newer run *does* find something, it takes over, and the
    /// cursor does not carry an index from the list that just stopped showing.
    #[test]
    fn a_newer_run_with_problems_takes_over_and_resets_the_cursor() {
        let (mut model, _, _) = running_cargo_test();
        model.task_runs.last_mut().unwrap().problems =
            vec![problem("/proj/src/old.rs", 7, 2), problem("/proj/src/old.rs", 9, 4)];
        model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
        let _ = take_resolve(&mut model);

        let (terminal, generation) = spawn_cargo_check(&mut model);
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation,
            bytes: compiler_message_json("src/new.rs", 3, 1),
        });

        model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
        let (_, path) = take_resolve(&mut model);
        assert_eq!(path, std::path::Path::new("/proj/src/new.rs"));
    }

    /// The status bar trades detail for width in fixed steps. A wide terminal gets the
    /// full readout; a narrow one keeps *which task* and *how it ended* and gives the
    /// rest of the room back to the hint strip. Both rungs are pinned here because only
    /// the narrow one is exercised incidentally by the headless demo.
    #[test]
    fn the_task_readout_sheds_detail_rather_than_crowding_out_the_hints() {
        let (mut model, _, _) = running_cargo_test();
        model.on_pty_event(termesh_core::PtyEvent::Exited {
            terminal: model.task_runs.last().unwrap().terminal,
            generation: model.terminals.last().unwrap().generation,
            exit: termesh_core::TerminalExit { code: Some(101), signal: None },
        });

        let wide = view::snapshot(&mut model, 180, 24);
        assert!(wide.contains("task #1 Test failed (human)"), "{wide}");

        let narrow = view::snapshot(&mut model, 96, 24);
        assert!(narrow.contains("Test failed"), "the task and its outcome survive: {narrow}");
        assert!(!narrow.contains("(human)"), "detail is shed to leave the hints room: {narrow}");
        assert!(narrow.contains("F9 Search"), "{narrow}");
    }

    #[test]
    fn task_history_retains_only_the_newest_twenty_runs() {
        let (mut model, _) = opened();
        for _ in 0..25 {
            model.run_task(termesh_core::TaskSpec {
                id: "cargo.test".into(),
                label: "Test".into(),
                program: "cargo".into(),
                args: vec!["test".into(), "--message-format=json-diagnostic-rendered-ansi".into()],
                cwd: "/proj".into(),
            });
        }

        assert_eq!(model.task_runs.len(), 20);
        assert_eq!(model.task_runs.first().unwrap().id.0, 6);
        assert_eq!(model.task_runs.last().unwrap().id.0, 25);
    }

    #[test]
    fn one_task_run_retains_at_most_five_hundred_problems() {
        use std::fmt::Write as _;

        let (mut model, terminal, generation) = running_cargo_test();
        let mut output = String::new();
        for line in 1..=501 {
            writeln!(
                output,
                r#"{{"reason":"compiler-message","message":{{"rendered":"error on line {line}\n","level":"error","message":"failure","spans":[{{"file_name":"src/lib.rs","line_start":{line},"column_start":1,"is_primary":true}}]}}}}"#
            )
            .unwrap();
        }

        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation,
            bytes: output.into_bytes(),
        });

        assert_eq!(model.task_runs.last().unwrap().problems.len(), 500);
    }

    /// Start a second catalog task and get its live PTY, so a test can drive real
    /// decoder output into a run that is not the first one.
    fn spawn_cargo_check(
        model: &mut Model,
    ) -> (termesh_core::TerminalId, termesh_core::TerminalGeneration) {
        model.run_task(termesh_core::TaskSpec {
            id: "cargo.check".into(),
            label: "Check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--message-format=json-diagnostic-rendered-ansi".into()],
            cwd: "/proj".into(),
        });
        let (terminal, generation) = model
            .take_pty_requests()
            .into_iter()
            .find_map(|request| match request {
                termesh_core::PtyRequest::Spawn { terminal, generation, spec, .. }
                    if spec.args.first().map(String::as_str) == Some("check") =>
                {
                    Some((terminal, generation))
                }
                _ => None,
            })
            .expect("the check task spawns a terminal");
        model.on_pty_event(termesh_core::PtyEvent::Spawned {
            terminal,
            generation,
            process_id: Some(43),
        });
        (terminal, generation)
    }

    /// One `cargo --message-format=json` diagnostic record, newline-terminated.
    fn compiler_message_json(file: &str, line: usize, column: usize) -> Vec<u8> {
        format!(
            r#"{{"reason":"compiler-message","message":{{"rendered":"error\n","level":"error","message":"cannot find value","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{column},"is_primary":true}}]}}}}
"#
        )
        .into_bytes()
    }

    fn problem(path: &str, line: usize, column: usize) -> termesh_core::Problem {
        termesh_core::Problem {
            path: path.into(),
            line,
            column,
            severity: termesh_core::ProblemSeverity::Error,
            message: "cannot find value".into(),
        }
    }

    fn take_resolve(model: &mut Model) -> (termesh_core::LocationRequestId, std::path::PathBuf) {
        model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ResolvePath { request, path } => Some((request, path)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn outside_workspace_problem_is_visible_but_not_opened() {
        let (mut model, _, _) = running_cargo_test();
        model.task_runs.last_mut().unwrap().problems = vec![problem("/other/secret.rs", 1, 1)];
        model.dispatch(Command::Action(termesh_core::Action::ProblemsShow));
        let frame = render_to_string(&mut model);
        assert!(frame.contains("Problems"));
        assert!(frame.contains("outside workspace"));
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));
        let (request, _) = take_resolve(&mut model);
        model.on_fs_event(FsEvent::PathResolved { request, path: "/other/secret.rs".into() });
        assert!(model.take_fs_requests().is_empty());
        assert!(model.notification.as_deref().unwrap().contains("outside workspace"));
    }

    #[test]
    fn next_problem_opens_and_positions_an_existing_buffer() {
        let (mut model, _, _) = running_cargo_test();
        model.open_file(std::path::PathBuf::from("/proj/src/lib.rs"));
        let (buffer, path) = model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ReadFile { buffer, path } => Some((buffer, path)),
                _ => None,
            })
            .unwrap();
        model.on_fs_event(FsEvent::FileLoaded {
            buffer,
            path,
            contents: b"one\ntwo\nthree\n".to_vec(),
        });
        model.task_runs.last_mut().unwrap().problems = vec![problem("/proj/src/lib.rs", 3, 2)];
        model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
        let (request, _) = take_resolve(&mut model);
        model.on_fs_event(FsEvent::PathResolved { request, path: "/proj/src/lib.rs".into() });
        assert_eq!(model.active_buffer().unwrap().cursor_position(), (2, 1));
    }

    #[test]
    fn resolved_problem_file_receives_its_pending_location_after_load() {
        let (mut model, _, _) = running_cargo_test();
        model.task_runs.last_mut().unwrap().problems = vec![problem("/proj/src/lib.rs", 4, 3)];
        model.dispatch(Command::Action(termesh_core::Action::ProblemsNext));
        let (request, _) = take_resolve(&mut model);
        model.on_fs_event(FsEvent::PathResolved { request, path: "/proj/src/lib.rs".into() });
        let (buffer, path) = model
            .take_fs_requests()
            .into_iter()
            .find_map(|request| match request {
                FsRequest::ReadFile { buffer, path } => Some((buffer, path)),
                _ => None,
            })
            .unwrap();
        model.on_fs_event(FsEvent::FileLoaded {
            buffer,
            path,
            contents: b"a\nb\nc\ndef\n".to_vec(),
        });
        assert_eq!(model.active_buffer().unwrap().cursor_position(), (3, 2));
    }

    #[test]
    fn parent_traversal_problem_is_not_retained() {
        let (mut model, terminal, generation) = running_cargo_test();
        let json = br#"{"reason":"compiler-message","message":{"rendered":"error\n","level":"error","message":"escape","spans":[{"file_name":"../secret.rs","line_start":1,"column_start":1,"is_primary":true}]}}
"#;
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation,
            bytes: json.to_vec(),
        });
        assert!(model.task_runs.last().unwrap().problems.is_empty());
    }

    #[test]
    fn narrow_workspace_search_keeps_a_preview_visible() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"one\ntwo needle\nthree\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        model.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        model.dispatch(Command::Action(termesh_core::Action::WorkspaceSearch));
        for value in "needle".chars() {
            input::on_chord(&mut model, KeyChord::plain(Key::Char(value)));
        }
        let frame = view::snapshot(&mut model, 72, 24);
        assert!(frame.contains("Search Workspace"), "{frame}");
        assert!(frame.contains("Preview"), "{frame}");
        assert!(frame.contains("two needle"), "{frame}");
    }

    fn running_terminal() -> Model {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::TerminalFocus));
        let _ = model.take_pty_requests();
        model.on_pty_event(termesh_core::PtyEvent::Spawned {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            process_id: Some(42),
        });
        model
    }

    #[test]
    fn retained_terminal_exit_queues_one_git_refresh() {
        let mut model = running_terminal();
        let initial = model.take_git_requests();
        let id = git_request_id(&initial[0]);
        model.on_git_event(GitEvent::SnapshotLoaded { id, snapshot: git_snapshot() });
        assert!(model.take_git_requests().is_empty());

        model.on_pty_event(termesh_core::PtyEvent::Exited {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            exit: termesh_core::TerminalExit { code: Some(0), signal: None },
        });

        assert!(matches!(model.take_git_requests().as_slice(), [GitRequest::Refresh { .. }]));
    }

    #[test]
    fn focusing_terminal_lazily_spawns_the_first_shell() {
        let (mut model, _) = opened();

        model.dispatch(Command::Action(termesh_core::Action::TerminalFocus));

        let requests = model.take_pty_requests();
        assert!(matches!(requests.as_slice(), [termesh_core::PtyRequest::Spawn {
            terminal,
            spec,
            size: termesh_core::TerminalSize { rows: 24, cols: 80 },
            ..
        }] if *terminal == termesh_core::TerminalId::new(1)
            && spec.cwd == std::path::Path::new("/proj")));
        assert_eq!(model.focus, Pane::Terminal);
    }

    #[test]
    fn terminal_output_updates_screen_capture_and_parser_responses() {
        let mut model = running_terminal();

        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            bytes: b"tests ok\r\n\x1b[6n".to_vec(),
        });

        let terminal = model.active_terminal().unwrap();
        assert!(terminal.screen.snapshot().plain_text().contains("tests ok"));
        assert!(terminal.capture.as_str().contains("tests ok"));
        assert!(matches!(model.take_pty_requests().as_slice(), [
            termesh_core::PtyRequest::Write { terminal, bytes, .. }
        ] if *terminal == termesh_core::TerminalId::new(1) && !bytes.is_empty()));
    }

    #[test]
    fn leaving_terminal_restores_the_previous_non_terminal_pane() {
        let (mut model, _) = opened();
        model.focus = Pane::Project;

        model.dispatch(Command::Action(termesh_core::Action::TerminalFocus));
        model.dispatch(Command::Action(termesh_core::Action::TerminalFocus));

        assert_eq!(model.focus, Pane::Project);
    }

    #[test]
    fn new_next_and_previous_manage_terminal_tabs() {
        let mut model = running_terminal();

        model.dispatch(Command::Action(termesh_core::Action::TerminalNew));
        assert_eq!(model.terminals.len(), 2);
        assert_eq!(model.active_terminal().unwrap().id, termesh_core::TerminalId::new(2));

        model.dispatch(Command::Action(termesh_core::Action::TerminalPrevious));
        assert_eq!(model.active_terminal().unwrap().id, termesh_core::TerminalId::new(1));
        model.dispatch(Command::Action(termesh_core::Action::TerminalNext));
        assert_eq!(model.active_terminal().unwrap().id, termesh_core::TerminalId::new(2));
    }

    #[test]
    fn resizing_updates_each_screen_and_live_pty() {
        let mut model = running_terminal();
        model.dispatch(Command::Action(termesh_core::Action::TerminalNew));
        let _ = model.take_pty_requests();

        model.set_terminal_size(termesh_core::TerminalSize { rows: 40, cols: 120 });

        let requests = model.take_pty_requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| matches!(
            request,
            termesh_core::PtyRequest::Resize {
                size: termesh_core::TerminalSize { rows: 40, cols: 120 },
                ..
            }
        )));
        assert!(model.terminals.iter().all(|terminal| {
            let snapshot = terminal.screen.snapshot();
            snapshot.rows() == 40 && snapshot.cols() == 120
        }));
    }

    #[test]
    fn restart_releases_the_old_process_and_respawns_the_same_tab() {
        let mut model = running_terminal();
        let spec = model.active_terminal().unwrap().spec.clone();

        model.dispatch(Command::Action(termesh_core::Action::TerminalRestart));

        assert_eq!(model.active_terminal().unwrap().id, termesh_core::TerminalId::new(1));
        assert_eq!(model.active_terminal().unwrap().spec, spec);
        assert_eq!(
            model.take_pty_requests(),
            [
                termesh_core::PtyRequest::Kill {
                    terminal: termesh_core::TerminalId::new(1),
                    generation: generation(1),
                },
                termesh_core::PtyRequest::Release {
                    terminal: termesh_core::TerminalId::new(1),
                    generation: generation(1),
                },
                termesh_core::PtyRequest::Spawn {
                    terminal: termesh_core::TerminalId::new(1),
                    generation: generation(2),
                    spec,
                    size: termesh_core::TerminalSize { rows: 24, cols: 80 },
                },
            ]
        );
    }

    #[test]
    fn restart_ignores_output_from_the_retired_process_generation() {
        let mut model = running_terminal();
        model.dispatch(Command::Action(termesh_core::Action::TerminalRestart));
        let _ = model.take_pty_requests();
        let terminal = termesh_core::TerminalId::new(1);

        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation: generation(1),
            bytes: b"stale output".to_vec(),
        });
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal,
            generation: generation(2),
            bytes: b"new output".to_vec(),
        });

        let capture = model.active_terminal().unwrap().capture.as_str();
        assert!(!capture.contains("stale output"));
        assert!(capture.contains("new output"));
    }

    #[test]
    fn closing_a_running_terminal_requires_confirmation() {
        let mut model = running_terminal();

        model.dispatch(Command::Action(termesh_core::Action::TerminalClose));

        let Some(crate::model::Overlay::Prompt(prompt)) = model.overlays.pop() else {
            panic!("running terminal should ask before closing");
        };
        assert!(!prompt.takes_input());
        model.confirm_prompt(prompt);
        assert!(model.terminals.is_empty());
        assert_eq!(
            model.take_pty_requests(),
            [
                termesh_core::PtyRequest::Kill {
                    terminal: termesh_core::TerminalId::new(1),
                    generation: generation(1),
                },
                termesh_core::PtyRequest::Release {
                    terminal: termesh_core::TerminalId::new(1),
                    generation: generation(1),
                },
            ]
        );
    }

    #[test]
    fn terminal_run_uses_a_human_owned_shell_command() {
        let (mut model, _) = opened();
        model.dispatch(Command::Action(termesh_core::Action::TerminalRun));
        let Some(crate::model::Overlay::Prompt(mut prompt)) = model.overlays.pop() else {
            panic!("terminal.run should prompt for a command");
        };
        prompt.input = "cargo test --workspace".into();

        model.confirm_prompt(prompt);

        assert_eq!(
            model.active_terminal().unwrap().owner,
            termesh_core::TerminalOwner::HumanCommand
        );
        let requests = model.take_pty_requests();
        let [termesh_core::PtyRequest::Spawn { spec, .. }] = requests.as_slice() else {
            panic!("human command should spawn one PTY");
        };
        assert_eq!(spec.cwd, std::path::Path::new("/proj"));
        assert!(spec.args.iter().any(|arg| arg == "cargo test --workspace"));
    }

    #[test]
    fn terminal_focus_sends_ctrl_c_to_the_pty_not_the_global_keymap() {
        let mut model = running_terminal();

        input::on_chord(&mut model, KeyChord::ctrl(Key::Char('c')));

        assert_eq!(
            model.take_pty_requests(),
            [termesh_core::PtyRequest::Write {
                terminal: termesh_core::TerminalId::new(1),
                generation: generation(1),
                bytes: vec![0x03],
            }]
        );
    }

    #[test]
    fn the_reserved_chord_leaves_the_terminal_instead_of_typing_into_it() {
        let mut model = running_terminal();

        input::on_chord(&mut model, KeyChord::plain(Key::F(6)));

        assert_ne!(model.focus, Pane::Terminal, "F6 should leave the shell");
        assert!(model.take_pty_requests().is_empty(), "F6 must not reach the PTY");
    }

    #[test]
    fn alt_c_enters_copy_mode_instead_of_reaching_the_focused_pty() {
        // The palette is swallowed while a terminal owns the keyboard, so copy mode
        // needs one explicitly reserved route from the pane where it is useful.
        let mut model = running_terminal();

        input::on_chord(&mut model, KeyChord::alt(Key::Char('c')));

        assert!(model.terminal_copy_mode(), "Alt+C should enter terminal copy mode");
        assert!(model.take_pty_requests().is_empty(), "Alt+C must not reach the PTY");
    }

    #[test]
    fn f11_opens_help_instead_of_reaching_the_focused_pty() {
        let mut model = running_terminal();

        input::on_chord(&mut model, KeyChord::plain(Key::F(11)));

        assert!(matches!(model.overlays.last(), Some(crate::model::Overlay::Help(_))));
        assert!(model.take_pty_requests().is_empty(), "F11 must not reach the PTY");
    }

    #[test]
    fn terminal_frame_shows_tabs_output_and_exit_status() {
        let mut model = running_terminal();
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            bytes: b"\x1b[32mtests ok\x1b[0m".to_vec(),
        });
        model.on_pty_event(termesh_core::PtyEvent::Exited {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            exit: termesh_core::TerminalExit { code: Some(0), signal: None },
        });

        let frame = render_to_string(&mut model);

        assert!(frame.contains("tests ok"));
        assert!(frame.contains("Terminal 1"));
        assert!(frame.contains("exited 0"));
    }

    /// Regression, end to end: running `git status` crashed the app. Its file list is
    /// tab-indented, alacritty keeps the literal '\t' in the cells a tab stop covers,
    /// and drawing one panicked ratatui's buffer. Rendering is the layer that died, so
    /// the guard lives here as well as in the screen model.
    #[test]
    fn rendering_tab_indented_output_does_not_panic() {
        let mut model = running_terminal();
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            bytes:
                b"On branch main\r\nChanges not staged for commit:\r\n\tmodified:   src/main.rs\r\n"
                    .to_vec(),
        });

        let frame = render_to_string(&mut model);

        assert!(frame.contains("modified:"), "{frame}");
        assert!(!frame.contains('\t'), "a raw tab must never reach the frame: {frame}");
    }

    #[test]
    fn global_shortcuts_work_after_the_focused_terminal_exits() {
        let mut model = running_terminal();
        model.on_pty_event(termesh_core::PtyEvent::Exited {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            exit: termesh_core::TerminalExit { code: Some(0), signal: None },
        });

        input::on_chord(&mut model, KeyChord::ctrl(Key::Char('p')));

        assert!(model.overlay_active(), "an exited PTY must not capture global shortcuts");
    }

    #[test]
    fn terminal_copy_mode_copies_a_grid_selection_without_pty_input() {
        let mut model = running_terminal();
        model.on_pty_event(termesh_core::PtyEvent::Output {
            terminal: termesh_core::TerminalId::new(1),
            generation: generation(1),
            bytes: b"hello".to_vec(),
        });
        model.dispatch(Command::Action(termesh_core::Action::TerminalCopyMode));

        for _ in 0..5 {
            input::on_chord(&mut model, KeyChord::plain(Key::Left));
        }
        for _ in 0..4 {
            input::on_chord(&mut model, KeyChord::shift(Key::Right));
        }
        input::on_chord(&mut model, KeyChord::plain(Key::Enter));

        assert_eq!(model.take_clipboard_text(), ["hello"]);
        assert!(model.take_pty_requests().is_empty());
        assert!(!model.terminal_copy_mode());
    }

    /// A reader over the fake, anchored at `/proj` with the model's ignore options.
    fn reader<'a>(fs: &'a FakeFileSystem) -> DirReader<'a> {
        DirReader::new(fs, std::path::Path::new("/proj"), IgnoreOptions::default())
    }

    fn tree_names(m: &Model) -> Vec<String> {
        m.explorer.as_ref().unwrap().tree.visible_rows().iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn opening_a_workspace_renders_its_first_level() {
        let (mut m, _fs) = opened();
        assert_eq!(tree_names(&m), ["proj", "src", "Cargo.toml", "README.md"]);

        let frame = render_to_string(&mut m);
        assert!(frame.contains("Cargo.toml"), "the tree reaches the screen");
        assert!(frame.contains("proj (rust)"), "status bar names the project and its kind");
    }

    #[test]
    fn configured_exclusions_hide_matching_entries() {
        let fs = sample_fs();
        let mut m = Model::new();
        m.settings.exclusions = vec!["README.md".to_string()];
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        assert_eq!(tree_names(&m), ["proj", "src", "Cargo.toml"]);
    }

    #[test]
    fn with_no_workspace_the_pane_explains_how_to_open_one() {
        let frame = render_to_string(&mut Model::new());
        assert!(frame.contains("No workspace open"));
    }

    #[test]
    fn opening_focuses_the_project_pane_and_watches_before_it_reads() {
        let fs = sample_fs();
        let mut m = Model::new();
        let root = termesh_workspace::detect_root(&fs, std::path::Path::new("/proj"));
        m.open_workspace(root);

        assert_eq!(m.focus, Pane::Project);
        let requests = m.take_fs_requests();

        let watch = requests.iter().position(|r| matches!(r, FsRequest::Watch(_)));
        let read = requests.iter().position(|r| matches!(r, FsRequest::ReadDir { .. }));
        let (Some(watch), Some(read)) = (watch, read) else {
            panic!("opening must queue both a watch and a read, got {requests:?}");
        };

        // Not a stylistic preference: `Watch` is what tells the worker the root, and
        // therefore what anchors the ignore chain. Reversed, the worker serves the very
        // first listing — the one the user sees on launch — unfiltered, and nothing
        // re-reads the root to correct it. The worker depends on this silently, so the
        // contract is asserted here.
        assert!(watch < read, "Watch must precede ReadDir, got {requests:?}");
    }

    #[test]
    fn expanding_a_directory_queues_exactly_one_read() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // select `src`
        m.dispatch(Command::ExplorerToggle);

        let requests = m.take_fs_requests();
        assert_eq!(requests.len(), 1, "one expansion, one read");

        m.on_fs_event(FsEvent::DirLoaded {
            id: m.explorer.as_ref().unwrap().tree.selected(),
            entries: fs.read_dir(std::path::Path::new("/proj/src")).unwrap(),
        });
        assert_eq!(
            tree_names(&m),
            ["proj", "src", "main.rs", "model.rs", "Cargo.toml", "README.md"]
        );
    }

    #[test]
    fn collapsing_does_not_re_read() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerToggle);
        let _ = m.take_fs_requests();
        m.settle_fs_sync(&mut reader(&fs));

        m.dispatch(Command::ExplorerToggle); // collapse
        assert!(m.take_fs_requests().is_empty(), "collapsing touches no disk");
    }

    #[test]
    fn explorer_keys_do_nothing_while_another_pane_is_focused() {
        let (mut m, _fs) = opened();
        let before = m.explorer.as_ref().unwrap().tree.selected();

        m.focus = Pane::Editor;
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerToggle);

        assert_eq!(m.explorer.as_ref().unwrap().tree.selected(), before);
        assert!(m.take_fs_requests().is_empty(), "no work queued from an unfocused pane");
    }

    #[test]
    fn arrow_keys_navigate_the_tree_through_the_keymap() {
        let (mut m, _fs) = opened();
        input::on_chord(&mut m, KeyChord::plain(Key::Down));
        assert_eq!(m.explorer.as_ref().unwrap().tree.selected_row(), 1);
        input::on_chord(&mut m, KeyChord::plain(Key::Up));
        assert_eq!(m.explorer.as_ref().unwrap().tree.selected_row(), 0);
    }

    #[test]
    fn a_failed_expansion_surfaces_on_the_node_and_in_the_status_bar() {
        let fs = sample_fs();
        fs.fail("/proj/src", termesh_core::FsError::PermissionDenied("/proj/src".into()));
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::ExplorerToggle);
        m.settle_fs_sync(&mut reader(&fs));

        let frame = render_to_string(&mut m);
        assert!(frame.contains("permission denied"), "the reason is visible, not swallowed");
        assert!(m.notification.is_some());
        assert!(tree_names(&m).contains(&"README.md".to_string()), "siblings still render");
    }

    #[test]
    fn a_watch_event_queues_one_read_for_the_containing_directory() {
        let (mut m, _fs) = opened();
        m.on_fs_event(FsEvent::Changed(vec![
            std::path::PathBuf::from("/proj/a.md"),
            std::path::PathBuf::from("/proj/b.md"),
        ]));
        let requests = m.take_fs_requests();
        assert_eq!(requests.len(), 1, "a burst in one directory coalesces to one read");
    }

    #[test]
    fn a_watch_event_brings_a_new_file_into_the_tree() {
        let (mut m, fs) = opened();
        assert!(!tree_names(&m).contains(&"NEW.md".to_string()));

        fs.add_file("/proj/NEW.md", b"");
        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/NEW.md")]));
        m.settle_fs_sync(&mut reader(&fs));

        assert!(tree_names(&m).contains(&"NEW.md".to_string()), "the new file appears");
    }

    #[test]
    fn a_watch_event_removes_a_deleted_file_from_the_tree() {
        let (mut m, fs) = opened();
        fs.remove_file(std::path::Path::new("/proj/README.md")).unwrap();

        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/README.md")]));
        m.settle_fs_sync(&mut reader(&fs));

        assert!(!tree_names(&m).contains(&"README.md".to_string()));
        assert!(tree_names(&m).contains(&"Cargo.toml".to_string()), "siblings survive");
    }

    #[test]
    fn a_watch_event_preserves_expansion_and_selection() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::ExplorerToggle);
        m.settle_fs_sync(&mut reader(&fs));
        m.dispatch(Command::ExplorerNext); // main.rs
        let selected = m.explorer.as_ref().unwrap().tree.selected();

        fs.add_file("/proj/UNRELATED.md", b"");
        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/UNRELATED.md")]));
        m.settle_fs_sync(&mut reader(&fs));

        assert_eq!(m.explorer.as_ref().unwrap().tree.selected(), selected, "selection survives");
        assert!(tree_names(&m).contains(&"main.rs".to_string()), "src stays expanded");
    }

    #[test]
    fn the_tree_hides_gitignored_and_hidden_entries() {
        let fs = FakeFileSystem::with_paths(&[
            "/proj/Cargo.toml",
            "/proj/src/main.rs",
            "/proj/target/debug/build",
            "/proj/.git/config",
            "/proj/.env",
        ]);
        fs.add_file("/proj/.gitignore", b"target\n");

        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        let names = tree_names(&m);
        assert!(!names.contains(&"target".to_string()), "gitignored");
        assert!(!names.contains(&".git".to_string()), "dotfile");
        assert!(!names.contains(&".env".to_string()), "dotfile");
        assert_eq!(names, ["proj", "src", "Cargo.toml"]);
    }

    #[test]
    fn show_all_reveals_what_the_default_hides() {
        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml", "/proj/target/debug/build"]);
        fs.add_file("/proj/.gitignore", b"target\n");

        let mut m = Model::new();
        m.ignore_options = IgnoreOptions::show_all();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        assert!(tree_names(&m).contains(&"target".to_string()));
    }

    #[test]
    fn the_agent_snapshot_shows_exactly_what_the_human_sees() {
        // The premise of the project: one source of truth. If ignore rules hide
        // `target/` from the screen, they hide it from agent context too — there is no
        // second traversal that could disagree.
        let fs = FakeFileSystem::with_paths(&[
            "/proj/Cargo.toml",
            "/proj/src/main.rs",
            "/proj/target/debug/build",
            "/proj/.env",
        ]);
        fs.add_file("/proj/.gitignore", b"target\n");

        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        let snapshot = m.workspace_snapshot().unwrap();
        let seen: Vec<String> =
            snapshot.visible_tree.iter().map(|e| e.path.to_string_lossy().into_owned()).collect();

        assert_eq!(seen, ["src", "Cargo.toml"]);
        assert!(!seen.iter().any(|p| p.contains("target")), "gitignored, so invisible to both");
        assert!(!seen.iter().any(|p| p.contains(".env")), "hidden, so invisible to both");
        assert_eq!(snapshot.project_kind, termesh_workspace::ProjectKind::Rust);
    }

    #[test]
    fn expanding_a_directory_widens_what_the_agent_can_see() {
        let (mut m, fs) = opened();
        let before = m.workspace_snapshot().unwrap().len();

        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::ExplorerToggle);
        m.settle_fs_sync(&mut reader(&fs));

        let after = m.workspace_snapshot().unwrap();
        assert!(after.len() > before, "opening a directory shares it with the agent");
        assert!(after.visible_tree.iter().any(|e| e.path == std::path::Path::new("src/main.rs")));
    }

    #[test]
    fn there_is_no_snapshot_without_a_workspace() {
        assert!(Model::new().workspace_snapshot().is_none());
    }

    // --- file operations ---------------------------------------------------------

    use crate::model::{Overlay, PromptKind};
    use termesh_core::Action;

    /// Invoke an action, type a name into the resulting prompt, and press Enter.
    fn do_prompt(m: &mut Model, action: Action, name: &str) {
        m.dispatch(Command::Action(action));
        for c in name.chars() {
            input::on_chord(m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(m, KeyChord::plain(Key::Enter));
    }

    #[test]
    fn creating_a_file_writes_it_and_it_appears_in_the_tree() {
        let (mut m, fs) = opened();
        do_prompt(&mut m, Action::FileNew, "NOTES.md");
        m.settle_fs_sync(&mut reader(&fs));

        assert!(fs.paths().contains(&std::path::PathBuf::from("/proj/NOTES.md")));
        assert!(tree_names(&m).contains(&"NOTES.md".to_string()), "and the tree refreshed");
    }

    #[test]
    fn a_new_file_lands_beside_the_selection_not_inside_it() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // src (a directory)
        do_prompt(&mut m, Action::FileNew, "inside.rs");
        m.settle_fs_sync(&mut reader(&fs));
        assert!(fs.paths().contains(&std::path::PathBuf::from("/proj/src/inside.rs")));

        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerNext); // a file at the root
        do_prompt(&mut m, Action::FileNew, "beside.md");
        m.settle_fs_sync(&mut reader(&fs));
        assert!(
            fs.paths().contains(&std::path::PathBuf::from("/proj/beside.md")),
            "selecting a file targets its containing directory"
        );
    }

    #[test]
    fn creating_a_folder_works_too() {
        let (mut m, fs) = opened();
        do_prompt(&mut m, Action::FolderNew, "assets");
        m.settle_fs_sync(&mut reader(&fs));
        assert!(tree_names(&m).contains(&"assets".to_string()));
    }

    #[test]
    fn renaming_moves_the_entry_and_prefills_the_current_name() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::Action(Action::FileRename));

        let Some(Overlay::Prompt(p)) = m.overlays.last() else { panic!("expected a prompt") };
        assert_eq!(p.input, "src", "pre-filled, so a small edit stays small");

        // Replace the name entirely.
        for _ in 0..3 {
            input::on_chord(&mut m, KeyChord::plain(Key::Backspace));
        }
        for c in "lib".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        m.settle_fs_sync(&mut reader(&fs));

        assert!(tree_names(&m).contains(&"lib".to_string()));
        assert!(!tree_names(&m).contains(&"src".to_string()));
    }

    #[test]
    fn deleting_asks_first_and_esc_cancels_without_touching_disk() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerNext); // Cargo.toml
        m.dispatch(Command::Action(Action::FileDelete));

        let Some(Overlay::Prompt(p)) = m.overlays.last() else { panic!("expected a prompt") };
        assert!(matches!(p.kind, PromptKind::ConfirmDelete { .. }));
        assert!(!p.takes_input(), "delete is a confirmation, not a text entry");

        input::on_chord(&mut m, KeyChord::plain(Key::Esc));
        m.settle_fs_sync(&mut reader(&fs));
        assert!(
            fs.paths().contains(&std::path::PathBuf::from("/proj/Cargo.toml")),
            "cancelling must not delete anything"
        );
    }

    #[test]
    fn confirming_a_delete_removes_the_entry() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerNext); // Cargo.toml
        m.dispatch(Command::Action(Action::FileDelete));
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        m.settle_fs_sync(&mut reader(&fs));

        assert!(!fs.paths().contains(&std::path::PathBuf::from("/proj/Cargo.toml")));
        assert!(!tree_names(&m).contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn deleting_a_directory_takes_its_contents() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::Action(Action::FileDelete));
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        m.settle_fs_sync(&mut reader(&fs));

        assert!(!fs.paths().iter().any(|p| p.starts_with("/proj/src")));
        assert!(
            fs.paths().contains(&std::path::PathBuf::from("/proj/Cargo.toml")),
            "siblings live"
        );
    }

    #[test]
    fn names_that_would_escape_the_directory_are_rejected() {
        let (mut m, fs) = opened();
        for bad in ["../escaped.md", "sub/nested.md", "..", ""] {
            do_prompt(&mut m, Action::FileNew, bad);
            assert!(m.take_fs_requests().is_empty(), "'{bad}' must not reach the filesystem");
            assert!(m.notification.is_some(), "'{bad}' should explain itself");
        }
        assert!(!fs.paths().iter().any(|p| p.to_string_lossy().contains("escaped")));
    }

    #[test]
    fn the_root_itself_cannot_be_renamed_or_deleted() {
        let (mut m, _fs) = opened();
        // Selection starts on the root.
        m.dispatch(Command::Action(Action::FileDelete));
        assert!(!m.overlay_active(), "no confirmation prompt for the root");
        m.dispatch(Command::Action(Action::FileRename));
        assert!(!m.overlay_active());
    }

    #[test]
    fn a_failed_mutation_reports_why_and_changes_nothing() {
        let (mut m, fs) = opened();
        fs.add_file("/proj/taken.md", b"existing");

        do_prompt(&mut m, Action::FileNew, "taken.md");
        m.settle_fs_sync(&mut reader(&fs));

        assert!(
            m.notification.as_ref().unwrap().contains("already exists"),
            "got: {:?}",
            m.notification
        );
        assert_eq!(fs.read_file(std::path::Path::new("/proj/taken.md")).unwrap(), b"existing");
    }

    #[test]
    fn file_operations_are_reachable_from_the_palette() {
        // The "one command surface" invariant: these are registry actions, so the
        // palette finds them without any explorer-specific plumbing.
        let mut m = Model::new();
        m.dispatch(Command::OpenPalette);
        for c in "rename".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        assert!(render_to_string(&mut m).contains("Rename"));
    }

    #[test]
    fn every_file_operation_is_permission_gated_for_agents() {
        for a in [Action::FileNew, Action::FolderNew, Action::FileRename, Action::FileDelete] {
            assert!(a.agent_needs_permission(), "{} writes, so it must be gated", a.id());
        }
    }

    // --- editor (Phase 03) --------------------------------------------------------

    /// Select `Cargo.toml` in the explorer and press Enter, settling the read.
    fn opened_with_file(fs: &FakeFileSystem) -> Model {
        let mut m = Model::new();
        m.open_workspace_sync(fs, std::path::Path::new("/proj"));
        m.dispatch(Command::ExplorerNext); // src
        m.dispatch(Command::ExplorerNext); // Cargo.toml
        m.dispatch(Command::ExplorerToggle);
        m.settle_fs_sync(&mut reader(fs));
        m
    }

    #[test]
    fn enter_on_a_file_opens_it_in_the_editor() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"[package]\nname = \"proj\"\n");
        let mut m = opened_with_file(&fs);

        let buffer = m.active_buffer().expect("a buffer should be open");
        assert_eq!(buffer.display_name(), "Cargo.toml");
        assert_eq!(buffer.text().to_string(), "[package]\nname = \"proj\"\n");
        assert_eq!(m.focus, Pane::Editor, "opening a file moves focus to it");

        let frame = render_to_string(&mut m);
        assert!(frame.contains("[package]"), "the file's contents reach the screen");
        assert!(frame.contains("Cargo.toml"), "and the pane is titled after it");
    }

    #[test]
    fn enter_on_a_directory_still_expands_rather_than_opening() {
        let (mut m, fs) = opened();
        m.dispatch(Command::ExplorerNext); // src, a directory
        m.dispatch(Command::ExplorerToggle);
        m.settle_fs_sync(&mut reader(&fs));

        assert!(m.active_buffer().is_none(), "directories expand, they do not open");
        assert!(tree_names(&m).contains(&"main.rs".to_string()));
    }

    #[test]
    fn opening_the_same_file_twice_focuses_the_existing_buffer() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"x");
        let mut m = opened_with_file(&fs);
        m.focus = Pane::Project;

        m.dispatch(Command::ExplorerToggle); // Enter again on the same row
        m.settle_fs_sync(&mut reader(&fs));

        assert_eq!(m.buffers.len(), 1, "no duplicate buffer");
        assert_eq!(m.focus, Pane::Editor);
    }

    #[test]
    fn a_binary_file_is_refused_by_name_rather_than_opened_as_mojibake() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", &[0xff, 0xfe, 0x00]);
        let m = opened_with_file(&fs);

        assert!(m.active_buffer().is_none());
        assert!(m.notification.as_ref().unwrap().contains("Cargo.toml"), "{:?}", m.notification);
    }

    /// The payoff of keymap context predicates: one chord, two meanings, decided by focus.
    /// Phase 02 left a note at `model.rs` asking Phase 03 to do exactly this instead of
    /// adding a second focus check.
    #[test]
    fn the_same_arrow_key_drives_the_tree_or_the_cursor_depending_on_focus() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"one\ntwo\nthree\n");
        let mut m = opened_with_file(&fs);

        // Focused on the editor, Down moves the cursor.
        input::on_chord(&mut m, KeyChord::plain(Key::Down));
        assert_eq!(m.active_buffer().unwrap().cursor_position(), (1, 0));
        let tree_row = m.explorer.as_ref().unwrap().tree.selected_row();

        // Focused on the tree, the very same chord moves the selection instead.
        m.focus = Pane::Project;
        input::on_chord(&mut m, KeyChord::plain(Key::Down));
        assert_eq!(m.active_buffer().unwrap().cursor_position(), (1, 0), "cursor stayed put");
        assert_ne!(m.explorer.as_ref().unwrap().tree.selected_row(), tree_row, "the tree moved");
    }

    #[test]
    fn typing_inserts_text_and_marks_the_buffer_dirty() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"ab\n");
        let mut m = opened_with_file(&fs);

        for c in "XY".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        let buffer = m.active_buffer().unwrap();
        assert_eq!(buffer.text().to_string(), "XYab\n");
        assert!(buffer.is_dirty());
        assert!(render_to_string(&mut m).contains("XYab"));
    }

    #[test]
    fn a_modified_chord_types_nothing() {
        // An unbound Ctrl+K must not leave a stray 'k' in somebody's source file.
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"ab\n");
        let mut m = opened_with_file(&fs);

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('k')));
        assert_eq!(m.active_buffer().unwrap().text().to_string(), "ab\n");
    }

    #[test]
    fn typing_does_nothing_while_the_tree_has_focus() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"ab\n");
        let mut m = opened_with_file(&fs);
        m.focus = Pane::Project;

        input::on_chord(&mut m, KeyChord::plain(Key::Char('z')));
        assert_eq!(m.active_buffer().unwrap().text().to_string(), "ab\n");
    }

    #[test]
    fn editing_and_undoing_round_trips_through_the_keymap() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"ab\n");
        let mut m = opened_with_file(&fs);

        for c in "xyz".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        assert_eq!(m.active_buffer().unwrap().text().to_string(), "xyzab\n");

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('z')));
        assert_eq!(
            m.active_buffer().unwrap().text().to_string(),
            "ab\n",
            "a run of typing is one undo step"
        );
    }

    #[test]
    fn saving_writes_the_buffer_back_through_the_service() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"ab\n");
        let mut m = opened_with_file(&fs);

        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('s')));
        m.settle_fs_sync(&mut reader(&fs));

        assert_eq!(fs.read_file(std::path::Path::new("/proj/Cargo.toml")).unwrap(), b"Xab\n");
        assert!(!m.active_buffer().unwrap().is_dirty(), "and the buffer settles");
    }

    #[test]
    fn saving_with_nothing_open_says_so_instead_of_failing_silently() {
        let mut m = Model::new();
        m.dispatch(Command::Action(Action::FileSave));
        assert!(m.notification.is_some());
        assert!(m.take_fs_requests().is_empty(), "and touches no disk");
    }

    #[test]
    fn opening_a_file_never_reads_it_on_the_render_loop() {
        // The read must be queued for the worker, exactly like a directory listing —
        // a cold file on a network mount must not freeze the UI (ADR-0005 §1).
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"x");
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerNext);
        m.dispatch(Command::ExplorerToggle);

        let requests = m.take_fs_requests();
        assert!(
            requests.iter().any(|r| matches!(r, FsRequest::ReadFile { .. })),
            "expected a queued read, got {requests:?}"
        );
        assert!(m.active_buffer().is_none(), "nothing is open until the worker answers");
    }

    // --- tabs (Phase 03, slice 8) --------------------------------------------------

    /// Open both files in the sample workspace.
    fn two_files_open(fs: &FakeFileSystem) -> Model {
        let mut m = Model::new();
        m.open_workspace_sync(fs, std::path::Path::new("/proj"));
        m.open_file_sync(fs, std::path::PathBuf::from("/proj/Cargo.toml"));
        m.open_file_sync(fs, std::path::PathBuf::from("/proj/README.md"));
        m
    }

    #[test]
    fn a_second_open_file_gets_a_tab_strip() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"[package]\n");
        fs.add_file("/proj/README.md", b"# hi\n");
        let mut m = two_files_open(&fs);

        assert_eq!(m.buffers.len(), 2);
        let frame = render_to_string(&mut m);
        assert!(frame.contains("Cargo.toml"), "both files are listed:\n{frame}");
        assert!(frame.contains("README.md"));
    }

    #[test]
    fn one_file_gets_no_strip_because_there_is_no_choice_to_make() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"only\n");
        let mut m = opened_with_file(&fs);
        // The row is worth more as text than as a strip of one.
        assert!(render_to_string(&mut m).contains(" 1  only"));
    }

    #[test]
    fn ctrl_tab_cycles_through_open_files_and_wraps() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\n");
        fs.add_file("/proj/README.md", b"b\n");
        let mut m = two_files_open(&fs);
        assert_eq!(m.active_buffer().unwrap().display_name(), "README.md");

        input::on_chord(&mut m, KeyChord::ctrl(Key::Tab));
        assert_eq!(m.active_buffer().unwrap().display_name(), "Cargo.toml", "wraps around");
        input::on_chord(&mut m, KeyChord::ctrl(Key::Tab));
        assert_eq!(m.active_buffer().unwrap().display_name(), "README.md");
    }

    #[test]
    fn closing_a_clean_file_takes_it_away_without_asking() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\n");
        fs.add_file("/proj/README.md", b"b\n");
        let mut m = two_files_open(&fs);

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('w')));
        assert_eq!(m.buffers.len(), 1);
        assert_eq!(m.active_buffer().unwrap().display_name(), "Cargo.toml", "lands on the left");
        assert!(!m.overlay_active(), "nothing to warn about");
    }

    #[test]
    fn closing_unsaved_work_asks_first() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\n");
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('w')));
        assert!(m.overlay_active(), "unsaved work is not discarded silently");
        assert_eq!(m.buffers.len(), 1, "and nothing closed yet");

        input::on_chord(&mut m, KeyChord::plain(Key::Esc));
        assert_eq!(m.buffers.len(), 1, "cancelling keeps it open");

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('w')));
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        assert!(m.buffers.is_empty(), "confirming closes it");
        assert!(m.active_buffer().is_none());
    }

    #[test]
    fn closing_the_last_file_returns_to_the_empty_state() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\n");
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('w')));
        assert!(render_to_string(&mut m).contains("No file open"));
    }

    #[test]
    fn tabs_do_nothing_with_a_single_file() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\n");
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::ctrl(Key::Tab));
        assert_eq!(m.active_buffer().unwrap().display_name(), "Cargo.toml");
    }

    // --- syntax highlighting (Phase 03, slice 8) -------------------------------------

    #[test]
    fn a_rust_file_is_highlighted_when_opened() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"// note\nfn main() {}\n");
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));

        let decorated = m.active_buffer().unwrap().decorations().len();
        assert!(decorated > 0, "the file should arrive highlighted");
    }

    #[test]
    fn a_file_we_have_no_grammar_for_is_left_plain() {
        let fs = sample_fs();
        fs.add_file("/proj/README.md", b"# heading\n");
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/README.md"));

        assert_eq!(m.active_buffer().unwrap().decorations().len(), 0);
        assert!(render_to_string(&mut m).contains("# heading"), "and still renders");
    }

    #[test]
    fn highlighting_follows_the_text_as_it_is_edited() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"fn main() {}\n");
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        let before = m.active_buffer().unwrap().decorations().len();

        // Turn the whole line into a comment.
        m.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(0));
        for c in "// ".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }

        let after = m.active_buffer().unwrap().decorations().len();
        assert_ne!(before, after, "the spans are recomputed, not carried forward stale");
    }

    /// Highlighting and search are both derived, but one must not wipe the other.
    #[test]
    fn a_search_survives_a_re_highlight_and_vice_versa() {
        let fs = sample_fs();
        fs.add_file("/proj/src/main.rs", b"fn main() { let one = 1; }\n");
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));

        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");
        let matches = m.find.as_ref().unwrap().matches.len();
        assert!(matches > 0);

        m.sync_syntax();
        let still_matching = m
            .active_buffer()
            .unwrap()
            .decorations()
            .iter()
            .filter(|d| matches!(d.class, termesh_editor::DecorationClass::Match { .. }))
            .count();
        assert_eq!(still_matching, matches, "a re-parse must not wipe the find results");
    }

    #[test]
    fn a_very_large_file_is_left_unhighlighted_rather_than_stuttering() {
        let fs = sample_fs();
        let big = "fn f() {}\n".repeat(40_000); // comfortably past the cap
        fs.add_file("/proj/src/main.rs", big.as_bytes());
        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));

        assert_eq!(m.active_buffer().unwrap().decorations().len(), 0);
    }

    // --- find / replace (Phase 03, slice 8) -----------------------------------------

    /// Open Ctrl+F (or Ctrl+H), clear whatever it pre-filled, type, confirm.
    fn find_prompt(m: &mut Model, chord: KeyChord, text: &str) {
        input::on_chord(m, chord);
        for _ in 0..64 {
            input::on_chord(m, KeyChord::plain(Key::Backspace));
        }
        for c in text.chars() {
            input::on_chord(m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(m, KeyChord::plain(Key::Enter));
    }

    #[test]
    fn find_prefills_the_previous_query_so_refining_is_a_small_edit() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('f')));
        let Some(crate::model::Overlay::Prompt(p)) = m.overlays.last() else { panic!() };
        assert_eq!(p.input, "one");
    }

    fn searchable() -> FakeFileSystem {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"one two\nthree one\nfour\none\n");
        fs
    }

    #[test]
    fn finding_highlights_every_match_and_jumps_to_the_first() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");

        let find = m.find.as_ref().expect("a find is running");
        assert_eq!(find.matches.len(), 3);
        assert_eq!(find.current, Some(0));
        assert_eq!(m.active_buffer().unwrap().cursor_position(), (0, 3), "selects the match");
        assert!(m.notification.as_ref().unwrap().contains('3'));
    }

    #[test]
    fn f3_steps_through_the_matches_and_wraps() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");

        let lines: Vec<usize> = (0..4)
            .map(|_| {
                input::on_chord(&mut m, KeyChord::plain(Key::F(3)));
                m.active_buffer().unwrap().cursor_position().0
            })
            .collect();
        assert_eq!(lines, [1, 3, 0, 1], "forward through all three, then wraps");
    }

    #[test]
    fn shift_f3_steps_backwards_without_getting_stuck() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");

        let lines: Vec<usize> = (0..3)
            .map(|_| {
                input::on_chord(&mut m, KeyChord::shift(Key::F(3)));
                m.active_buffer().unwrap().cursor_position().0
            })
            .collect();
        assert_eq!(lines, [3, 1, 0], "a cursor inside a match must not find it again");
    }

    #[test]
    fn a_search_with_no_matches_says_so_rather_than_moving_the_cursor() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        let before = m.active_buffer().unwrap().cursor_position();

        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "zzz");
        assert_eq!(m.active_buffer().unwrap().cursor_position(), before);
        assert!(m.notification.as_ref().unwrap().contains("no matches"));
    }

    #[test]
    fn search_is_smart_about_case() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"Error error ERROR\n");
        let mut m = opened_with_file(&fs);

        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "error");
        assert_eq!(m.find.as_ref().unwrap().matches.len(), 3, "lowercase matches all");

        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "Error");
        assert_eq!(m.find.as_ref().unwrap().matches.len(), 1, "a capital narrows it");
    }

    #[test]
    fn matches_are_visible_on_screen() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");

        let decorated = m.active_buffer().unwrap().decorations().len();
        assert_eq!(decorated, 3, "one decoration per match");
        assert!(render_to_string(&mut m).contains("one two"), "and the text still renders");
    }

    #[test]
    fn replacing_changes_every_match_in_one_undo_step() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('r')), "1");

        assert_eq!(m.active_buffer().unwrap().text().to_string(), "1 two\nthree 1\nfour\n1\n");
        assert!(m.notification.as_ref().unwrap().contains('3'));

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('z')));
        assert_eq!(
            m.active_buffer().unwrap().text().to_string(),
            "one two\nthree one\nfour\none\n",
            "three replacements, one undo"
        );
    }

    #[test]
    fn replacing_with_nothing_deletes_the_matches() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('r')));
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));

        assert_eq!(m.active_buffer().unwrap().text().to_string(), " two\nthree \nfour\n\n");
    }

    #[test]
    fn replace_without_a_search_asks_for_one_first() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('r')));
        assert!(!m.overlay_active());
        assert!(m.notification.as_ref().unwrap().contains("Ctrl+F"));
    }

    #[test]
    fn stale_matches_are_dropped_after_a_replace() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('f')), "one");
        find_prompt(&mut m, KeyChord::ctrl(Key::Char('r')), "1");

        // The old ranges describe a document that no longer exists.
        assert!(m.find.is_none());
        assert_eq!(m.active_buffer().unwrap().decorations().len(), 0);
    }

    #[test]
    fn workspace_search_uses_f9_outside_the_editor() {
        let fs = searchable();
        let mut m = opened_with_file(&fs);
        m.focus = Pane::Project;
        input::on_chord(&mut m, KeyChord::plain(Key::F(9)));
        assert!(matches!(m.overlays.last(), Some(crate::model::Overlay::Search(search))
            if search.mode == SearchMode::Text));
    }

    #[test]
    fn command_palette_uses_f10_in_every_non_terminal_pane() {
        for focus in [Pane::Project, Pane::Editor, Pane::Agent] {
            let mut model = Model::new();
            model.focus = focus;
            input::on_chord(&mut model, KeyChord::plain(Key::F(10)));
            assert!(matches!(model.overlays.last(), Some(crate::model::Overlay::Palette(_))));
        }
    }

    #[test]
    fn tabs_render_to_their_stops_so_the_cursor_can_line_up() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"a\tb\n");
        let mut m = opened_with_file(&fs);

        // A tab after one character advances to column 4, not by a flat four spaces.
        assert!(render_to_string(&mut m).contains("a   b"), "tab stops, not fixed width");
    }

    // --- decorations reach the screen (Phase 03, slice 4) -------------------------

    use termesh_core::ProposalId;
    use termesh_editor::{Decoration, DecorationClass, HunkSide, HunkState};

    /// Attach a hunk decoration over `start..end` of the active buffer.
    fn decorate_hunk(m: &mut Model, start: usize, end: usize, side: HunkSide) {
        m.active_buffer_mut().unwrap().decorations_mut().push(Decoration::new(
            start,
            end,
            DecorationClass::Hunk { proposal: ProposalId::new(1), side, state: HunkState::Clean },
        ));
    }

    #[test]
    fn a_proposed_change_marks_its_line_in_the_gutter() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"fn main() {}\nother\n");
        let mut m = opened_with_file(&fs);
        decorate_hunk(&mut m, 3, 7, HunkSide::Removed); // `main`

        let frame = render_to_string(&mut m);
        assert!(frame.contains("1~"), "the changed line is marked, got:\n{frame}");
        assert!(!frame.contains("2~"), "and only that line");
    }

    #[test]
    fn an_addition_and_a_removal_are_marked_differently() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"one\ntwo\n");
        let mut m = opened_with_file(&fs);
        decorate_hunk(&mut m, 0, 3, HunkSide::Removed);
        decorate_hunk(&mut m, 4, 4, HunkSide::Added); // zero-width anchor on line 2

        let frame = render_to_string(&mut m);
        assert!(frame.contains("1~"), "removal");
        assert!(frame.contains("2+"), "addition, an anchor rather than a range");
    }

    /// The review must stay honest while the human keeps typing: a hunk they edited
    /// inside is flagged, not quietly left looking acceptable.
    #[test]
    fn typing_inside_a_proposed_change_flags_it_on_screen() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"fn main() {}\n");
        let mut m = opened_with_file(&fs);
        decorate_hunk(&mut m, 3, 7, HunkSide::Removed);

        // Put the cursor inside the hunk and type.
        m.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(5));
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));

        let frame = render_to_string(&mut m);
        assert!(frame.contains("1!"), "the collision is visible, got:\n{frame}");
    }

    #[test]
    fn a_hunk_rides_forward_when_the_human_types_above_it() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"fn main() {}\n");
        let mut m = opened_with_file(&fs);
        decorate_hunk(&mut m, 3, 7, HunkSide::Removed);

        m.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(0));
        for c in "pub ".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }

        let buffer = m.active_buffer().unwrap();
        let d = buffer.decorations().iter().next().unwrap();
        assert_eq!((d.start, d.end), (7, 11), "still on `main`");
        assert!(
            matches!(d.class, DecorationClass::Hunk { state: HunkState::Clean, .. }),
            "an edit elsewhere is not a conflict"
        );
    }

    #[test]
    fn an_undecorated_buffer_renders_no_markers() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"fn main() {}\n");
        let mut m = opened_with_file(&fs);
        let frame = render_to_string(&mut m);
        // Number, an empty marker column, then the text.
        assert!(frame.contains(" 1  fn main"), "plain gutter, got:\n{frame}");
    }

    #[test]
    fn the_status_bar_reports_the_cursor_position() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"one\ntwo\n");
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::plain(Key::Down));
        input::on_chord(&mut m, KeyChord::plain(Key::Right));

        assert!(render_to_string(&mut m).contains("2:2"), "one-based line:column for humans");
    }

    #[test]
    fn a_long_file_scrolls_to_keep_the_cursor_visible() {
        let fs = sample_fs();
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        fs.add_file("/proj/Cargo.toml", body.as_bytes());
        let mut m = opened_with_file(&fs);

        for _ in 0..60 {
            input::on_chord(&mut m, KeyChord::plain(Key::Down));
        }
        let frame = render_to_string(&mut m);
        assert!(frame.contains("line 61"), "the cursor's line is on screen");
        // The trailing space matters: it distinguishes "line 2" from "line 20".
        assert!(!frame.contains("line 2 "), "and the top of the file has scrolled away");
    }

    /// The viewport must only move when the cursor would leave it. Scrolling on *every*
    /// vertical step pins the cursor to one screen row and slides the file underneath —
    /// which still keeps the cursor "visible", so the test above cannot catch it.
    #[test]
    fn moving_within_the_viewport_scrolls_nothing() {
        let fs = sample_fs();
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        fs.add_file("/proj/Cargo.toml", body.as_bytes());
        let mut m = opened_with_file(&fs);

        for _ in 0..60 {
            input::on_chord(&mut m, KeyChord::plain(Key::Down));
        }
        let top_after_scrolling = m.active_buffer().unwrap().scroll_top();

        for _ in 0..5 {
            input::on_chord(&mut m, KeyChord::plain(Key::Up));
        }
        assert_eq!(
            m.active_buffer().unwrap().scroll_top(),
            top_after_scrolling,
            "the cursor moved up inside the viewport, so the text should have stayed put"
        );
    }

    #[test]
    fn scrolling_up_past_the_top_edge_moves_the_viewport_again() {
        let fs = sample_fs();
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        fs.add_file("/proj/Cargo.toml", body.as_bytes());
        let mut m = opened_with_file(&fs);

        for _ in 0..60 {
            input::on_chord(&mut m, KeyChord::plain(Key::Down));
        }
        for _ in 0..60 {
            input::on_chord(&mut m, KeyChord::plain(Key::Up));
        }
        assert_eq!(m.active_buffer().unwrap().scroll_top(), 0, "back at the top of the file");
        assert_eq!(m.active_buffer().unwrap().cursor_position().0, 0);
    }

    // --- open files follow the disk ------------------------------------------------

    #[test]
    fn an_open_file_reloads_when_it_changes_on_disk() {
        // An editor showing stale text after something else edited the file is lying
        // about the file — which is exactly what an agent writing directly produced.
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"before\n");
        let mut m = opened_with_file(&fs);
        assert_eq!(m.active_buffer().unwrap().text().to_string(), "before\n");

        fs.add_file("/proj/Cargo.toml", b"after\n");
        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/Cargo.toml")]));
        m.settle_fs_sync(&mut reader(&fs));

        assert_eq!(m.active_buffer().unwrap().text().to_string(), "after\n");
        assert_eq!(m.buffers.len(), 1, "reloaded in place, not opened twice");
    }

    #[test]
    fn unsaved_work_is_never_replaced_by_a_reload() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"before\n");
        let mut m = opened_with_file(&fs);
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));

        fs.add_file("/proj/Cargo.toml", b"after\n");
        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/Cargo.toml")]));
        m.settle_fs_sync(&mut reader(&fs));

        assert_eq!(
            m.active_buffer().unwrap().text().to_string(),
            "Xbefore\n",
            "the human's edits survive"
        );
        assert!(
            m.notification.as_ref().unwrap().contains("unsaved"),
            "and they are told, got {:?}",
            m.notification
        );
    }

    #[test]
    fn a_change_to_a_file_we_do_not_have_open_reloads_nothing() {
        let fs = sample_fs();
        fs.add_file("/proj/Cargo.toml", b"mine\n");
        let mut m = opened_with_file(&fs);

        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/proj/README.md")]));
        let queued = m.take_fs_requests();
        assert!(
            !queued.iter().any(|r| matches!(r, FsRequest::ReadFile { .. })),
            "no buffer to reload: {queued:?}"
        );
    }

    #[test]
    fn fs_events_before_a_workspace_is_open_are_ignored() {
        let mut m = Model::new();
        m.on_fs_event(FsEvent::Changed(vec![std::path::PathBuf::from("/whatever")]));
        assert!(m.explorer.is_none());
    }
}

#[cfg(test)]
mod agent_tests {
    //! The review loop through the *model*, driven by a scripted agent.
    //!
    //! Deliberately routed through `Model::on_agent_event` and `take_agent_requests` —
    //! the same functions the real ACP worker will feed (ADR-0007 §2). A test that talked
    //! to the agent some other way would be exercising a path the product never uses.

    use crate::model::Model;
    use crate::{input, view};
    use termesh_agent::{AgentRequest, AgentService};
    use termesh_core::input::{Key, KeyChord};
    use termesh_core::{
        Action, AgentEvent, AgentTerminalOperation, AgentTerminalRequestId, AgentTerminalResponse,
        AppMessage, Command, PermissionDecision, PermissionRequestId, PtyEvent, PtyRequest,
        ReadRequestId, SessionId, TaskOrigin, TaskSpec, TerminalExit, TerminalOwner, TerminalSpec,
    };
    use termesh_filesystem::{DirReader, FileSystemService};
    use termesh_test_support::{FakeFileSystem, ScriptedAgent, ScriptedUpdate};
    use termesh_ui::Pane;

    const ORIGINAL: &str = "fn main() {\n    todo!()\n}\n";
    const IMPROVED: &str = "fn main() {\n    println!(\"hi\");\n}\n";

    fn generation(value: u64) -> termesh_core::TerminalGeneration {
        termesh_core::TerminalGeneration::new(value)
    }

    fn workspace() -> FakeFileSystem {
        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml", "/proj/src/main.rs"]);
        fs.add_file("/proj/src/main.rs", ORIGINAL.as_bytes());
        fs
    }

    /// A model with `/proj/src/main.rs` open in the editor.
    fn opened(fs: &FakeFileSystem) -> Model {
        let mut m = Model::new();
        m.open_workspace_sync(fs, std::path::Path::new("/proj"));
        m.open_file_sync(fs, std::path::PathBuf::from("/proj/src/main.rs"));
        m
    }

    fn model_with_agent_workspace(session: SessionId) -> Model {
        let fs = workspace();
        let mut model = opened(&fs);
        model.agent_name = Some("scripted".into());
        model.on_agent_event(AgentEvent::SessionStarted { session });
        model
    }

    /// A workspace root the host platform agrees is absolute, for the grant tests.
    ///
    /// `/proj` is *rooted* on Windows but not *absolute* — that needs a drive prefix — and
    /// `CommandGrant::from_spec` rightly refuses a non-absolute cwd, because containment
    /// cannot be checked against one. The rest of this suite keeps the `/proj` fixture,
    /// which never reaches that check; only the tests that assert a grant need this.
    const GRANT_ROOT: &str = if cfg!(windows) { r"C:\proj" } else { "/proj" };

    fn grant_root() -> &'static std::path::Path {
        std::path::Path::new(GRANT_ROOT)
    }

    fn model_with_grant_workspace(session: SessionId) -> Model {
        let fs = FakeFileSystem::new();
        fs.add_file(grant_root().join("Cargo.toml"), b"[package]\nname = \"proj\"\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, grant_root());
        model.agent_name = Some("scripted".into());
        model.on_agent_event(AgentEvent::SessionStarted { session });
        model
    }

    fn grant_spec(program: &str, args: &[&str]) -> TerminalSpec {
        TerminalSpec {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: grant_root().to_path_buf(),
            env: Vec::new(),
        }
    }

    fn terminal_spec(program: &str, args: &[&str]) -> TerminalSpec {
        TerminalSpec {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: "/proj".into(),
            env: Vec::new(),
        }
    }

    fn cargo_test_task_spec() -> TerminalSpec {
        terminal_spec("cargo", &["test", "--message-format=json-diagnostic-rendered-ansi"])
    }

    #[test]
    fn agent_task_context_contains_the_same_exact_catalog() {
        let model = model_with_agent_workspace(SessionId::new(1));

        let context = model.agent_context();

        assert!(context.contains("task.run cargo.test (Test)"), "{context}");
        assert!(context.contains("program: cargo"), "{context}");
        assert!(
            context
                .contains("args: [\"test\", \"--message-format=json-diagnostic-rendered-ansi\"]"),
            "{context}"
        );
        assert!(
            context.contains(
                "invocation: ACP terminal/create with this exact program, args, and cwd; normal permission applies"
            ),
            "{context}"
        );
    }

    #[test]
    fn agent_task_catalog_match_does_not_bypass_permission() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);

        request_create(&mut model, session, 90, cargo_test_task_spec(), false);

        assert!(model.task_runs.is_empty());
        assert!(model.agent.as_ref().unwrap().pending_permission.is_some());
        assert!(model.take_pty_requests().is_empty());
    }

    #[test]
    fn agent_git_commands_keep_the_structured_terminal_permission_path() {
        let session = SessionId::new(1);
        let spec = TerminalSpec {
            program: "git".into(),
            args: vec!["commit".into(), "-m".into(), "agent message".into()],
            cwd: "/repo".into(),
            env: Vec::new(),
        };

        let mut rejected = model_with_agent_workspace(session);
        request_create(&mut rejected, session, 95, spec.clone(), false);
        assert!(rejected.take_pty_requests().is_empty());
        rejected.decide_permission(PermissionDecision::RejectOnce);
        assert!(rejected.take_pty_requests().is_empty());

        let mut approved = model_with_agent_workspace(session);
        request_create(&mut approved, session, 96, spec.clone(), false);
        assert!(approved.take_pty_requests().is_empty());
        approved.decide_permission(PermissionDecision::AllowOnce);
        assert!(matches!(
            approved.take_pty_requests().as_slice(),
            [PtyRequest::Spawn { spec: queued, .. }] if queued == &spec
        ));
    }

    #[test]
    fn agent_task_permitted_exact_command_becomes_a_task_run() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        request_create(&mut model, session, 91, cargo_test_task_spec(), false);

        model.decide_permission(PermissionDecision::AllowOnce);

        let run = model.task_runs.last().expect("exact catalog command is a task");
        assert_eq!(run.spec.id, "cargo.test");
        assert_eq!(run.origin, TaskOrigin::Agent { session });
        assert_eq!(model.terminals.last().unwrap().owner, TerminalOwner::Agent { session });
    }

    #[test]
    fn agent_task_near_match_remains_an_ordinary_terminal() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let mut spec = cargo_test_task_spec();
        spec.args.push("--workspace".into());
        request_create(&mut model, session, 92, spec, false);

        model.decide_permission(PermissionDecision::AllowOnce);

        assert!(model.task_runs.is_empty());
        assert_eq!(model.terminals.last().unwrap().owner, TerminalOwner::Agent { session });
    }

    #[test]
    fn agent_task_unsupported_workspace_reports_no_adapter() {
        let fs = FakeFileSystem::with_paths(&["/proj/package.json"]);
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        assert!(model.agent_context().contains("tasks: (no adapter)"));
    }

    #[test]
    fn agent_task_persistent_exact_grant_still_classifies_the_run() {
        let session = SessionId::new(1);
        let mut model = model_with_grant_workspace(session);
        let spec = grant_spec("cargo", &["test", "--message-format=json-diagnostic-rendered-ansi"]);
        let mut policy = termesh_workspace::PermissionPolicy::default();
        assert!(policy.remember(grant_root(), &spec));
        model.set_permission_policy(policy);

        request_create(&mut model, session, 93, spec, false);

        assert!(model.agent.as_ref().unwrap().pending_permission.is_none());
        assert_eq!(model.task_runs.last().unwrap().origin, TaskOrigin::Agent { session });
    }

    #[test]
    fn agent_task_and_human_task_receive_identical_decoded_output() {
        let fs = workspace();
        let task = TaskSpec {
            id: "cargo.test".into(),
            label: "Test".into(),
            program: "cargo".into(),
            args: vec!["test".into(), "--message-format=json-diagnostic-rendered-ansi".into()],
            cwd: "/proj".into(),
        };
        let mut human = opened(&fs);
        human.run_task(task);
        let human_requests = human.take_pty_requests();
        let [PtyRequest::Spawn { terminal: human_terminal, generation: human_generation, .. }] =
            human_requests.as_slice()
        else {
            panic!("human task should spawn once");
        };
        let (human_terminal, human_generation) = (*human_terminal, *human_generation);

        let session = SessionId::new(1);
        let mut agent = model_with_agent_workspace(session);
        request_create(&mut agent, session, 94, cargo_test_task_spec(), false);
        agent.decide_permission(PermissionDecision::AllowOnce);
        let agent_requests = agent.take_pty_requests();
        let [PtyRequest::Spawn { terminal: agent_terminal, generation: agent_generation, .. }] =
            agent_requests.as_slice()
        else {
            panic!("agent task should spawn once");
        };
        let (agent_terminal, agent_generation) = (*agent_terminal, *agent_generation);

        for (model, terminal, generation) in [
            (&mut human, human_terminal, human_generation),
            (&mut agent, agent_terminal, agent_generation),
        ] {
            model.on_pty_event(PtyEvent::Spawned { terminal, generation, process_id: Some(7) });
            let json = br#"{"reason":"compiler-message","message":{"rendered":"error[E0425]\n","level":"error","message":"cannot find value","spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true}]}}
"#;
            let split = json.len() / 2;
            model.on_pty_event(PtyEvent::Output {
                terminal,
                generation,
                bytes: json[..split].to_vec(),
            });
            model.on_pty_event(PtyEvent::Output {
                terminal,
                generation,
                bytes: json[split..].to_vec(),
            });
        }

        let human_capture = human.terminals.last().unwrap().capture.as_str().to_string();
        let agent_capture = agent.terminals.last().unwrap().capture.as_str().to_string();
        assert_eq!(agent_capture, human_capture);
        assert_eq!(
            agent.task_runs.last().unwrap().problems,
            human.task_runs.last().unwrap().problems
        );
        assert!(agent_capture.contains("error[E0425]"));
        assert!(!agent_capture.contains("compiler-message"));

        let output_request = AgentTerminalRequestId::new(95);
        agent.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: output_request,
            operation: AgentTerminalOperation::Output { terminal: agent_terminal },
        });
        assert!(agent.take_agent_requests().iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Output { output, .. },
            } if *request == output_request
                && output.contains("error[E0425]")
                && !output.contains("compiler-message")
        )));
    }

    fn script() -> ScriptedAgent {
        ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Message("Replacing the todo.".into()),
            ScriptedUpdate::ReadFile("/proj/src/main.rs".into()),
            ScriptedUpdate::Edit {
                path: "/proj/src/main.rs".into(),
                old_text: Some(ORIGINAL.into()),
                new_text: IMPROVED.into(),
            },
            ScriptedUpdate::End,
        ])
    }

    /// Pump the model and the agent until neither has anything left to say.
    fn settle(m: &mut Model, agent: &mut ScriptedAgent) {
        for _ in 0..32 {
            let requests = m.take_agent_requests();
            let events = agent.poll();
            if requests.is_empty() && events.is_empty() {
                return;
            }
            for request in requests {
                agent.send(request);
            }
            for event in events {
                m.on_agent_event(event);
            }
        }
        panic!("the agent loop did not settle");
    }

    /// Start a session and run one turn, typing the question the way a human would.
    fn run_turn(m: &mut Model, agent: &mut ScriptedAgent) {
        m.agent_name.get_or_insert_with(|| "scripted".into());
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(m, agent);
        ask(m, "do the thing");
        settle(m, agent);
    }

    /// Open the prompt overlay, type, and confirm.
    fn ask(m: &mut Model, question: &str) {
        m.dispatch(Command::Action(Action::AgentPrompt));
        for c in question.chars() {
            input::on_chord(m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(m, KeyChord::plain(Key::Enter));
    }

    /// An open session with nothing said yet must not render as a blank pane.
    /// ACP streams a sentence as many chunks. One transcript entry per chunk shattered
    /// the answer into fragments on screen — this is what that looked like.
    /// The bug this exists for: the transcript was trimmed to a character budget before
    /// the pane saw it, so a long answer *deleted* the question that prompted it. It
    /// looked like scrolling and was not — the text was gone, not off-screen.
    #[test]
    fn a_long_answer_never_swallows_the_question_that_prompted_it() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let long: String = (1..=200).map(|i| format!("answer line {i}. ")).collect();
        let mut agent = ScriptedAgent::new()
            .with_turn(vec![ScriptedUpdate::Message(long), ScriptedUpdate::End]);

        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);
        ask(&mut m, "what is this file about");
        settle(&mut m, &mut agent);

        // A tall pane, like a real terminal — where the old budget left nothing to scroll.
        view::snapshot(&mut m, 200, 60);
        assert!(m.agent_scroll_max > 0, "a 200-line answer must be scrollable in any pane");

        m.focus = Pane::Agent;
        for _ in 0..500 {
            input::on_chord(&mut m, KeyChord::plain(Key::PageUp));
        }
        let top = view::snapshot(&mut m, 200, 60);
        assert!(top.contains("what is this file about"), "the question survives:\n{top}");
        assert!(top.contains("answer line 1."), "and so does the start of the answer");
    }

    /// A long answer must be reachable, not just its tail.
    #[test]
    fn a_long_answer_can_be_scrolled_back_through() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let long: String = (1..=60).map(|i| format!("answer line {i}. ")).collect();
        let mut agent = ScriptedAgent::new()
            .with_turn(vec![ScriptedUpdate::Message(long), ScriptedUpdate::End]);
        run_turn(&mut m, &mut agent);

        let bottom = view::snapshot(&mut m, 96, 28);
        // The final words, not "line 60" — the pane wraps, so a phrase can straddle rows.
        assert!(bottom.contains("60."), "the newest is shown by default:\n{bottom}");
        assert!(m.agent_scroll_max > 0, "and there is more above it");
        assert!(bottom.contains('\u{2191}'), "the title says how much is hidden above");

        m.focus = Pane::Agent;
        for _ in 0..20 {
            input::on_chord(&mut m, KeyChord::plain(Key::Up));
        }
        let scrolled = view::snapshot(&mut m, 96, 28);
        assert_ne!(scrolled, bottom, "scrolling up shows something else");
        assert!(scrolled.contains("answer line 1."), "reaching the start of the answer");
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let long: String = (1..=60).map(|i| format!("answer line {i}. ")).collect();
        let mut agent = ScriptedAgent::new()
            .with_turn(vec![ScriptedUpdate::Message(long), ScriptedUpdate::End]);
        run_turn(&mut m, &mut agent);
        view::snapshot(&mut m, 96, 28);

        m.focus = Pane::Agent;
        for _ in 0..500 {
            input::on_chord(&mut m, KeyChord::plain(Key::Up));
        }
        assert_eq!(m.agent_scroll, m.agent_scroll_max, "cannot scroll past the start");

        for _ in 0..500 {
            input::on_chord(&mut m, KeyChord::plain(Key::Down));
        }
        assert_eq!(m.agent_scroll, 0, "nor past the end");
    }

    /// Scrolling up to read must not be yanked away by the agent still talking.
    #[test]
    fn a_new_turn_returns_the_view_to_the_latest() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let long: String = (1..=60).map(|i| format!("answer line {i}. ")).collect();
        let mut agent = ScriptedAgent::new()
            .with_turn(vec![ScriptedUpdate::Message(long), ScriptedUpdate::End])
            .with_turn(vec![ScriptedUpdate::Message("something new".into()), ScriptedUpdate::End]);
        run_turn(&mut m, &mut agent);
        view::snapshot(&mut m, 96, 28);

        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Up));
        assert!(m.agent_scroll > 0);

        ask(&mut m, "again");
        settle(&mut m, &mut agent);
        assert_eq!(m.agent_scroll, 0, "a new answer brings the view back to it");
    }

    #[test]
    fn streamed_chunks_become_one_readable_answer() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Message("This file ".into()),
            ScriptedUpdate::Message("describes the ".into()),
            ScriptedUpdate::Message("project.".into()),
            ScriptedUpdate::End,
        ]);
        run_turn(&mut m, &mut agent);

        let transcript = &m.agent.as_ref().unwrap().transcript;
        let answers: Vec<&str> =
            transcript.iter().map(|l| l.text.as_str()).filter(|t| t.contains("file")).collect();
        assert_eq!(
            answers,
            ["This file describes the project."],
            "three chunks, one sentence: {transcript:?}"
        );
    }

    #[test]
    fn the_question_and_the_answer_are_told_apart() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Thought("considering".into()),
            ScriptedUpdate::Message("the answer".into()),
            ScriptedUpdate::End,
        ]);
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);
        ask(&mut m, "my question");
        settle(&mut m, &mut agent);

        let frame = view::snapshot(&mut m, 96, 28);
        assert!(frame.contains("my question"), "the question is shown:\n{frame}");
        assert!(frame.contains("the answer"), "and so is the answer");
        // Reasoning is marked so it cannot be mistaken for the answer.
        assert!(frame.contains('\u{2022}'), "reasoning is marked apart");
    }

    #[test]
    fn a_fresh_session_still_says_what_to_do() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new();
        m.agent_name = Some("opencode".into());
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);
        assert!(m.agent.is_some());

        let frame = view::snapshot(&mut m, 96, 28);
        assert!(frame.contains("Session open"), "got:\n{frame}");
        assert!(frame.contains("F4"), "and how to use it");
    }

    /// The deadlock this exists for: the Agent context used to require a session, but
    /// Enter in that pane is how you *start* one — so the key that creates a session was
    /// unreachable until a session existed, and pressing it did nothing at all.
    #[test]
    fn enter_in_the_agent_pane_works_before_any_session_exists() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        assert!(m.agent.is_none(), "no session yet — the case that was broken");

        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        assert!(m.overlay_active(), "Enter should open the question box");
    }

    #[test]
    fn asking_works_from_every_advertised_route() {
        let fs = workspace();
        for chord in [KeyChord::plain(Key::F(4)), KeyChord::alt(Key::Char('i'))] {
            let mut m = opened(&fs);
            m.agent_name = Some("opencode".into());
            input::on_chord(&mut m, chord);
            assert!(m.overlay_active(), "{chord} should open the question box");
        }

        // And the palette, which needs no chord at all.
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        m.dispatch(Command::Action(Action::AgentPrompt));
        assert!(m.overlay_active());
    }

    /// Tab reaches the Agent pane, which is the route the pane itself advertises.
    #[test]
    fn tab_can_reach_the_agent_pane() {
        let fs = workspace();
        let mut m = opened(&fs);
        for _ in 0..4 {
            if m.focus == Pane::Agent {
                return;
            }
            input::on_chord(&mut m, KeyChord::plain(Key::Tab));
        }
        panic!("Tab never reached the Agent pane; focus stuck at {:?}", m.focus);
    }

    #[test]
    fn a_connected_agent_looks_different_from_no_agent_at_all() {
        // The bug this exists for: a working setup with no session yet rendered exactly
        // the same words as an unconfigured one, so it looked broken.
        let fs = workspace();
        let mut m = opened(&fs);
        let unconfigured = view::snapshot(&mut m, 96, 28);
        assert!(unconfigured.contains("No agent configured"), "got:\n{unconfigured}");
        assert!(unconfigured.contains("agents.toml"), "and says how to fix it");
        // The fallback is half the message and had no assertion, which is why 0.1.0 could
        // ship it as "Tier 0: ... (Phase 04)" — build vocabulary naming neither the key to
        // press nor the thing that happens. Someone with no ACP agent can still work.
        //
        // Match "press F6", not "F6": the status bar carries "F6 Terminal" on every frame,
        // so a bare "F6" is satisfied by a completely empty agent pane.
        assert!(unconfigured.contains("press F6"), "offers the fallback:\n{unconfigured}");
        assert!(unconfigured.contains("any AI CLI"), "and says what for:\n{unconfigured}");

        m.agent_name = Some("opencode".into());
        let connected = view::snapshot(&mut m, 96, 28);
        assert!(connected.contains("opencode"), "names the agent:\n{connected}");
        assert!(connected.contains("F4"), "and says what to press");
    }

    /// One key should do the obvious thing rather than demanding a session first.
    #[test]
    fn asking_with_no_session_opens_one_and_sends_the_question() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.agent_name = Some("opencode".into());
        let mut agent = script();

        ask(&mut m, "improve this");
        // The session is not open yet, so the turn is held rather than lost.
        settle(&mut m, &mut agent);

        assert!(m.agent.is_some(), "a session was opened for us");
        let sent = agent.sent();
        assert!(
            sent.iter()
                .any(|r| matches!(r, AgentRequest::Prompt { text, .. } if text == "improve this")),
            "and the held question was sent: {sent:?}"
        );
    }

    #[test]
    fn asking_with_no_agent_configured_says_so() {
        let fs = workspace();
        let mut m = opened(&fs);
        m.dispatch(Command::Action(Action::AgentPrompt));
        assert!(!m.overlay_active());
        assert!(m.notification.as_ref().unwrap().contains("agents.toml"));
    }

    #[test]
    fn a_session_starts_from_the_action_registry() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new();

        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);
        assert!(m.agent.is_some(), "one command surface: the palette starts the session");
    }

    #[test]
    fn the_agent_is_served_the_live_buffer_including_unsaved_edits() {
        let fs = workspace();
        let mut m = opened(&fs);
        // An unsaved change the disk knows nothing about.
        m.focus = Pane::Editor;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));

        let mut agent = script();
        run_turn(&mut m, &mut agent);

        let served = agent.served().first().expect("the agent asked for the file");
        assert!(
            served.1.as_ref().unwrap().starts_with('X'),
            "the agent must see what the human sees, got {:?}",
            served.1
        );
    }

    /// The product's central promise: an agent write is a proposal, not a disk write.
    /// We advertise `writeTextFile`, so an agent that uses it must get a review — if it
    /// gets "not supported" instead it writes the file itself, unreviewed, which is
    /// exactly what happened before this path existed.
    #[test]
    fn an_agent_write_becomes_a_reviewable_proposal_not_a_file_change() {
        let fs = workspace();
        let mut m = opened(&fs);
        let before = fs.read_file(std::path::Path::new("/proj/src/main.rs")).unwrap();

        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Write { path: "/proj/src/main.rs".into(), content: IMPROVED.into() },
            ScriptedUpdate::End,
        ]);
        run_turn(&mut m, &mut agent);

        let session = m.agent.as_ref().expect("a session");
        assert_eq!(session.proposals.len(), 1, "the write is a proposal");
        assert!(!session.proposals[0].has_conflicts(), "and it applies cleanly");
        assert_eq!(
            m.active_buffer().unwrap().text().to_string(),
            ORIGINAL,
            "the buffer is untouched until accepted"
        );
        assert_eq!(
            fs.read_file(std::path::Path::new("/proj/src/main.rs")).unwrap(),
            before,
            "and nothing reached disk"
        );

        // Accepting puts it in the buffer — still not on disk until saved.
        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('a')));
        assert_eq!(m.active_buffer().unwrap().text().to_string(), IMPROVED);
        assert_eq!(
            fs.read_file(std::path::Path::new("/proj/src/main.rs")).unwrap(),
            before,
            "accepting is not saving"
        );
    }

    /// A write says what the file should become, not what it was. Diffing against an
    /// empty base would turn every write into a whole-file rewrite.
    #[test]
    fn a_write_diffs_against_the_buffer_not_against_nothing() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Write { path: "/proj/src/main.rs".into(), content: IMPROVED.into() },
            ScriptedUpdate::End,
        ]);
        run_turn(&mut m, &mut agent);

        let proposal = &m.agent.as_ref().unwrap().proposals[0];
        assert_eq!(proposal.hunks.len(), 1, "one changed line, one hunk: {:?}", proposal.hunks);
    }

    #[test]
    fn a_proposal_becomes_reviewable_hunks_on_screen() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        run_turn(&mut m, &mut agent);

        let session = m.agent.as_ref().expect("a session");
        assert_eq!(session.proposals.len(), 1);
        assert!(!session.proposals[0].has_conflicts());

        let frame = view::snapshot(&mut m, 96, 28);
        assert!(frame.contains("Proposed"), "the pane announces it:\n{frame}");
        assert!(frame.contains('~') || frame.contains('+'), "and the gutter marks it");
    }

    /// The phase's exit criterion, end to end and through the keymap.
    #[test]
    fn prompt_review_accept_undo() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        run_turn(&mut m, &mut agent);

        // Review in the agent pane and accept with `a`.
        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('a')));
        settle(&mut m, &mut agent);

        assert_eq!(m.active_buffer().unwrap().text().to_string(), IMPROVED, "the edit applied");

        // One keystroke takes it all back.
        m.focus = Pane::Editor;
        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('z')));
        assert_eq!(
            m.active_buffer().unwrap().text().to_string(),
            ORIGINAL,
            "an accepted proposal undoes whole"
        );
    }

    #[test]
    fn rejecting_leaves_the_buffer_untouched() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        run_turn(&mut m, &mut agent);

        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('r')));
        settle(&mut m, &mut agent);

        assert_eq!(m.active_buffer().unwrap().text().to_string(), ORIGINAL);
        assert!(m.agent.as_ref().unwrap().proposals.is_empty(), "and the review is over");
    }

    /// ADR-0007 §8: the agent must not be told a partial accept succeeded.
    #[test]
    fn a_proposal_the_human_collided_with_is_reported_as_rejected() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        m.agent_name = Some("scripted".into());
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);
        ask(&mut m, "improve it");

        // The human edits the very line the agent is about to rewrite, before the
        // proposal lands.
        let at = ORIGINAL.find("todo").unwrap();
        m.active_buffer_mut()
            .unwrap()
            .edit(at, at + 4, "unimplemented", termesh_editor::EditSource::Keyboard)
            .unwrap();
        settle(&mut m, &mut agent);

        let session = m.agent.as_ref().unwrap();
        assert!(session.proposals[0].has_conflicts(), "the collision is caught");

        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('a')));

        let answered = m.take_agent_requests();
        assert!(
            answered.iter().any(|r| matches!(
                r,
                AgentRequest::Permission {
                    decision: termesh_core::PermissionDecision::RejectOnce,
                    ..
                }
            )),
            "a partial accept is reported as a rejection, got {answered:?}"
        );
        assert!(
            m.active_buffer().unwrap().text().to_string().contains("unimplemented"),
            "and the human's edit survives"
        );
    }

    /// The single-owner property, observed from outside: after the human types into a
    /// proposed change, the pane and the gutter must tell the same story. They used to be
    /// maintained separately, which is how a proposal ends up clean in one and conflicted
    /// in the other.
    #[test]
    fn the_agent_pane_and_the_gutter_never_disagree() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        run_turn(&mut m, &mut agent);

        assert!(!m.agent.as_ref().unwrap().proposals[0].has_conflicts());
        assert!(view::snapshot(&mut m, 96, 28).contains('~'), "clean to start with");

        // Type inside the very line the agent proposed to replace.
        m.focus = Pane::Editor;
        let at = ORIGINAL.find("todo").unwrap() + 2;
        m.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(at));
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));

        let conflicted = m.agent.as_ref().unwrap().proposals[0].has_conflicts();
        let frame = view::snapshot(&mut m, 96, 28);
        assert!(conflicted, "the proposal knows it collided");
        assert_eq!(conflicted, frame.contains('!'), "and the gutter says the same thing:\n{frame}");
    }

    /// A conflict is a statement about the current text, not a permanent verdict.
    #[test]
    fn undoing_the_collision_makes_the_proposal_applicable_again() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        run_turn(&mut m, &mut agent);

        m.focus = Pane::Editor;
        let at = ORIGINAL.find("todo").unwrap() + 2;
        m.active_buffer_mut().unwrap().set_selection(termesh_editor::Selection::point(at));
        input::on_chord(&mut m, KeyChord::plain(Key::Char('X')));
        assert!(m.agent.as_ref().unwrap().proposals[0].has_conflicts());

        input::on_chord(&mut m, KeyChord::ctrl(Key::Char('z')));
        assert!(
            !m.agent.as_ref().unwrap().proposals[0].has_conflicts(),
            "taking the edit back should make the proposal clean again"
        );
    }

    #[test]
    fn prompting_opens_an_input_box_rather_than_sending_a_canned_turn() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new();
        m.agent_name = Some("scripted".into());
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);

        m.dispatch(Command::Action(Action::AgentPrompt));
        assert!(m.overlay_active(), "the human types the question");
        assert!(m.take_agent_requests().is_empty(), "and nothing is sent until they confirm");

        for c in "fix the bug".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));

        let sent = m.take_agent_requests();
        assert!(
            sent.iter()
                .any(|r| matches!(r, AgentRequest::Prompt { text, .. } if text == "fix the bug")),
            "got {sent:?}"
        );
    }

    #[test]
    fn an_empty_question_is_not_sent() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new();
        m.agent_name = Some("scripted".into());
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);

        m.dispatch(Command::Action(Action::AgentPrompt));
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        assert!(m.take_agent_requests().is_empty());
        assert!(m.notification.is_some());
    }

    #[test]
    fn the_question_is_echoed_into_the_transcript() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = script();
        m.dispatch(Command::Action(Action::AgentSessionNew));
        settle(&mut m, &mut agent);

        m.dispatch(Command::Action(Action::AgentPrompt));
        for c in "rename it".chars() {
            input::on_chord(&mut m, KeyChord::plain(Key::Char(c)));
        }
        input::on_chord(&mut m, KeyChord::plain(Key::Enter));
        settle(&mut m, &mut agent);

        assert!(
            view::snapshot(&mut m, 96, 28).contains("rename it"),
            "the human should see what they asked"
        );
    }

    #[test]
    fn a_permission_request_shows_the_exact_command_before_it_runs() {
        let fs = workspace();
        let mut m = opened(&fs);
        let mut agent = ScriptedAgent::new().with_turn(vec![ScriptedUpdate::Permission {
            summary: "run the tests".into(),
            command: vec!["cargo".into(), "test".into()],
        }]);
        run_turn(&mut m, &mut agent);

        let frame = view::snapshot(&mut m, 96, 28);
        assert!(frame.contains("\"cargo\" \"test\""), "argv elements shown safely:\n{frame}");

        m.focus = Pane::Agent;
        input::on_chord(&mut m, KeyChord::plain(Key::Char('n')));
        let answered = m.take_agent_requests();
        assert!(answered.iter().any(|r| matches!(
            r,
            AgentRequest::Permission { decision: termesh_core::PermissionDecision::RejectOnce, .. }
        )));
    }

    #[test]
    fn managed_command_prompt_shows_program_args_cwd_and_environment() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let mut spec = terminal_spec("cargo", &["test name", "--exact"]);
        spec.env.push(("RUST_LOG".into(), "debug value".into()));
        request_create(&mut model, session, 8, spec, false);

        let frame = view::snapshot(&mut model, 120, 34);
        assert!(frame.contains("program: \"cargo\""), "{frame}");
        assert!(frame.contains("arg[0]: \"test name\""), "{frame}");
        assert!(frame.contains("arg[1]: \"--exact\""), "{frame}");
        assert!(frame.contains("cwd: /proj"), "{frame}");
        assert!(frame.contains("env \"RUST_LOG\"=\"debug value\""), "{frame}");
    }

    /// An agent command must not move the cursor out from under the user: focus follows
    /// what *the user* asked for. Otherwise a command spawning while they type sends the
    /// rest of their keystrokes to the agent's PTY stdin (ADR-0008 §3 makes terminal
    /// focus shell-first, so there is nothing downstream to catch this).
    #[test]
    fn an_agent_spawned_terminal_does_not_steal_focus() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let focus_before = model.focus;
        assert_ne!(focus_before, Pane::Terminal, "precondition: user is not in the terminal");

        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(40),
            operation: AgentTerminalOperation::Create {
                spec: terminal_spec("cargo", &["test"]),
                output_byte_limit: 4096,
                preauthorized: false,
            },
        });
        model.decide_permission(PermissionDecision::AllowOnce);

        assert_eq!(model.focus, focus_before, "the agent's terminal must not take focus");
    }

    /// The human routes still focus, or "open a terminal" would not open anything.
    #[test]
    fn a_user_opened_terminal_still_takes_focus() {
        let fs = workspace();
        let mut model = opened(&fs);

        model.dispatch(Command::Action(Action::TerminalFocus));

        assert_eq!(model.focus, Pane::Terminal);
    }

    /// The other half of "focus follows the user": when they are already *in* the
    /// terminal pane, an agent terminal must not yank their visible tab either. The
    /// agent still has to get a `Created` naming the terminal it actually asked for,
    /// which is the part that would break if anything resolved it via the active tab.
    #[test]
    fn an_agent_terminal_spawned_while_the_user_is_in_the_pane_keeps_their_tab() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        model.dispatch(Command::Action(Action::TerminalFocus)); // user opens their shell
        let theirs = model.active_terminal().expect("a shell is open").id;
        assert_eq!(model.focus, Pane::Terminal, "precondition: the user is in the pane");
        let _ = model.take_pty_requests();

        let request = AgentTerminalRequestId::new(70);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request,
            operation: AgentTerminalOperation::Create {
                spec: terminal_spec("cargo", &["test"]),
                output_byte_limit: 4096,
                preauthorized: true,
            },
        });
        let requests = model.take_pty_requests();
        let [PtyRequest::Spawn { terminal: spawned, .. }] = requests.as_slice() else {
            panic!("expected one spawn, got {requests:?}");
        };
        let spawned = *spawned;
        assert_ne!(spawned, theirs, "the agent got its own terminal");

        assert_eq!(
            model.active_terminal().map(|item| item.id),
            Some(theirs),
            "the user's visible tab must not change under them"
        );

        // The agent's reply must name its own terminal, not whatever tab is on screen.
        model.on_pty_event(PtyEvent::Spawned {
            terminal: spawned,
            generation: generation(1),
            process_id: Some(7),
        });
        assert!(
            model.take_agent_requests().iter().any(|response| matches!(
                response,
                AgentRequest::TerminalResponse {
                    request: replied,
                    response: AgentTerminalResponse::Created { terminal: created },
                } if *replied == request && *created == spawned
            )),
            "the Created response must name the agent's terminal"
        );
    }

    /// Only one prompt can be on screen, so a second request must be answered, not
    /// silently swallowed. Overwriting `pending_permission` dropped the first origin: a
    /// `terminal/create` awaiting approval never got a reply and the agent blocked
    /// forever. The `TerminalCreate` path already guarded this; the generic one did not.
    #[test]
    fn a_second_permission_request_is_rejected_rather_than_dropping_the_first() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let create_request = AgentTerminalRequestId::new(60);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: create_request,
            operation: AgentTerminalOperation::Create {
                spec: terminal_spec("cargo", &["test"]),
                output_byte_limit: 4096,
                preauthorized: false,
            },
        });
        assert!(model.agent.as_ref().unwrap().pending_permission.is_some());
        let _ = model.take_agent_requests();

        let second = PermissionRequestId::new(61);
        model.on_agent_event(AgentEvent::PermissionRequested {
            session,
            request: second,
            summary: "Delete everything?".into(),
            command: vec!["rm".into(), "-rf".into()],
            terminal_spec: None,
        });

        assert!(
            model.take_agent_requests().iter().any(|request| matches!(
                request,
                AgentRequest::Permission { request, decision }
                    if *request == second && !decision.allows()
            )),
            "the second request must get a rejection instead of evicting the first"
        );
        // And the terminal create is still live, so approving it still answers the agent.
        model.decide_permission(PermissionDecision::AllowOnce);
        let requests = model.take_pty_requests();
        assert!(
            matches!(requests.as_slice(), [PtyRequest::Spawn { .. }]),
            "the first request must survive, got {requests:?}"
        );
    }

    /// A request whose session does not resolve still needs an answer — dropping it
    /// leaves the agent waiting on a reply that will never come.
    #[test]
    fn a_permission_request_for_an_unknown_session_is_answered_not_dropped() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let _ = model.take_agent_requests();
        let stray = PermissionRequestId::new(62);

        model.on_agent_event(AgentEvent::PermissionRequested {
            session: SessionId::new(999),
            request: stray,
            summary: "Run something?".into(),
            command: vec!["ls".into()],
            terminal_spec: None,
        });

        assert!(
            model.take_agent_requests().iter().any(|request| matches!(
                request,
                AgentRequest::Permission { request, decision }
                    if *request == stray && !decision.allows()
            )),
            "an unresolved session must still be answered"
        );
    }

    /// `remove_terminal` reset the active tab to the *removed* index, which is only right
    /// when the closed tab was the active one. Closing an earlier tab therefore silently
    /// moved the user to a different session than the one they were watching.
    #[test]
    fn closing_an_earlier_tab_keeps_the_active_one_selected() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        for id in [50, 51, 52] {
            model.on_agent_event(AgentEvent::TerminalRequest {
                session,
                request: AgentTerminalRequestId::new(id),
                operation: AgentTerminalOperation::Create {
                    spec: terminal_spec("cargo", &["test"]),
                    output_byte_limit: 4096,
                    preauthorized: true,
                },
            });
        }
        let third = model.terminals[2].id;
        let first = model.terminals[0].id;
        assert_eq!(
            model.active_terminal().map(|item| item.id),
            Some(third),
            "precondition: the newest tab is the active one"
        );

        model.confirm_prompt(crate::model::Prompt {
            title: String::new(),
            input: String::new(),
            kind: crate::model::PromptKind::ConfirmCloseTerminal { terminal: first },
        });

        assert_eq!(
            model.active_terminal().map(|item| item.id),
            Some(third),
            "closing tab 1 must leave the user on the tab they were watching"
        );
    }

    /// A `Write` racing the child's exit fails EIO on the master, and the worker reports
    /// that as `Failed`. It must not retroactively turn a process that exited 0 into a
    /// failure — the agent reads the exit status from here, and would see `null`.
    #[test]
    fn a_late_write_failure_does_not_erase_a_recorded_exit() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(41),
            operation: AgentTerminalOperation::Create {
                spec: terminal_spec("cargo", &["test"]),
                output_byte_limit: 4096,
                preauthorized: true,
            },
        });
        let requests = model.take_pty_requests();
        let [PtyRequest::Spawn { terminal, .. }] = requests.as_slice() else {
            panic!("expected one spawn, got {requests:?}");
        };
        let terminal = *terminal;
        let exit = TerminalExit { code: Some(0), signal: None };
        model.on_pty_event(PtyEvent::Exited {
            terminal,
            generation: generation(1),
            exit: exit.clone(),
        });

        model.on_pty_event(PtyEvent::Failed {
            terminal,
            generation: generation(1),
            message: "PTY write failed: Input/output error".into(),
        });

        let wait = AgentTerminalRequestId::new(42);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: wait,
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        assert!(
            model.take_agent_requests().iter().any(|response| matches!(
                response,
                AgentRequest::TerminalResponse {
                    request,
                    response: AgentTerminalResponse::Exited(actual),
                } if *request == wait && actual == &exit
            )),
            "the agent must still see the real exit code, not a write error"
        );
    }

    #[test]
    fn approved_agent_command_runs_and_returns_output() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let create_request = AgentTerminalRequestId::new(9);
        let spec = terminal_spec("cargo", &["test"]);

        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: create_request,
            operation: AgentTerminalOperation::Create {
                spec: spec.clone(),
                output_byte_limit: 4096,
                preauthorized: false,
            },
        });
        assert!(model.agent.as_ref().unwrap().pending_permission.is_some());
        assert!(model.take_pty_requests().is_empty());

        model.decide_permission(PermissionDecision::AllowOnce);
        let requests = model.take_pty_requests();
        let [PtyRequest::Spawn { terminal, spec: queued, .. }] = requests.as_slice() else {
            panic!("expected one spawn, got {requests:?}");
        };
        assert_eq!(queued, &spec);
        let terminal = *terminal;

        model.on_pty_event(PtyEvent::Spawned {
            terminal,
            generation: generation(1),
            process_id: Some(12),
        });
        assert!(model.take_agent_requests().iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Created { terminal: created },
            } if *request == create_request && *created == terminal
        )));

        model.on_pty_event(PtyEvent::Output {
            terminal,
            generation: generation(1),
            bytes: b"test result: ok\r\n".to_vec(),
        });
        model.on_pty_event(PtyEvent::Exited {
            terminal,
            generation: generation(1),
            exit: TerminalExit { code: Some(0), signal: None },
        });

        let output_request = AgentTerminalRequestId::new(10);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: output_request,
            operation: AgentTerminalOperation::Output { terminal },
        });
        let wait_request = AgentTerminalRequestId::new(11);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: wait_request,
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        let responses = model.take_agent_requests();
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Output { output, exit: Some(exit), .. },
            } if *request == output_request
                && output.contains("test result: ok")
                && exit.code == Some(0)
        )));
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Exited(exit),
            } if *request == wait_request && exit.code == Some(0)
        )));
    }

    fn request_create(
        model: &mut Model,
        session: SessionId,
        request: u64,
        spec: TerminalSpec,
        preauthorized: bool,
    ) {
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(request),
            operation: AgentTerminalOperation::Create {
                spec,
                output_byte_limit: 4096,
                preauthorized,
            },
        });
    }

    fn spawned_agent_terminal(model: &mut Model, session: SessionId) -> termesh_core::TerminalId {
        request_create(model, session, 20, terminal_spec("cargo", &["test"]), true);
        let requests = model.take_pty_requests();
        let [PtyRequest::Spawn { terminal, .. }] = requests.as_slice() else {
            panic!("expected a spawn");
        };
        let terminal = *terminal;
        model.on_pty_event(PtyEvent::Spawned {
            terminal,
            generation: generation(1),
            process_id: Some(7),
        });
        let _ = model.take_agent_requests();
        terminal
    }

    #[test]
    fn rejected_agent_command_never_spawns() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        request_create(&mut model, session, 21, terminal_spec("cargo", &["test"]), false);

        model.decide_permission(PermissionDecision::RejectOnce);

        assert!(model.take_pty_requests().is_empty());
        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            }] if *request == AgentTerminalRequestId::new(21) && message == "command rejected"
        ));
    }

    #[test]
    fn preauthorized_agent_command_does_not_prompt_twice() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        request_create(&mut model, session, 22, terminal_spec("cargo", &["test"]), true);

        assert!(model.agent.as_ref().unwrap().pending_permission.is_none());
        assert!(matches!(model.take_pty_requests().as_slice(), [PtyRequest::Spawn { .. }]));
    }

    #[test]
    fn matching_workspace_policy_runs_without_a_prompt() {
        let session = SessionId::new(1);
        let mut model = model_with_grant_workspace(session);
        let spec = grant_spec("cargo", &["test"]);
        let mut policy = termesh_workspace::PermissionPolicy::default();
        assert!(policy.remember(grant_root(), &spec));
        model.set_permission_policy(policy);

        request_create(&mut model, session, 23, spec, false);

        assert!(model.agent.as_ref().unwrap().pending_permission.is_none());
        assert!(matches!(model.take_pty_requests().as_slice(), [PtyRequest::Spawn { .. }]));
    }

    #[test]
    fn allow_always_remembers_only_safe_workspace_commands() {
        let session = SessionId::new(1);
        let spec = grant_spec("cargo", &["test"]);
        let mut safe = model_with_grant_workspace(session);
        request_create(&mut safe, session, 33, spec.clone(), false);
        safe.decide_permission(PermissionDecision::AllowAlways);
        assert!(safe.permission_policy().is_dirty());
        assert!(safe.permission_policy().permits(grant_root(), &spec));
        assert!(matches!(safe.take_pty_requests().as_slice(), [PtyRequest::Spawn { .. }]));

        let mut unsafe_model = model_with_agent_workspace(session);
        let mut unsafe_spec = spec;
        unsafe_spec.env.push(("TOKEN".into(), "secret".into()));
        request_create(&mut unsafe_model, session, 34, unsafe_spec, false);
        unsafe_model.decide_permission(PermissionDecision::AllowAlways);
        assert!(!unsafe_model.permission_policy().is_dirty());
        assert!(unsafe_model
            .notification
            .as_deref()
            .is_some_and(|message| message.contains("unsafe to remember")));
        assert!(matches!(unsafe_model.take_pty_requests().as_slice(), [PtyRequest::Spawn { .. }]));
    }

    #[test]
    fn structured_execute_permission_can_record_the_later_terminal_grant() {
        let session = SessionId::new(1);
        let mut model = model_with_grant_workspace(session);
        let spec = grant_spec("cargo", &["test"]);
        model.on_agent_event(AgentEvent::PermissionRequested {
            session,
            request: PermissionRequestId::new(35),
            summary: "run tests".into(),
            command: vec!["cargo".into(), "test".into()],
            terminal_spec: Some(spec.clone()),
        });

        model.decide_permission(PermissionDecision::AllowAlways);

        assert!(model.permission_policy().permits(grant_root(), &spec));
        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::Permission {
                request,
                decision: PermissionDecision::AllowAlways,
            }] if *request == PermissionRequestId::new(35)
        ));
    }

    #[test]
    fn running_output_is_immediate_and_wait_is_completed_by_exit() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let terminal = spawned_agent_terminal(&mut model, session);
        model.on_pty_event(PtyEvent::Output {
            terminal,
            generation: generation(1),
            bytes: b"building\r\n".to_vec(),
        });

        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(24),
            operation: AgentTerminalOperation::Output { terminal },
        });
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(25),
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        let output = model.take_agent_requests();
        assert!(matches!(
            output.as_slice(),
            [AgentRequest::TerminalResponse {
                response: AgentTerminalResponse::Output { output, exit: None, .. },
                ..
            }] if output.contains("building")
        ));

        let exit = TerminalExit { code: Some(3), signal: None };
        model.on_pty_event(PtyEvent::Exited {
            terminal,
            generation: generation(1),
            exit: exit.clone(),
        });
        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Exited(actual),
            }] if *request == AgentTerminalRequestId::new(25) && actual == &exit
        ));
    }

    #[test]
    fn kill_acknowledges_without_releasing_but_release_invalidates() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let terminal = spawned_agent_terminal(&mut model, session);

        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(36),
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(26),
            operation: AgentTerminalOperation::Kill { terminal },
        });
        assert!(matches!(model.take_pty_requests().as_slice(), [PtyRequest::Kill { .. }]));
        let responses = model.take_agent_requests();
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Exited(TerminalExit { signal: Some(signal), .. }),
            } if *request == AgentTerminalRequestId::new(36) && signal == "killed"
        )));
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Acknowledged,
            } if *request == AgentTerminalRequestId::new(26)
        )));

        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(27),
            operation: AgentTerminalOperation::Release { terminal },
        });
        let effects = model.take_pty_requests();
        assert!(effects.contains(&PtyRequest::Kill { terminal, generation: generation(1) }));
        assert!(effects.contains(&PtyRequest::Release { terminal, generation: generation(1) }));
        assert!(model.terminals.iter().any(|item| item.id == terminal && item.released));

        let _ = model.take_agent_requests();
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(28),
            operation: AgentTerminalOperation::Output { terminal },
        });
        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::TerminalResponse { response: AgentTerminalResponse::Error(_), .. }]
        ));
    }

    #[test]
    fn spawn_failure_answers_the_create_request() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        request_create(&mut model, session, 29, terminal_spec("missing", &[]), true);
        let requests = model.take_pty_requests();
        let [PtyRequest::Spawn { terminal, .. }] = requests.as_slice() else {
            panic!("expected a spawn");
        };
        let terminal = *terminal;

        model.on_pty_event(PtyEvent::Failed {
            terminal,
            generation: generation(1),
            message: "not found".into(),
        });

        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            }] if *request == AgentTerminalRequestId::new(29) && message == "not found"
        ));
    }

    #[test]
    fn cancellation_and_shutdown_complete_waiters_and_release_processes() {
        let session = SessionId::new(1);
        let mut cancelled = model_with_agent_workspace(session);
        let terminal = spawned_agent_terminal(&mut cancelled, session);
        cancelled.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(30),
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        cancelled.on_agent_event(AgentEvent::TurnEnded {
            session,
            reason: termesh_core::StopReason::Cancelled,
        });
        assert!(cancelled.take_agent_requests().iter().any(
            |response| matches!(response, AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            } if *request == AgentTerminalRequestId::new(30) && message.contains("cancelled"))
        ));
        let effects = cancelled.take_pty_requests();
        assert!(effects.contains(&PtyRequest::Kill { terminal, generation: generation(1) }));
        assert!(effects.contains(&PtyRequest::Release { terminal, generation: generation(1) }));

        let mut shutting_down = model_with_agent_workspace(session);
        let terminal = spawned_agent_terminal(&mut shutting_down, session);
        shutting_down.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(31),
            operation: AgentTerminalOperation::WaitForExit { terminal },
        });
        shutting_down.shutdown_terminals();
        assert!(shutting_down.take_agent_requests().iter().any(
            |response| matches!(response, AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            } if *request == AgentTerminalRequestId::new(31) && message.contains("shutting down"))
        ));
    }

    #[test]
    fn cancellation_and_shutdown_reject_permission_held_terminal_creates() {
        let session = SessionId::new(1);
        let mut cancelled = model_with_agent_workspace(session);
        request_create(&mut cancelled, session, 37, terminal_spec("cargo", &["test"]), false);
        cancelled.on_agent_event(AgentEvent::TurnEnded {
            session,
            reason: termesh_core::StopReason::Cancelled,
        });
        assert!(cancelled.agent.as_ref().unwrap().pending_permission.is_none());
        assert!(cancelled.take_agent_requests().iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            } if *request == AgentTerminalRequestId::new(37) && message.contains("cancelled")
        )));
        cancelled.decide_permission(PermissionDecision::AllowOnce);
        assert!(cancelled.take_pty_requests().is_empty());

        let mut shutting_down = model_with_agent_workspace(session);
        request_create(&mut shutting_down, session, 38, terminal_spec("cargo", &["test"]), false);
        shutting_down.shutdown_terminals();
        assert!(shutting_down.agent.as_ref().unwrap().pending_permission.is_none());
        assert!(shutting_down.take_agent_requests().iter().any(|response| matches!(
            response,
            AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Error(message),
            } if *request == AgentTerminalRequestId::new(38) && message.contains("shutting down")
        )));
    }

    #[test]
    fn turn_cancellation_cancels_a_generic_acp_permission_prompt() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let request = PermissionRequestId::new(39);
        model.on_agent_event(AgentEvent::PermissionRequested {
            session,
            request,
            summary: "run a tool".into(),
            command: vec!["cargo".into(), "test".into()],
            terminal_spec: None,
        });

        model.on_agent_event(AgentEvent::TurnEnded {
            session,
            reason: termesh_core::StopReason::Cancelled,
        });

        assert!(matches!(
            model.take_agent_requests().as_slice(),
            [AgentRequest::PermissionCancelled { request: actual }] if *actual == request
        ));
    }

    #[test]
    fn terminal_attachment_is_retained_after_release() {
        let session = SessionId::new(1);
        let mut model = model_with_agent_workspace(session);
        let terminal = spawned_agent_terminal(&mut model, session);
        model.on_agent_event(AgentEvent::TerminalRequest {
            session,
            request: AgentTerminalRequestId::new(32),
            operation: AgentTerminalOperation::Release { terminal },
        });
        let _ = model.take_agent_requests();
        model.on_agent_event(AgentEvent::TerminalAttached { session, terminal });

        assert_eq!(model.agent.as_ref().unwrap().attached_terminals, [terminal]);
        assert!(model.terminals.iter().any(|item| item.id == terminal));
        let frame = view::snapshot(&mut model, 120, 34);
        assert!(frame.contains("retained in the Terminal pane"), "{frame}");
        assert!(frame.contains("released"), "{frame}");
    }

    #[test]
    fn agent_events_reach_the_model_through_the_app_message_channel() {
        // The path the real ACP worker will use: AppMessage::Agent, exactly as the
        // filesystem worker uses AppMessage::Fs.
        let message = AppMessage::Agent(termesh_core::AgentEvent::SessionStarted {
            session: termesh_core::SessionId::new(1),
        });
        let mut m = Model::new();
        match message {
            AppMessage::Agent(event) => m.on_agent_event(event),
            _ => unreachable!(),
        }
        assert!(m.agent.is_some());
    }

    fn workspace_with(files: &[(&str, &[u8])]) -> (Model, FakeFileSystem) {
        let paths: Vec<&str> = files.iter().map(|(path, _)| *path).collect();
        let fs = FakeFileSystem::with_paths(&paths);
        for (path, contents) in files {
            fs.add_file(path, contents);
        }
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        (model, fs)
    }

    fn start_session(model: &mut Model) -> SessionId {
        let session = SessionId::new(1);
        model.on_agent_event(AgentEvent::SessionStarted { session });
        session
    }

    fn settle_fs(model: &mut Model, fs: &FakeFileSystem) {
        let mut reader = DirReader::unfiltered(fs);
        model.settle_fs_sync(&mut reader);
    }

    fn one(requests: &[AgentRequest]) -> &AgentRequest {
        assert_eq!(requests.len(), 1, "{requests:?}");
        &requests[0]
    }

    #[test]
    fn the_agent_can_read_a_file_that_is_not_open_in_a_buffer() {
        // The whole point of shared context: the agent sees the project, not just
        // the tabs the human happens to have clicked on.
        let (mut model, fs) = workspace_with(&[("/proj/src/lib.rs", b"pub fn hidden() {}\n")]);
        let session = start_session(&mut model);

        model.on_agent_event(AgentEvent::ReadFileRequested {
            session,
            request: ReadRequestId::new(1),
            path: "/proj/src/lib.rs".into(),
        });
        settle_fs(&mut model, &fs);

        let served = model.take_agent_requests();
        let AgentRequest::FileContents { contents, .. } = one(&served) else { panic!() };
        assert_eq!(contents.as_deref(), Some("pub fn hidden() {}\n"));
    }

    #[test]
    fn an_unopened_read_does_not_claim_a_buffer_version() {
        // A file read off disk has no version we took, so the proposal must anchor by
        // content. Recording a served read here would let a rebase pretend otherwise.
        let (mut model, fs) = workspace_with(&[("/proj/src/lib.rs", b"x\n")]);
        let session = start_session(&mut model);
        model.on_agent_event(AgentEvent::ReadFileRequested {
            session,
            request: ReadRequestId::new(1),
            path: "/proj/src/lib.rs".into(),
        });
        settle_fs(&mut model, &fs);
        assert!(model.served_reads_for_test().is_empty());
    }

    #[test]
    fn a_read_outside_the_workspace_root_is_refused_with_a_reason() {
        let (mut model, _fs) = workspace_with(&[("/proj/src/lib.rs", b"x\n")]);
        let session = start_session(&mut model);
        model.on_agent_event(AgentEvent::ReadFileRequested {
            session,
            request: ReadRequestId::new(1),
            path: "/etc/passwd".into(),
        });
        let served = model.take_agent_requests();
        let AgentRequest::FileContents { contents, .. } = one(&served) else { panic!() };
        assert_eq!(contents, &None, "outside the root stays refused");
    }

    #[test]
    fn a_read_of_a_missing_file_names_the_path() {
        // The failure must be legible in the agent transcript, not a bare -32000.
        let (mut model, fs) = workspace_with(&[("/proj/src/lib.rs", b"x\n")]);
        let session = start_session(&mut model);
        model.on_agent_event(AgentEvent::ReadFileRequested {
            session,
            request: ReadRequestId::new(1),
            path: "/proj/src/gone.rs".into(),
        });
        settle_fs(&mut model, &fs);
        let served = model.take_agent_requests();
        let AgentRequest::FileContents { path, contents, .. } = one(&served) else { panic!() };
        assert_eq!(path, std::path::Path::new("/proj/src/gone.rs"));
        assert_eq!(contents, &None);
    }
}

#[cfg(test)]
mod headless_demo {
    //! `--dump-frame --agent-demo` is the CI-visible proof that the phase's whole point
    //! renders: a proposal becomes hunks and marks the gutter, checkable on any machine
    //! with no agent installed and no network.

    use crate::model::Model;
    use crate::{
        apply_color_choice, run_agent_demo, run_git_demo, run_java_demo, run_lsp_demo,
        run_polyglot_demo, run_search_task_demo, run_terminal_demo, view,
    };
    use termesh_platform::ColorDepth;
    use termesh_test_support::FakeFileSystem;
    use termesh_ui::Theme;

    #[test]
    fn dump_frame_color_16_selects_the_degraded_theme() {
        let args = crate::cli::Cli::parse(["--dump-frame", "--color=16"]);
        let mut model = Model::new();

        apply_color_choice(&mut model, args.color, ColorDepth::TrueColor);

        assert_eq!(model.theme.depth(), ColorDepth::Ansi16);
        assert!(matches!(args.mode, crate::cli::Mode::DumpFrame { .. }));
    }

    #[test]
    fn the_frame_stays_legible_without_any_color() {
        let mut diagnostic_model = Model::new();
        run_lsp_demo(&mut diagnostic_model);
        diagnostic_model.theme = Theme::for_depth(ColorDepth::None);
        let diagnostic_frame = view::snapshot(&mut diagnostic_model, 80, 24);
        assert!(
            diagnostic_frame.contains('E'),
            "the error gutter mark survives: {diagnostic_frame}"
        );

        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml", "/proj/src/main.rs"]);
        fs.add_file("/proj/src/main.rs", b"fn main() {}\nfn other() {}\n");
        let mut hunk_model = Model::new();
        hunk_model.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        hunk_model.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        run_agent_demo(&mut hunk_model);
        hunk_model.theme = Theme::for_depth(ColorDepth::None);
        let hunk_frame = view::snapshot(&mut hunk_model, 80, 24);
        assert!(hunk_frame.contains('~'), "the diff hunk mark survives: {hunk_frame}");
    }

    /// The README shows two frames and calls them real output. They were real when pasted;
    /// the mock they replaced was drawn by hand and had drifted away from the actual UI,
    /// which is the failure this guards against. Both demos run against synthetic
    /// workspaces, so their frames are byte-identical anywhere and can simply be compared.
    ///
    /// If this fails after a deliberate UI change, repaste from the command in the README —
    /// do not relax the assertion.
    #[test]
    fn the_readme_frames_are_still_what_the_demos_render() {
        let readme = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../README.md")
            .canonicalize()
            .expect("README.md sits at the workspace root");
        // Normalised because the repository has no `.gitattributes`, so Git for Windows
        // checks markdown out with CRLF while the rendered frame is always LF. Without
        // this the test is green on Unix and red on Windows for a reason that has nothing
        // to do with whether the README is accurate.
        let readme = std::fs::read_to_string(readme).unwrap().replace("\r\n", "\n");

        // `--dump-frame` renders at 96x28; the README copies have trailing spaces stripped
        // because editors and pre-commit hooks remove them from markdown anyway.
        let rendered = |demo: fn(&mut Model)| {
            let mut model = Model::new();
            demo(&mut model);
            view::snapshot(&mut model, 96, 28)
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
        };

        for (flag, demo) in
            [("--lsp-demo", run_lsp_demo as fn(&mut Model)), ("--git-demo", run_git_demo)]
        {
            let frame = rendered(demo);
            assert!(
                readme.contains(frame.trim_end()),
                "README no longer shows what `termesh --dump-frame . {flag}` renders. \
                 Current output:\n{frame}"
            );
        }
    }

    #[test]
    fn every_pane_still_renders_at_sixty_columns() {
        for depth in [ColorDepth::TrueColor, ColorDepth::Ansi16, ColorDepth::None] {
            for width in [60u16, 80, 120, 180] {
                let mut model = Model::new();
                run_polyglot_demo(&mut model);
                model.overlays.clear();
                run_terminal_demo(&mut model);
                model.theme = Theme::for_depth(depth);

                let frame = view::snapshot(&mut model, width, 24);

                assert!(!frame.contains('\u{fffd}'), "replacement char at {width}x24 {depth:?}");
                assert!(
                    frame.lines().all(|line| line.chars().count() <= width as usize),
                    "nothing overflows at {width} columns: {frame}"
                );
                for content in ["Cargo", "export const", "test result", "No agent"] {
                    assert!(
                        frame.contains(content),
                        "missing pane content {content:?} at {width}x24 {depth:?}: {frame}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_demo_turn_puts_review_hunks_on_screen() {
        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml", "/proj/src/main.rs"]);
        fs.add_file("/proj/src/main.rs", b"fn main() {}\nfn other() {}\n");

        let mut m = Model::new();
        m.open_workspace_sync(&fs, std::path::Path::new("/proj"));
        m.open_file_sync(&fs, std::path::PathBuf::from("/proj/src/main.rs"));
        run_agent_demo(&mut m);

        let frame = view::snapshot(&mut m, 96, 28);
        assert!(frame.contains("Proposed"), "the agent pane announces it:\n{frame}");
        assert!(frame.contains("1~"), "and the changed line is marked:\n{frame}");
        assert!(frame.contains("[a]ccept"), "with the review keys visible");
    }

    #[test]
    fn the_demo_says_what_it_needs_when_no_file_is_open() {
        let mut m = Model::new();
        run_agent_demo(&mut m);
        assert!(m.notification.as_ref().unwrap().contains("--open"));
    }

    #[test]
    fn terminal_demo_frame_contains_ansi_build_output() {
        let mut model = Model::new();
        run_terminal_demo(&mut model);

        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("cargo test"), "{frame}");
        assert!(frame.contains("test result: ok"), "{frame}");
        assert!(frame.contains("exited 0"), "{frame}");
    }

    #[test]
    fn search_task_demo_frame_contains_a_jumpable_failed_task() {
        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml", "/proj/src/lib.rs"]);
        fs.add_file("/proj/src/lib.rs", b"fn demo() {}\n");
        let mut model = Model::new();
        model.open_workspace_sync(&fs, std::path::Path::new("/proj"));

        run_search_task_demo(&mut model);

        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("Problems"), "{frame}");
        assert!(frame.contains("src/lib.rs:12:5"), "{frame}");
        assert!(frame.contains("cannot find value"), "{frame}");
        assert!(frame.contains("Test failed"), "{frame}");
        assert!(frame.contains("F8 Next"), "{frame}");
    }

    #[test]
    fn git_demo_renders_status_conflict_and_diff() {
        let mut model = Model::new();
        run_git_demo(&mut model);
        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("Git Changes") || frame.contains("Git Diff"), "{frame}");
        assert!(frame.contains("conflict.rs"), "{frame}");
        assert!(frame.contains("branch: main"), "{frame}");
    }

    #[test]
    fn lsp_demo_renders_diagnostics_and_a_hover() {
        let mut model = Model::new();
        run_lsp_demo(&mut model);
        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("mismatched types"), "{frame}");
        assert!(frame.contains("1E"), "{frame}");
    }

    #[test]
    fn the_lsp_demo_never_spawns_a_process() {
        let mut model = Model::new();
        run_lsp_demo(&mut model);
        assert!(model
            .take_lsp_requests()
            .iter()
            .all(|(_, request)| !matches!(request, termesh_core::LspRequest::Start { .. })));
    }

    #[test]
    fn polyglot_demo_shows_two_languages_and_their_tasks() {
        let mut model = Model::new();
        run_polyglot_demo(&mut model);
        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("app.ts"), "{frame}");
        assert!(frame.contains("npm"), "{frame}");
    }

    #[test]
    fn the_polyglot_demo_never_spawns_a_process() {
        let mut model = Model::new();
        run_polyglot_demo(&mut model);
        assert!(model
            .take_lsp_requests()
            .iter()
            .all(|(_, request)| !matches!(request, termesh_core::LspRequest::Start { .. })));
        assert!(model.take_pty_requests().is_empty());
    }

    #[test]
    fn java_demo_shows_a_diagnostic_and_maven_tasks() {
        let mut model = Model::new();
        run_java_demo(&mut model);
        let frame = view::snapshot(&mut model, 96, 28);
        assert!(frame.contains("App.java"), "{frame}");
        assert!(frame.contains("cannot find symbol"), "{frame}");
        assert!(frame.contains("maven: test"), "{frame}");
    }

    #[test]
    fn the_java_demo_never_spawns_a_process() {
        let mut model = Model::new();
        run_java_demo(&mut model);
        assert!(model
            .take_lsp_requests()
            .iter()
            .all(|(_, request)| !matches!(request, termesh_core::LspRequest::Start { .. })));
        assert!(model.take_pty_requests().is_empty());
    }
}

/// The Phase-06 exit criterion driven end to end: a real repository, the real
/// `GitService`, the real worker, and the real model — no injected events.
///
/// The discriminating case is **workspace root ≠ repository root**. Every Git command
/// runs at the repository root and every path in the snapshot is repository-relative, so
/// a workspace opened on a subdirectory is the one arrangement where a stray `join` or a
/// workspace-relative path would still look plausible in the fakes but stage the wrong
/// file — or nothing — against a real repository.
#[cfg(test)]
mod real_git_flow {
    use std::path::Path;
    use std::process::Command;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use termesh_core::{GitChangeKind, GitEvent};
    use termesh_git::{GitWorker, RealGitService};

    use crate::model::{Model, Overlay, Prompt, PromptKind};

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run everything the model asks for through the real worker until it stops asking.
    fn settle(model: &mut Model, worker: &GitWorker, events: &Receiver<GitEvent>) {
        loop {
            let requests = model.take_git_requests();
            if requests.is_empty() {
                return;
            }
            let mut outstanding = requests.len();
            for request in requests {
                assert!(worker.request(request), "the Git worker stopped early");
            }
            while outstanding > 0 {
                let event = events.recv_timeout(Duration::from_secs(60)).unwrap();
                // `Started` is progress, not an answer; every request ends in exactly one
                // of the other variants.
                let answered = !matches!(event, GitEvent::Started { .. });
                model.on_git_event(event);
                outstanding -= usize::from(answered);
            }
        }
    }

    fn select_row(model: &mut Model, path: &str) {
        let Some(Overlay::GitStatus(status)) = model.overlays.last_mut() else {
            panic!("the Git status overlay should be open");
        };
        let index = status
            .rows()
            .iter()
            .position(|row| row.path == Path::new(path))
            .unwrap_or_else(|| panic!("{path} is not in the status rows"));
        status.selected = index;
    }

    #[test]
    fn a_subdirectory_workspace_reviews_stages_and_commits_only_the_index() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let repository = std::env::temp_dir().join(format!("termesh-git-flow-{stamp}"));
        let workspace = repository.join("sub");
        std::fs::create_dir_all(&workspace).unwrap();
        git(&repository, &["init", "-q"]);
        git(&repository, &["config", "user.name", "Termesh Test"]);
        git(&repository, &["config", "user.email", "termesh@example.invalid"]);
        std::fs::write(workspace.join("tracked.rs"), "fn base() {}\n").unwrap();
        std::fs::write(repository.join("outside.txt"), "outside\n").unwrap();
        git(&repository, &["add", "--", "sub/tracked.rs", "outside.txt"]);
        git(&repository, &["commit", "-qm", "initial"]);
        // One edit inside the workspace, one outside it, and one brand-new file.
        std::fs::write(workspace.join("tracked.rs"), "fn changed() {}\n").unwrap();
        std::fs::write(repository.join("outside.txt"), "outside changed\n").unwrap();
        std::fs::write(workspace.join("untracked.rs"), "fn fresh() {}\n").unwrap();

        let (tx, events) = std::sync::mpsc::channel();
        let worker = GitWorker::spawn(RealGitService::new(), move |event| {
            let _ = tx.send(event);
        });

        let mut model = Model::new();
        model.open_workspace(termesh_workspace::WorkspaceRoot {
            path: workspace.clone(),
            kind: termesh_workspace::ProjectKind::Rust,
            kinds: vec![termesh_workspace::ProjectKind::Rust],
            detected: true,
        });
        settle(&mut model, &worker, &events);

        let snapshot = model.git.snapshot.clone().expect("a snapshot for a real repository");
        assert!(snapshot.workspace_root.ends_with("sub"), "{:?}", snapshot.workspace_root);
        assert_eq!(snapshot.workspace_root.parent(), Some(snapshot.repository_root.as_path()));
        // Repository-relative, not workspace-relative: this is the whole point of the case.
        assert!(snapshot.files.iter().any(|file| file.path == Path::new("sub/tracked.rs")));
        assert!(snapshot.files.iter().any(|file| file.path == Path::new("outside.txt")));
        // The context diff is scoped to the workspace, so the agent is not shown edits
        // from outside the project it was opened on.
        assert!(snapshot.context_diff.worktree.contains("tracked.rs"));
        assert!(!snapshot.context_diff.worktree.contains("outside.txt"));

        crate::model::Model::dispatch(
            &mut model,
            termesh_core::Command::Action(termesh_core::Action::GitShow),
        );
        settle(&mut model, &worker, &events);

        // Review before staging: the worktree diff of the real file.
        select_row(&mut model, "sub/tracked.rs");
        model.open_selected_git_diff();
        settle(&mut model, &worker, &events);
        let diff = model.git.diff.clone().expect("a worktree diff");
        assert!(diff.text.contains("-fn base() {}"), "{}", diff.text);
        assert!(diff.text.contains("+fn changed() {}"), "{}", diff.text);
        crate::input::on_chord(
            &mut model,
            termesh_core::input::KeyChord::plain(termesh_core::input::Key::Esc),
        );

        select_row(&mut model, "sub/tracked.rs");
        crate::input::on_chord(
            &mut model,
            termesh_core::input::KeyChord::plain(termesh_core::input::Key::Char('s')),
        );
        settle(&mut model, &worker, &events);
        let staged = model.git.snapshot.clone().unwrap();
        let tracked = staged
            .files
            .iter()
            .find(|file| file.path == Path::new("sub/tracked.rs"))
            .expect("the staged file survives the refresh");
        assert_eq!(tracked.index, Some(GitChangeKind::Modified));
        assert_eq!(tracked.worktree, None);

        model.confirm_prompt(Prompt {
            title: "Commit staged changes".into(),
            input: "phase 06 subdirectory".into(),
            kind: PromptKind::GitCommit,
        });
        settle(&mut model, &worker, &events);

        let committed = git_stdout(&repository, &["log", "-1", "--name-only", "--format="]);
        assert!(committed.contains("sub/tracked.rs"), "{committed}");
        assert!(!committed.contains("outside.txt"), "{committed}");
        assert!(!committed.contains("untracked.rs"), "{committed}");
        // The edit outside the workspace is untouched, in the worktree and on disk.
        assert_eq!(
            std::fs::read_to_string(repository.join("outside.txt")).unwrap(),
            "outside changed\n"
        );
        let after = model.git.snapshot.clone().unwrap();
        assert!(after.files.iter().all(|file| file.path != Path::new("sub/tracked.rs")));
        assert!(after.files.iter().any(|file| file.path == Path::new("outside.txt")
            && file.worktree == Some(GitChangeKind::Modified)));

        drop(worker);
        let _ = std::fs::remove_dir_all(&repository);
    }
}

#[cfg(test)]
mod settings_tests {
    use crate::model::Model;
    use crate::view;
    use std::path::{Path, PathBuf};
    use termesh_filesystem::DirReader;
    use termesh_test_support::FakeFileSystem;

    fn settle(model: &mut Model, fs: &FakeFileSystem) {
        let mut reader = DirReader::unfiltered(fs);
        model.settle_fs_sync(&mut reader);
    }

    #[test]
    fn config_reload_rereads_both_files_through_the_worker() {
        // config.reload must not block the render loop reading these itself — it queues
        // the same worker read every other file load uses (ADR-0014 Task 5).
        let (Some(config_path), Some(keymap_path)) =
            (termesh_platform::config_file(), termesh_platform::keymap_file())
        else {
            return; // no resolvable home/config dir in this environment; nothing to test
        };
        let fs = FakeFileSystem::new();
        fs.add_file(&config_path, b"version = 1\ntab_width = 8\n");
        fs.add_file(&keymap_path, b"version = 1\n\n[global]\n\"alt+g\" = \"git.show\"\n");

        let mut model = Model::new();
        model.dispatch(termesh_core::Command::Action(termesh_core::Action::ConfigReload));
        settle(&mut model, &fs);

        assert_eq!(model.settings.tab_width, 8);
        assert_eq!(
            model.keymap.resolve(
                &termesh_core::input::KeyChord::alt(termesh_core::input::Key::Char('g')),
                termesh_core::input::KeyContext::Global,
            ),
            Some(&termesh_core::Command::Action(termesh_core::Action::GitShow)),
        );
    }

    #[test]
    fn config_reload_with_no_files_on_disk_resets_to_defaults() {
        // A file deleted since the last load must not leave stale settings behind —
        // reload reflects what is on disk right now.
        let (Some(config_path), Some(_keymap_path)) =
            (termesh_platform::config_file(), termesh_platform::keymap_file())
        else {
            return;
        };
        let fs = FakeFileSystem::new();
        let mut model = Model::new();
        model.apply_settings("version = 1\ntab_width = 8\n", &config_path);
        assert_eq!(model.settings.tab_width, 8);

        model.dispatch(termesh_core::Command::Action(termesh_core::Action::ConfigReload));
        settle(&mut model, &fs);

        assert_eq!(model.settings, termesh_config::Settings::default());
    }

    #[test]
    fn non_utf8_config_uses_defaults_and_reports_the_file_and_fallback() {
        let Some(config_path) = termesh_platform::config_file() else { return };
        let fs = FakeFileSystem::new();
        fs.add_file(&config_path, &[0xff, 0xfe]);
        let mut model = Model::new();
        model.settings.tab_width = 8;

        model.dispatch(termesh_core::Command::Action(termesh_core::Action::ConfigReload));
        settle(&mut model, &fs);

        assert_eq!(model.settings, termesh_config::Settings::default());
        let notification = model.notification.as_deref().unwrap_or_default();
        assert!(notification.contains("config.toml"), "{notification}");
        assert!(notification.contains("UTF-8"), "{notification}");
        assert!(notification.contains("default"), "{notification}");
    }

    #[test]
    fn an_unreadable_config_uses_defaults_and_reports_why() {
        let Some(config_path) = termesh_platform::config_file() else { return };
        let fs = FakeFileSystem::new();
        fs.fail(&config_path, termesh_core::FsError::PermissionDenied(config_path.clone()));
        let mut model = Model::new();
        model.settings.tab_width = 8;

        model.dispatch(termesh_core::Command::Action(termesh_core::Action::ConfigReload));
        settle(&mut model, &fs);

        assert_eq!(model.settings, termesh_config::Settings::default());
        let notification = model.notification.as_deref().unwrap_or_default();
        assert!(notification.contains("permission denied"), "{notification}");
        assert!(notification.contains("default"), "{notification}");
    }

    #[test]
    fn reloading_exclusions_refreshes_an_already_loaded_tree() {
        let Some(config_path) = termesh_platform::config_file() else { return };
        let fs = FakeFileSystem::with_paths(&["/proj/keep.rs", "/proj/hidden.log"]);
        let mut model = Model::new();
        model.open_workspace_sync(&fs, Path::new("/proj"));
        assert!(view::snapshot(&mut model, 96, 28).contains("hidden.log"));

        fs.add_file(&config_path, b"version = 1\nexclusions = [\"*.log\"]\n");
        model.dispatch(termesh_core::Command::Action(termesh_core::Action::ConfigReload));
        settle(&mut model, &fs);

        let frame = view::snapshot(&mut model, 96, 28);
        assert!(!frame.contains("hidden.log"), "{frame}");
        assert!(frame.contains("keep.rs"), "{frame}");
    }

    #[test]
    fn a_malformed_config_file_surfaces_a_notification_naming_the_file() {
        let mut model = Model::new();
        model.apply_settings("tab_width = \n", Path::new("/home/.config/termesh/config.toml"));
        let notification = model.notification.as_ref().unwrap();
        assert!(notification.contains("config.toml"), "{notification}");
    }

    #[test]
    fn a_missing_config_file_is_not_a_diagnostic() {
        let mut model = Model::new();
        model.apply_settings("", Path::new("/home/.config/termesh/config.toml"));
        assert!(model.notification.is_none());
        assert_eq!(model.settings, termesh_config::Settings::default());
    }

    #[test]
    fn a_configured_shell_is_used_for_new_terminals() {
        let mut model = Model::new();
        model.apply_settings("version = 1\nshell = \"/bin/zsh\"\n", Path::new("/config.toml"));
        model.dispatch(termesh_core::Command::Action(termesh_core::Action::TerminalNew));
        assert_eq!(model.terminals[0].spec.program, "/bin/zsh");
    }

    #[test]
    fn a_keymap_file_rebinds_a_chord_on_the_live_model() {
        let mut model = Model::new();
        model.apply_keymap(
            "version = 1\n\n[global]\n\"alt+g\" = \"git.show\"\n",
            Path::new("/home/.config/termesh/keymap.toml"),
        );
        assert_eq!(
            model.keymap.resolve(
                &termesh_core::input::KeyChord::alt(termesh_core::input::Key::Char('g')),
                termesh_core::input::KeyContext::Global,
            ),
            Some(&termesh_core::Command::Action(termesh_core::Action::GitShow)),
        );
        assert!(model.notification.is_none());
    }

    #[test]
    fn reapplying_the_keymap_file_drops_a_binding_the_user_removed() {
        // A reload must overlay onto the compiled defaults, not onto whatever the
        // previous load left live — otherwise a deleted line in keymap.toml never takes
        // effect and the file stops being the source of truth (ADR-0014 §3).
        let mut model = Model::new();
        let path = Path::new("/home/.config/termesh/keymap.toml");
        model.apply_keymap("version = 1\n\n[global]\n\"alt+g\" = \"git.show\"\n", path);
        model.apply_keymap("version = 1\n", path);
        assert_eq!(
            model.keymap.resolve(
                &termesh_core::input::KeyChord::alt(termesh_core::input::Key::Char('g')),
                termesh_core::input::KeyContext::Global,
            ),
            None,
            "the binding removed from the file must not survive a reload"
        );
    }

    #[test]
    fn a_malformed_keymap_file_surfaces_a_notification_and_keeps_the_defaults() {
        let mut model = Model::new();
        model
            .apply_keymap("version = 1\n[global\n", Path::new("/home/.config/termesh/keymap.toml"));
        let notification = model.notification.as_ref().unwrap();
        assert!(notification.contains("keymap.toml"), "{notification}");
        assert_eq!(
            model.keymap.resolve(
                &termesh_core::input::KeyChord::plain(termesh_core::input::Key::F(10)),
                termesh_core::input::KeyContext::Global,
            ),
            Some(&termesh_core::Command::OpenPalette),
        );
    }

    #[test]
    fn a_valid_config_file_changes_the_tab_width_used_by_the_editor() {
        let fs = FakeFileSystem::with_paths(&["/proj/src/main.rs"]);
        fs.add_file("/proj/src/main.rs", b"\tx\n");

        let mut default_width = Model::new();
        default_width.open_workspace_sync(&fs, Path::new("/proj"));
        default_width.open_file_sync(&fs, PathBuf::from("/proj/src/main.rs"));
        let default_frame = view::snapshot(&mut default_width, 96, 28);

        let mut narrow_tab = Model::new();
        narrow_tab.apply_settings("version = 1\ntab_width = 1\n", Path::new("/config.toml"));
        narrow_tab.open_workspace_sync(&fs, Path::new("/proj"));
        narrow_tab.open_file_sync(&fs, PathBuf::from("/proj/src/main.rs"));
        let narrow_frame = view::snapshot(&mut narrow_tab, 96, 28);

        assert_ne!(default_frame, narrow_frame, "tab_width must change what renders");
    }
}
