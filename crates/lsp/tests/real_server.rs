use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use termesh_core::{LspEvent, LspRequest};
use termesh_lsp::{recipe_for, LanguageService, LspSession};

fn rust_analyzer_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

struct TemporaryWorkspace(PathBuf);

impl TemporaryWorkspace {
    fn new() -> Self {
        let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir()
            .join(format!("termesh-lsp-real-server-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"termesh_lsp_probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn valid() {}\n").unwrap();
        Self(root)
    }
}

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_real_server_hand_shakes_and_reports_diagnostics() {
    if !rust_analyzer_available() {
        return; // CI has no rust-analyzer; the scripted double carries the coverage.
    }

    let workspace = TemporaryWorkspace::new();
    let recipe = recipe_for("rust").unwrap();
    let (events_tx, events) = mpsc::channel();
    let mut session = LspSession::spawn(&recipe, &workspace.0, move |event| {
        let _ = events_tx.send(event);
    })
    .expect("rust-analyzer was probed immediately before spawning");

    loop {
        match events.recv_timeout(Duration::from_secs(30)).expect("language server handshake") {
            LspEvent::Ready => break,
            LspEvent::Failed { failure, .. } => panic!("handshake failed: {}", failure.message),
            _ => {}
        }
    }

    let path = workspace.0.join("src/lib.rs");
    session.send(LspRequest::DidOpen {
        path: path.clone(),
        language_id: "rust".into(),
        version: 1,
        text: "pub fn broken( {\n".into(),
    });

    loop {
        match events.recv_timeout(Duration::from_secs(30)).expect("diagnostics") {
            LspEvent::Diagnostics { path: event_path, items, .. }
                if event_path == path && !items.is_empty() =>
            {
                break;
            }
            LspEvent::Failed { failure, .. } => {
                panic!("language server failed: {}", failure.message)
            }
            _ => {}
        }
    }

    session.send(LspRequest::Shutdown);
}
