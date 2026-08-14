//! [`Buffer`] — a rope-backed document and the only thing that applies transactions.
//!
//! This is the chokepoint ARCHITECTURE.md §8 asks for. Nothing above this type mutates
//! text: callers hand over an [`EditTransaction`] and the buffer validates it, computes
//! the inverse while the pre-image is still live, applies it, bumps the version, carries
//! the selection, and records the undo step. A transaction authored against a version the
//! buffer has moved past is rejected here rather than silently corrupting the document —
//! which is what makes an *asynchronous* agent safe to accept edits from.

use std::path::{Path, PathBuf};

use ropey::Rope;
use termesh_core::BufferId;
use termesh_filesystem::{FileSystemService, FsError};

use crate::change::ChangeSet;
use crate::decoration::DecorationSet;
use crate::history::History;
use crate::movement;
use crate::selection::Selection;
use crate::transaction::{EditSource, EditTransaction, Version};

/// How a file's lines were terminated on disk.
///
/// The rope always holds `\n` so every offset calculation in the editor sees one
/// character per line break. The original ending is remembered and restored on save, so
/// opening a CRLF file and pressing save does not rewrite every line of somebody's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    /// What the file mostly used. A mixed file is normalized to the dominant ending —
    /// picking per-line would mean tracking an ending per line for no practical gain.
    fn detect(text: &str) -> Self {
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count() - crlf;
        if crlf > lf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }
}

/// Why a transaction was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// Authored against a revision the buffer has moved past. Agent proposals hit this
    /// when the human has typed since; the fix is to rebase, never to force.
    StaleVersion {
        expected: Version,
        found: Version,
    },
    /// The changeset describes a document of a different size. A programming error —
    /// the changeset and the buffer were never the same document.
    LengthMismatch {
        expected: usize,
        found: usize,
    },
    /// The file is not valid UTF-8. V1 edits UTF-8 only (ARCHITECTURE.md §10).
    NotUtf8(PathBuf),
    Fs(FsError),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::StaleVersion { expected, found } => write!(
                f,
                "edit was written against version {} but the buffer is at {}",
                expected.0, found.0
            ),
            EditError::LengthMismatch { expected, found } => {
                write!(f, "edit expects a {expected}-char document, buffer has {found}")
            }
            EditError::NotUtf8(p) => write!(f, "not valid UTF-8: {}", p.display()),
            EditError::Fs(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<FsError> for EditError {
    fn from(e: FsError) -> Self {
        EditError::Fs(e)
    }
}

pub type EditResult<T> = Result<T, EditError>;

/// A text buffer: the document, where the cursor is, and how it got here.
#[derive(Debug)]
pub struct Buffer {
    id: BufferId,
    /// `None` for an untitled buffer that has never been saved.
    path: Option<PathBuf>,
    text: Rope,
    version: Version,
    selection: Selection,
    history: History,
    line_ending: LineEnding,
    /// The version last written to disk. `None` when nothing has been saved yet.
    saved_version: Option<Version>,
    /// The column vertical motion is aiming for, so stepping through a short line and
    /// back returns the cursor to where it started. Cleared by anything horizontal.
    goal_column: Option<usize>,
    /// Syntax, diagnostic, and agent-hunk overlays. Carried forward on every applied
    /// transaction, so a pending proposal stays anchored to the code it describes while
    /// the human keeps typing (ADR-0006 §3).
    decorations: DecorationSet,
    /// Changes already applied locally but not yet drained for document sync.
    pending_changes: Vec<ChangeSet>,
    /// First visible line.
    ///
    /// Remembered rather than derived from the cursor: a viewport computed purely from
    /// `(cursor, height)` pins the cursor to one screen row and slides the file beneath
    /// it. Scrolling has to be the *minimum* move that keeps the cursor on screen, and
    /// "minimum" needs to know where the viewport already was.
    scroll_top: usize,
}

impl Buffer {
    /// An empty, untitled buffer.
    pub fn new(id: BufferId) -> Self {
        Self {
            id,
            path: None,
            text: Rope::new(),
            version: Version::default(),
            selection: Selection::default(),
            history: History::new(),
            line_ending: LineEnding::default(),
            saved_version: None,
            goal_column: None,
            decorations: DecorationSet::new(),
            pending_changes: Vec::new(),
            scroll_top: 0,
        }
    }

    /// A buffer over text already in hand. Used by tests and by the agent's view of a
    /// file it supplied; [`Buffer::load`] is the path for files on disk.
    pub fn from_text(id: BufferId, path: Option<PathBuf>, text: &str) -> Self {
        let line_ending = LineEnding::detect(text);
        Self {
            id,
            path,
            text: Rope::from_str(&text.replace("\r\n", "\n")),
            version: Version::default(),
            selection: Selection::default(),
            history: History::new(),
            line_ending,
            saved_version: Some(Version::default()),
            goal_column: None,
            decorations: DecorationSet::new(),
            pending_changes: Vec::new(),
            scroll_top: 0,
        }
    }

    /// Read a file through the service boundary — never `std::fs` (CONTRIBUTING.md invariants).
    pub fn load(id: BufferId, fs: &dyn FileSystemService, path: &Path) -> EditResult<Self> {
        let bytes = fs.read_file(path)?;
        let text = String::from_utf8(bytes).map_err(|_| EditError::NotUtf8(path.to_path_buf()))?;
        Ok(Self::from_text(id, Some(path.to_path_buf()), &text))
    }

    /// Write the buffer back, restoring the line ending it arrived with.
    ///
    /// Saving ends the current undo group: a save is a boundary the user thinks in, so
    /// typing afterwards should not merge into what was already written out.
    pub fn save(&mut self, fs: &dyn FileSystemService) -> EditResult<()> {
        let path = self.path.clone().ok_or_else(|| {
            EditError::Fs(FsError::Other {
                path: PathBuf::new(),
                message: "buffer has no path; save-as is not wired up yet".into(),
            })
        })?;

        fs.write_file(&path, self.to_disk_string().as_bytes())?;
        self.saved_version = Some(self.version);
        self.history.break_group();
        Ok(())
    }

    /// The document as it would be written out, with the on-disk line ending restored.
    pub fn to_disk_string(&self) -> String {
        match self.line_ending {
            LineEnding::Lf => self.text.to_string(),
            LineEnding::Crlf => self.text.to_string().replace('\n', "\r\n"),
        }
    }

    pub fn id(&self) -> BufferId {
        self.id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Move the cursor. Ends the current undo group, because a deliberate move is where
    /// a user expects one undo step to stop and the next to begin.
    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.history.break_group();
    }

    /// Whether there are changes not yet written to disk.
    pub fn is_dirty(&self) -> bool {
        self.saved_version != Some(self.version)
    }

    /// The name to show on a tab.
    pub fn display_name(&self) -> String {
        match &self.path {
            Some(p) => p.file_name().unwrap_or(p.as_os_str()).to_string_lossy().into_owned(),
            None => "untitled".to_string(),
        }
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Drain the changes this buffer has not yet reported for document sync.
    ///
    /// Captured at mutation time because it cannot be reconstructed afterwards:
    /// `History::Entry` records neither a version nor a source.
    pub fn take_pending_changes(&mut self) -> Vec<ChangeSet> {
        std::mem::take(&mut self.pending_changes)
    }

    /// Build a transaction against the current state, for `source`.
    ///
    /// Goes through [`History::group_for`], so a run of typing coalesces and anything
    /// else gets its own undo step without the caller having to know the policy.
    pub fn transaction(&mut self, changes: ChangeSet, source: EditSource) -> EditTransaction {
        let group = self.history.group_for(&source);
        EditTransaction::new(self.id, self.version, changes, source, group)
    }

    /// Apply a transaction — the single path by which this document ever changes.
    ///
    /// Rejects anything authored against a different revision or a different-sized
    /// document. That check is the whole reason agent edits are safe: a proposal written
    /// while the human was typing cannot land blind, it lands rebased or not at all.
    pub fn apply(&mut self, transaction: &EditTransaction) -> EditResult<()> {
        if transaction.base_version != self.version {
            return Err(EditError::StaleVersion {
                expected: transaction.base_version,
                found: self.version,
            });
        }
        if transaction.changes.len_before() != self.text.len_chars() {
            return Err(EditError::LengthMismatch {
                expected: transaction.changes.len_before(),
                found: self.text.len_chars(),
            });
        }
        if transaction.is_empty() {
            return Ok(());
        }

        // Computed here, while `self.text` is still the pre-image (ADR-0006 §6).
        let inverse = transaction.changes.invert(&self.text);

        self.text = transaction.changes.apply(&self.text);
        self.version = self.version.next();
        // Overlays ride the edit. Pending hunks conflict rather than vanish; derived
        // spans are dropped for their producer to regenerate (see `DecorationSet::map`).
        self.decorations.map(&transaction.changes);
        self.selection = match &transaction.selection {
            Some(explicit) => explicit.clone(),
            None => self.selection.map(&transaction.changes),
        };
        self.pending_changes.push(transaction.changes.clone());
        self.history.push(transaction, inverse);
        Ok(())
    }

    /// Convenience for the common shape: replace `from..to` with `insert`.
    pub fn edit(
        &mut self,
        from: usize,
        to: usize,
        insert: &str,
        source: EditSource,
    ) -> EditResult<()> {
        let changes = ChangeSet::replace(self.text.len_chars(), from, to, insert);
        let transaction = self.transaction(changes, source);
        self.apply(&transaction)
    }

    /// Record that `version` reached disk.
    ///
    /// The async save path: the write happens on the worker thread, so by the time it
    /// succeeds the user may have typed again. Comparing versions rather than clearing a
    /// flag means a buffer edited mid-write stays correctly dirty.
    pub fn mark_saved(&mut self, version: Version) {
        self.saved_version = Some(version);
        self.history.break_group();
    }

    // --- cursor motion ------------------------------------------------------------
    //
    // Single cursor for V1 (ARCHITECTURE.md §10), so these drive the primary range. The
    // rules themselves live in `movement`, as pure functions over the rope.

    fn cursor(&self) -> usize {
        self.selection.primary().head
    }

    /// Put the cursor at `pos`, forgetting any sticky column.
    fn place_cursor(&mut self, pos: usize) {
        self.goal_column = None;
        self.set_selection(Selection::point(pos));
    }

    pub fn move_left(&mut self) {
        let pos = movement::left(&self.text, self.cursor());
        self.place_cursor(pos);
    }

    pub fn move_right(&mut self) {
        let pos = movement::right(&self.text, self.cursor());
        self.place_cursor(pos);
    }

    pub fn move_line_start(&mut self) {
        let pos = movement::line_start(&self.text, self.cursor());
        self.place_cursor(pos);
    }

    pub fn move_line_end(&mut self) {
        let pos = movement::line_end(&self.text, self.cursor());
        self.place_cursor(pos);
    }

    /// Move a line up or down, keeping the sticky column so a short line in between does
    /// not permanently drag the cursor left.
    pub fn move_line(&mut self, down: bool) {
        let cursor = self.cursor();
        let goal = self.goal_column.or_else(|| Some(movement::column_of(&self.text, cursor)));
        let pos = if down {
            movement::down(&self.text, cursor, goal)
        } else {
            movement::up(&self.text, cursor, goal)
        };
        self.set_selection(Selection::point(pos));
        self.goal_column = goal;
    }

    /// The cursor as a `(line, column)` pair, for the status bar and the renderer.
    pub fn decorations(&self) -> &DecorationSet {
        &self.decorations
    }

    pub fn decorations_mut(&mut self) -> &mut DecorationSet {
        &mut self.decorations
    }

    /// The char range of `line`, for clipping decorations to it.
    pub fn line_range(&self, line: usize) -> (usize, usize) {
        if line >= self.text.len_lines() {
            let end = self.text.len_chars();
            return (end, end);
        }
        let start = self.text.line_to_char(line);
        (start, movement::line_end(&self.text, start))
    }

    pub fn scroll_top(&self) -> usize {
        self.scroll_top
    }

    /// Scroll the least amount that brings the cursor back into a `height`-line viewport.
    ///
    /// Called by the commands that move the cursor, never by the renderer — `render`
    /// stays a pure function of the model (ARCHITECTURE.md §7.1).
    pub fn scroll_to_cursor(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        // Keep a line of context beyond the cursor where the viewport is tall enough.
        let margin = if height > 4 { 1 } else { 0 };
        let (line, _) = self.cursor_position();

        if line < self.scroll_top + margin {
            self.scroll_top = line.saturating_sub(margin);
        } else if line + margin >= self.scroll_top + height {
            self.scroll_top = (line + margin + 1).saturating_sub(height);
        }
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        let cursor = self.cursor();
        (movement::line_of(&self.text, cursor), movement::column_of(&self.text, cursor))
    }

    // --- text entry ---------------------------------------------------------------

    /// Insert text at the cursor, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str, source: EditSource) -> EditResult<()> {
        let range = self.selection.primary();
        self.goal_column = None;
        self.edit(range.start(), range.end(), text, source)
    }

    /// Delete backwards: the selection if there is one, otherwise the preceding char.
    pub fn delete_backward(&mut self) -> EditResult<()> {
        let range = self.selection.primary();
        self.goal_column = None;
        if !range.is_empty() {
            return self.edit(range.start(), range.end(), "", EditSource::Keyboard);
        }
        let cursor = range.head;
        if cursor == 0 {
            return Ok(());
        }
        self.edit(cursor - 1, cursor, "", EditSource::Keyboard)
    }

    /// Delete forwards: the selection if there is one, otherwise the following char.
    pub fn delete_forward(&mut self) -> EditResult<()> {
        let range = self.selection.primary();
        self.goal_column = None;
        if !range.is_empty() {
            return self.edit(range.start(), range.end(), "", EditSource::Keyboard);
        }
        let cursor = range.head;
        if cursor >= self.text.len_chars() {
            return Ok(());
        }
        self.edit(cursor, cursor + 1, "", EditSource::Keyboard)
    }

    /// Reverse the most recent undo group. Returns whether anything happened.
    pub fn undo(&mut self) -> bool {
        match self.history.undo() {
            Some(changes) => {
                self.replay(&changes);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.history.redo() {
            Some(changes) => {
                self.replay(&changes);
                true
            }
            None => false,
        }
    }

    /// Apply a changeset the history handed back.
    ///
    /// Deliberately not routed through [`Self::apply`]: these are already-recorded edits
    /// being replayed, so re-recording them would push undo steps for undoing.
    fn replay(&mut self, changes: &ChangeSet) {
        debug_assert_eq!(changes.len_before(), self.text.len_chars());
        self.text = changes.apply(&self.text);
        self.version = self.version.next();
        self.selection = self.selection.map(changes);
        self.decorations.map(changes);
        self.pending_changes.push(changes.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoration::{Decoration, DecorationClass, HunkSide};
    use crate::selection::Range;
    use crate::HunkState;
    use termesh_core::ProposalId;
    use termesh_test_support::FakeFileSystem;

    fn buffer(text: &str) -> Buffer {
        Buffer::from_text(BufferId::new(1), Some(PathBuf::from("/proj/main.rs")), text)
    }

    #[test]
    fn a_new_buffer_is_empty_untitled_and_clean() {
        let b = Buffer::new(BufferId::new(1));
        assert_eq!(b.text().to_string(), "");
        assert_eq!(b.display_name(), "untitled");
        assert!(b.path().is_none());
        assert!(b.is_dirty(), "an unsaved untitled buffer has nowhere to have been saved to");
    }

    #[test]
    fn editing_bumps_the_version_and_marks_it_dirty() {
        let mut b = buffer("hello");
        assert!(!b.is_dirty());
        let before = b.version();

        b.edit(5, 5, " world", EditSource::Keyboard).unwrap();
        assert_eq!(b.text().to_string(), "hello world");
        assert_eq!(b.version(), before.next());
        assert!(b.is_dirty());
    }

    #[test]
    fn an_applied_transaction_is_queued_for_document_sync() {
        let mut b = Buffer::from_text(BufferId::new(1), None, "fn main() {}");
        b.edit(3, 7, "test", EditSource::Keyboard).unwrap();
        let queued = b.take_pending_changes();
        assert_eq!(queued.len(), 1);
        assert!(b.take_pending_changes().is_empty(), "draining is destructive");
    }

    #[test]
    fn undo_and_redo_are_queued_too() {
        // A hook in `apply` alone desyncs the server on every undo: `replay` mutates the
        // rope and bumps the version without going through `apply` (ADR-0011 §4).
        let mut b = Buffer::from_text(BufferId::new(1), None, "abc");
        b.edit(0, 0, "x", EditSource::Keyboard).unwrap();
        let _ = b.take_pending_changes();

        assert!(b.undo());
        assert_eq!(b.take_pending_changes().len(), 1, "undo must be sent to the server");

        assert!(b.redo());
        assert_eq!(b.take_pending_changes().len(), 1, "redo must be sent to the server");
    }

    #[test]
    fn an_empty_transaction_queues_nothing() {
        let mut b = Buffer::from_text(BufferId::new(1), None, "abc");
        let tx = b.transaction(ChangeSet::identity(3), EditSource::Keyboard);
        b.apply(&tx).unwrap();
        assert!(b.take_pending_changes().is_empty());
    }

    #[test]
    fn a_stale_transaction_is_refused_rather_than_applied() {
        let mut b = buffer("hello");
        // Authored against the current version...
        let stale = b.transaction(
            ChangeSet::replace(5, 0, 5, "goodbye"),
            EditSource::Agent(ProposalId::new(1)),
        );
        // ...but the human types first.
        b.edit(5, 5, "!", EditSource::Keyboard).unwrap();

        let err = b.apply(&stale).unwrap_err();
        assert!(matches!(err, EditError::StaleVersion { .. }), "got {err:?}");
        assert_eq!(b.text().to_string(), "hello!", "the document is untouched");
    }

    #[test]
    fn a_changeset_for_a_different_document_is_refused() {
        let mut b = buffer("hello");
        let wrong = EditTransaction::new(
            b.id(),
            b.version(),
            ChangeSet::replace(99, 0, 1, "x"),
            EditSource::Keyboard,
            Default::default(),
        );
        assert!(matches!(b.apply(&wrong), Err(EditError::LengthMismatch { .. })));
        assert_eq!(b.text().to_string(), "hello");
    }

    #[test]
    fn an_empty_transaction_changes_nothing_and_is_not_an_error() {
        let mut b = buffer("hello");
        let before = b.version();
        b.edit(2, 2, "", EditSource::Keyboard).unwrap();
        assert_eq!(b.version(), before, "a no-op does not advance the revision");
        assert!(!b.can_undo());
    }

    #[test]
    fn the_cursor_rides_along_with_an_edit_before_it() {
        let mut b = buffer("hello world");
        b.set_selection(Selection::point(6));
        b.edit(0, 0, ">> ", EditSource::Paste).unwrap();
        assert_eq!(b.selection().primary(), Range::point(9));
    }

    #[test]
    fn a_transaction_can_pin_the_cursor_explicitly() {
        let mut b = buffer("hello");
        let tx = b
            .transaction(ChangeSet::replace(5, 0, 0, "abc"), EditSource::Keyboard)
            .with_selection(Selection::point(0));
        b.apply(&tx).unwrap();
        assert_eq!(b.selection().primary(), Range::point(0), "the explicit choice wins");
    }

    // --- undo/redo through the buffer --------------------------------------------

    #[test]
    fn undo_and_redo_move_the_document_and_the_version() {
        let mut b = buffer("hello");
        b.edit(5, 5, " world", EditSource::Paste).unwrap();

        assert!(b.undo());
        assert_eq!(b.text().to_string(), "hello");
        assert!(b.redo());
        assert_eq!(b.text().to_string(), "hello world");
        assert!(!b.redo(), "nothing left to redo");
    }

    #[test]
    fn an_agent_edit_undoes_in_one_step_and_stays_traceable() {
        let mut b = buffer("fn main() {}");
        let source = EditSource::Agent(ProposalId::new(7));

        let tx = b.transaction(ChangeSet::replace(12, 3, 7, "run"), source);
        assert_eq!(tx.proposal(), Some(ProposalId::new(7)));
        b.apply(&tx).unwrap();
        assert_eq!(b.text().to_string(), "fn run() {}");

        assert!(b.undo());
        assert_eq!(b.text().to_string(), "fn main() {}");
    }

    // --- disk round trip ----------------------------------------------------------

    #[test]
    fn a_file_loads_edits_and_saves_through_the_service() {
        let fs = FakeFileSystem::with_paths(&["/proj/main.rs"]);
        fs.add_file("/proj/main.rs", b"fn main() {}\n");

        let mut b = Buffer::load(BufferId::new(1), &fs, Path::new("/proj/main.rs")).unwrap();
        assert_eq!(b.text().to_string(), "fn main() {}\n");
        assert_eq!(b.display_name(), "main.rs");
        assert!(!b.is_dirty());

        b.edit(3, 7, "run", EditSource::Keyboard).unwrap();
        assert!(b.is_dirty());

        b.save(&fs).unwrap();
        assert!(!b.is_dirty(), "saving settles the dirty flag");
        assert_eq!(fs.read_file(Path::new("/proj/main.rs")).unwrap(), b"fn run() {}\n");
    }

    #[test]
    fn saving_ends_the_undo_group() {
        let fs = FakeFileSystem::with_paths(&["/proj/main.rs"]);
        fs.add_file("/proj/main.rs", b"()");
        let mut b = Buffer::load(BufferId::new(1), &fs, Path::new("/proj/main.rs")).unwrap();

        b.edit(1, 1, "a", EditSource::Keyboard).unwrap();
        b.save(&fs).unwrap();
        b.edit(2, 2, "b", EditSource::Keyboard).unwrap();

        b.undo();
        assert_eq!(b.text().to_string(), "(a)", "typing after a save is its own step");
    }

    #[test]
    fn crlf_survives_a_round_trip() {
        let fs = FakeFileSystem::with_paths(&["/proj/win.rs"]);
        fs.add_file("/proj/win.rs", b"one\r\ntwo\r\n");

        let mut b = Buffer::load(BufferId::new(1), &fs, Path::new("/proj/win.rs")).unwrap();
        // Internally one char per break, so offsets never have to know about \r.
        assert_eq!(b.text().to_string(), "one\ntwo\n");
        assert_eq!(b.line_ending(), LineEnding::Crlf);

        b.save(&fs).unwrap();
        assert_eq!(
            fs.read_file(Path::new("/proj/win.rs")).unwrap(),
            b"one\r\ntwo\r\n",
            "saving must not rewrite every line of somebody's diff"
        );
    }

    #[test]
    fn lf_files_stay_lf() {
        let mut b = buffer("one\ntwo\n");
        assert_eq!(b.line_ending(), LineEnding::Lf);
        b.edit(0, 0, "x", EditSource::Keyboard).unwrap();
        assert_eq!(b.to_disk_string(), "xone\ntwo\n");
    }

    #[test]
    fn a_non_utf8_file_is_refused_by_name() {
        let fs = FakeFileSystem::with_paths(&["/proj/blob.bin"]);
        fs.add_file("/proj/blob.bin", &[0xff, 0xfe, 0x00]);

        let err = Buffer::load(BufferId::new(1), &fs, Path::new("/proj/blob.bin")).unwrap_err();
        assert!(matches!(err, EditError::NotUtf8(_)), "got {err:?}");
        assert!(err.to_string().contains("blob.bin"), "the message names the file");
    }

    #[test]
    fn a_missing_file_reports_the_filesystem_error() {
        let fs = FakeFileSystem::with_paths(&["/proj/main.rs"]);
        let err = Buffer::load(BufferId::new(1), &fs, Path::new("/proj/nope.rs")).unwrap_err();
        assert!(matches!(err, EditError::Fs(FsError::NotFound(_))), "got {err:?}");
    }

    // --- typing and motion through the buffer -------------------------------------

    #[test]
    fn typing_inserts_at_the_cursor_and_carries_it_along() {
        let mut b = buffer("()");
        b.set_selection(Selection::point(1));
        for ch in ["a", "b", "c"] {
            b.insert(ch, EditSource::Keyboard).unwrap();
        }
        assert_eq!(b.text().to_string(), "(abc)");
        assert_eq!(b.cursor_position(), (0, 4));
    }

    #[test]
    fn a_run_of_typing_is_one_undo_step_but_a_cursor_move_splits_it() {
        let mut b = buffer("()");
        b.set_selection(Selection::point(1));
        b.insert("a", EditSource::Keyboard).unwrap();
        b.insert("b", EditSource::Keyboard).unwrap();
        b.undo();
        assert_eq!(b.text().to_string(), "()", "one run, one undo");

        b.set_selection(Selection::point(1));
        b.insert("x", EditSource::Keyboard).unwrap();
        b.move_right(); // a deliberate move
        b.insert("y", EditSource::Keyboard).unwrap();
        b.undo();
        assert_eq!(b.text().to_string(), "(x)", "the move ended the run");
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut b = buffer("hello world");
        b.set_selection(Selection::single(Range::new(0, 5)));
        b.insert("bye", EditSource::Keyboard).unwrap();
        assert_eq!(b.text().to_string(), "bye world");
    }

    #[test]
    fn backspace_and_delete_take_one_character_each_way() {
        let mut b = buffer("abcd");
        b.set_selection(Selection::point(2));
        b.delete_backward().unwrap();
        assert_eq!(b.text().to_string(), "acd");
        b.delete_forward().unwrap();
        assert_eq!(b.text().to_string(), "ad");
    }

    #[test]
    fn deleting_at_the_edges_of_the_document_does_nothing() {
        let mut b = buffer("ab");
        b.set_selection(Selection::point(0));
        b.delete_backward().unwrap();
        b.set_selection(Selection::point(2));
        b.delete_forward().unwrap();
        assert_eq!(b.text().to_string(), "ab", "no edit, and no panic at the boundaries");
        assert!(!b.can_undo(), "and nothing recorded to undo");
    }

    #[test]
    fn deleting_removes_the_selection_when_there_is_one() {
        let mut b = buffer("hello world");
        b.set_selection(Selection::single(Range::new(5, 11)));
        b.delete_backward().unwrap();
        assert_eq!(b.text().to_string(), "hello");
    }

    #[test]
    fn a_newline_splits_the_line_and_moves_the_cursor_down() {
        let mut b = buffer("ab");
        b.set_selection(Selection::point(1));
        b.insert("\n", EditSource::Keyboard).unwrap();
        assert_eq!(b.text().to_string(), "a\nb");
        assert_eq!(b.cursor_position(), (1, 0));
    }

    #[test]
    fn vertical_motion_keeps_its_column_across_a_short_line() {
        let mut b = buffer("abcdefgh\nxy\nabcdefgh\n");
        b.set_selection(Selection::point(6)); // line 0, column 6

        b.move_line(true);
        assert_eq!(b.cursor_position(), (1, 2), "clamped to the short line");
        b.move_line(true);
        assert_eq!(b.cursor_position(), (2, 6), "and restored below it");
    }

    #[test]
    fn horizontal_motion_forgets_the_sticky_column() {
        let mut b = buffer("abcdefgh\nxy\nabcdefgh\n");
        b.set_selection(Selection::point(6));
        b.move_line(true); // clamped to column 2
        b.move_left(); // a deliberate horizontal move re-aims
        b.move_line(true);
        assert_eq!(b.cursor_position(), (2, 1), "the new column wins");
    }

    #[test]
    fn home_and_end_land_on_the_visible_ends_of_the_line() {
        let mut b = buffer("  indented\nnext\n");
        b.set_selection(Selection::point(5));
        b.move_line_end();
        assert_eq!(b.cursor_position(), (0, 10), "before the newline, not after it");
        b.move_line_start();
        assert_eq!(b.cursor_position(), (0, 0));
    }

    // --- decorations ride edits ---------------------------------------------------

    #[test]
    fn a_hunk_stays_anchored_to_its_code_while_the_human_types_above_it() {
        // The property continuous rebasing exists for: review stays correct mid-typing.
        let mut b = buffer("fn main() {}\n");
        b.decorations_mut().push(Decoration::new(
            3,
            7,
            DecorationClass::Hunk {
                proposal: ProposalId::new(1),
                side: HunkSide::Removed,
                state: HunkState::Clean,
            },
        ));

        b.set_selection(Selection::point(0));
        b.insert("pub ", EditSource::Keyboard).unwrap();

        let d = b.decorations().iter().next().unwrap();
        assert_eq!((d.start, d.end), (7, 11), "still on `main`, four chars further along");
        assert_eq!(b.text().to_string(), "pub fn main() {}\n");
    }

    #[test]
    fn editing_inside_a_hunk_conflicts_it_rather_than_dropping_it() {
        let mut b = buffer("fn main() {}\n");
        b.decorations_mut().push(Decoration::new(
            3,
            7,
            DecorationClass::Hunk {
                proposal: ProposalId::new(1),
                side: HunkSide::Removed,
                state: HunkState::Clean,
            },
        ));

        b.set_selection(Selection::point(5));
        b.insert("X", EditSource::Keyboard).unwrap();

        let d = b.decorations().iter().next().unwrap();
        assert!(
            matches!(d.class, DecorationClass::Hunk { state: HunkState::Conflicted(_), .. }),
            "the human must be told, not silently overruled"
        );
    }

    #[test]
    fn line_ranges_cover_the_visible_text_of_each_line() {
        let b = buffer("abc\ndefgh\n");
        assert_eq!(b.line_range(0), (0, 3), "excludes the newline");
        assert_eq!(b.line_range(1), (4, 9));
    }

    #[test]
    fn a_line_past_the_end_reports_an_empty_range_at_the_end() {
        let b = buffer("abc");
        let end = b.text().len_chars();
        assert_eq!(b.line_range(99), (end, end));
    }

    // --- async save ---------------------------------------------------------------

    #[test]
    fn marking_saved_settles_the_dirty_flag() {
        let mut b = buffer("hello");
        b.edit(5, 5, "!", EditSource::Keyboard).unwrap();
        let version = b.version();
        assert!(b.is_dirty());

        b.mark_saved(version);
        assert!(!b.is_dirty());
    }

    /// The race the async save path exists to survive: the write is on a worker thread,
    /// so the user can type before it lands.
    #[test]
    fn typing_while_a_save_is_in_flight_leaves_the_buffer_dirty() {
        let mut b = buffer("hello");
        b.edit(5, 5, "!", EditSource::Keyboard).unwrap();
        let in_flight = b.version();

        b.edit(6, 6, "?", EditSource::Keyboard).unwrap(); // typed before the write returned
        b.mark_saved(in_flight);

        assert!(b.is_dirty(), "what reached disk is not what is in the buffer");
    }

    #[test]
    fn multibyte_content_edits_by_char_offset() {
        let mut b = buffer("héllo wörld");
        b.edit(6, 11, "there", EditSource::Keyboard).unwrap();
        assert_eq!(b.text().to_string(), "héllo there");
    }
}
