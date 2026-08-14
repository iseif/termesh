//! Project tree model, file watching, mutations, ignore rules. Phase 02.
//!
//! The `FileSystemService` trait here is the workspace's only door to the real
//! filesystem (ARCHITECTURE.md §7.4). Its shape — synchronous methods, called from a
//! worker thread rather than the render loop — is fixed by ADR-0005 §3 and is the
//! template every later service copies.
#![forbid(unsafe_code)]

pub mod ignore_rules;
pub mod reader;
pub mod real;
pub mod service;
pub mod tree;
pub mod watch;
pub mod worker;

pub use ignore_rules::{matches_exclusion, IgnoreOptions, IgnoreRules};
pub use reader::DirReader;
pub use real::RealFileSystem;
pub use service::{sort_entries, DirEntryInfo, EntryKind, FileSystemService, FsError, FsResult};
pub use tree::{ChildState, FileTree, Node, Row};
pub use watch::{Coalescer, RootWatcher};
pub use worker::FsWorker;
