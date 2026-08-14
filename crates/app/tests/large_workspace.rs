//! Algorithmic large-workspace guardrails (ARCHITECTURE.md §19).
//!
//! The binary crate has no public library surface, so this integration target compiles
//! the state modules in-place. Their unused UI entry points are expected in this narrow
//! harness; production is still checked with warnings denied by the workspace gate.
#![allow(dead_code)]

#[path = "../src/git_state.rs"]
mod git_state;
#[path = "../src/lsp_state.rs"]
mod lsp_state;
#[path = "../src/model.rs"]
mod model;
#[path = "../src/search_state.rs"]
mod search_state;
#[path = "../src/task_state.rs"]
mod task_state;

use std::path::Path;
use std::time::{Duration, Instant};

use model::Model;
use termesh_core::{Command, TerminalSize};
use termesh_filesystem::{DirReader, FileSystemService};
use termesh_terminal::TerminalScreen;
use termesh_test_support::{synthetic_tree, CountingFileSystem};

fn opened_big_workspace(fs: &CountingFileSystem) -> Model {
    let mut model = Model::new();
    model.open_workspace_sync(fs, Path::new("/big"));
    model
}

fn settle(model: &mut Model, fs: &CountingFileSystem) {
    let mut reader = DirReader::new(fs, Path::new("/big"), model.ignore_options);
    model.settle_fs_sync(&mut reader);
}

#[test]
fn rendering_one_tree_level_does_not_walk_the_whole_repository() {
    let fs = CountingFileSystem::new(synthetic_tree(8, 12));

    let _model = opened_big_workspace(&fs);

    assert!(fs.read_dir_calls() < 4, "one level, not the tree: {}", fs.read_dir_calls());
}

#[test]
fn expanding_a_directory_reads_only_that_directory() {
    let fs = CountingFileSystem::new(synthetic_tree(8, 12));
    let mut model = opened_big_workspace(&fs);
    model.dispatch(Command::ExplorerNext);
    let before = fs.read_dir_calls();

    model.dispatch(Command::ExplorerToggle);
    settle(&mut model, &fs);

    assert_eq!(fs.read_dir_calls() - before, 1);
}

#[test]
fn a_very_large_file_is_not_syntax_highlighted() {
    let fs = CountingFileSystem::new(synthetic_tree(8, 12));
    fs.write_file(Path::new("/big/big.rs"), "x\n".repeat(200_000).as_bytes()).unwrap();
    let mut model = opened_big_workspace(&fs);

    model.open_file_sync(&fs, "/big/big.rs".into());

    assert!(model.active_buffer().unwrap().decorations().is_empty(), "HIGHLIGHT_LIMIT holds");
}

#[test]
fn terminal_scrollback_stays_bounded_under_a_flood() {
    let mut screen = TerminalScreen::new(TerminalSize { rows: 24, cols: 80 });
    for line in 0..100_000 {
        screen.feed(format!("line {line}\n").as_bytes());
    }
    assert!(screen.history_size() <= 10_000);
}

/// §19's 150 ms warm start, measured against the fixture above.
///
/// Read this for what it is: the fixture materialises one spine, so the number below is
/// the cost of opening a workspace *once laziness is working*, not the cost of a
/// hundred-thousand-file repository. The claim that the second follows from the first is
/// carried by the call-counting tests above, which is where it belongs — a timer cannot
/// distinguish a lazy walk from a fast one.
///
/// Ignored by default: a wall-clock assertion on a shared CI runner fails for reasons
/// that have nothing to do with this code, and a gate that cries wolf gets muted.
#[test]
#[ignore = "timing; run with --ignored on a quiet machine"]
fn opening_a_workspace_stays_within_the_warm_start_budget() {
    let started = Instant::now();
    let _model = opened_big_workspace(&CountingFileSystem::new(synthetic_tree(8, 12)));
    assert!(started.elapsed() < Duration::from_millis(150), "{:?}", started.elapsed());
}
