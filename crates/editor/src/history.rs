//! Undo/redo over the transaction log (ADR-0006 §6).
//!
//! One history for every author. A keyboard edit, a formatter run, and an accepted agent
//! proposal all land in the same log, so undo means the same thing regardless of who made
//! the change — which is the property that makes agent edits reviewable rather than
//! frightening.
//!
//! The history holds no reference to a document. It hands back a [`ChangeSet`] and the
//! caller applies it, which keeps undo testable with no rope and no buffer.

use crate::change::ChangeSet;
use crate::transaction::{EditSource, EditTransaction, UndoGroupId};

#[derive(Debug, Clone)]
struct Entry {
    group: UndoGroupId,
    forward: ChangeSet,
    /// Computed at *apply* time, while the pre-image was still live (ADR-0006 §6).
    inverse: ChangeSet,
}

/// The linear undo log.
///
/// ARCHITECTURE.md §8 wants one undo path for all sources; a linear stack delivers that.
/// Helix's undo *tree* is strictly more powerful, and is additive on top of this log if
/// V1 ever needs it — the log is the hard part and we are building it either way.
#[derive(Debug, Default)]
pub struct History {
    applied: Vec<Entry>,
    /// Undone entries, newest last. Cleared as soon as a fresh edit arrives, because
    /// redoing onto a diverged document is not something we can honour.
    undone: Vec<Entry>,
    next_group: u64,
    /// The source of the last recorded edit, for coalescing decisions.
    last_source: Option<EditSource>,
    /// Set when something (a cursor move, a save) should end the current run of typing.
    group_broken: bool,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// The undo group a new edit from `source` belongs to.
    ///
    /// Consecutive typing merges; anything else starts a fresh group, as does an explicit
    /// [`break_group`](Self::break_group) from a cursor move or a save.
    pub fn group_for(&mut self, source: &EditSource) -> UndoGroupId {
        let continues = !self.group_broken
            && source.coalesces()
            && self.last_source.as_ref() == Some(source)
            && !self.applied.is_empty();

        if continues {
            return self.applied[self.applied.len() - 1].group;
        }

        self.next_group += 1;
        UndoGroupId(self.next_group)
    }

    /// End the current undo group, so the next edit starts a new one.
    ///
    /// Called on a cursor move, a save, or an idle timeout — the boundaries a user
    /// intuitively expects undo to stop at.
    pub fn break_group(&mut self) {
        self.group_broken = true;
    }

    /// Record a transaction that has just been applied, with the inverse computed against
    /// the document as it was *before* the change.
    pub fn push(&mut self, transaction: &EditTransaction, inverse: ChangeSet) {
        if transaction.is_empty() {
            return;
        }
        // A new edit invalidates the redo stack: those changesets were authored against a
        // document that no longer exists.
        self.undone.clear();
        // Cleared here rather than in `group_for`, so an edit that turns out to be empty
        // cannot consume a pending break: ask for a group, record nothing, and the next
        // real keystroke must still start fresh.
        self.group_broken = false;
        self.last_source = Some(transaction.source.clone());
        self.applied.push(Entry {
            group: transaction.undo_group,
            forward: transaction.changes.clone(),
            inverse,
        });
    }

    pub fn can_undo(&self) -> bool {
        !self.applied.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// The change that undoes the most recent group, or `None` at the start of history.
    ///
    /// The whole group comes back as one changeset, so a run of typing — or an accepted
    /// multi-hunk proposal — is one keystroke to reverse.
    pub fn undo(&mut self) -> Option<ChangeSet> {
        let group = self.applied.last()?.group;

        let mut composed: Option<ChangeSet> = None;
        while self.applied.last().is_some_and(|e| e.group == group) {
            let entry = self.applied.pop().expect("just checked");
            // Newest first: the last edit applied is the first one undone.
            composed = Some(match composed {
                None => entry.inverse.clone(),
                Some(acc) => acc.compose(&entry.inverse),
            });
            self.undone.push(entry);
        }

        // Typing after an undo must not silently rejoin the group we just reversed.
        self.group_broken = true;
        self.last_source = None;
        composed
    }

    /// The change that reapplies the most recently undone group.
    pub fn redo(&mut self) -> Option<ChangeSet> {
        let group = self.undone.last()?.group;

        let mut composed: Option<ChangeSet> = None;
        while self.undone.last().is_some_and(|e| e.group == group) {
            let entry = self.undone.pop().expect("just checked");
            // `undone` was pushed newest-first, so popping replays in original order.
            composed = Some(match composed {
                None => entry.forward.clone(),
                Some(acc) => acc.compose(&entry.forward),
            });
            self.applied.push(entry);
        }

        self.group_broken = true;
        self.last_source = None;
        composed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::Version;
    use ropey::Rope;
    use termesh_core::{BufferId, ProposalId};

    /// A tiny document that records edits into a history, so tests read like usage.
    struct Doc {
        text: Rope,
        history: History,
        version: Version,
    }

    impl Doc {
        fn new(text: &str) -> Self {
            Self { text: Rope::from_str(text), history: History::new(), version: Version(0) }
        }

        fn edit(&mut self, from: usize, to: usize, insert: &str, source: EditSource) {
            let changes = ChangeSet::replace(self.text.len_chars(), from, to, insert);
            let group = self.history.group_for(&source);
            let tx = EditTransaction::new(BufferId::new(1), self.version, changes, source, group);
            // Inverse computed here, against the live pre-image — the ADR-0006 §6 rule.
            let inverse = tx.changes.invert(&self.text);
            self.text = tx.changes.apply(&self.text);
            self.version = self.version.next();
            self.history.push(&tx, inverse);
        }

        fn type_char(&mut self, at: usize, ch: &str) {
            self.edit(at, at, ch, EditSource::Keyboard);
        }

        fn undo(&mut self) -> bool {
            match self.history.undo() {
                Some(cs) => {
                    self.text = cs.apply(&self.text);
                    self.version = self.version.next();
                    true
                }
                None => false,
            }
        }

        fn redo(&mut self) -> bool {
            match self.history.redo() {
                Some(cs) => {
                    self.text = cs.apply(&self.text);
                    self.version = self.version.next();
                    true
                }
                None => false,
            }
        }

        fn text(&self) -> String {
            self.text.to_string()
        }
    }

    #[test]
    fn nothing_to_undo_at_the_start_of_history() {
        let mut doc = Doc::new("hello");
        assert!(!doc.history.can_undo());
        assert!(!doc.undo());
    }

    #[test]
    fn a_single_edit_undoes_and_redoes() {
        let mut doc = Doc::new("hello");
        doc.edit(5, 5, " world", EditSource::Paste);
        assert_eq!(doc.text(), "hello world");

        assert!(doc.undo());
        assert_eq!(doc.text(), "hello");
        assert!(doc.redo());
        assert_eq!(doc.text(), "hello world");
    }

    #[test]
    fn a_run_of_typing_undoes_as_one_step() {
        let mut doc = Doc::new("()");
        for (i, ch) in "abc".chars().enumerate() {
            doc.type_char(1 + i, &ch.to_string());
        }
        assert_eq!(doc.text(), "(abc)");

        assert!(doc.undo());
        assert_eq!(doc.text(), "()", "three keystrokes, one undo");
        assert!(!doc.history.can_undo());
    }

    #[test]
    fn a_cursor_move_ends_the_run() {
        let mut doc = Doc::new("()");
        doc.type_char(1, "a");
        doc.history.break_group(); // as a cursor move would
        doc.type_char(2, "b");
        assert_eq!(doc.text(), "(ab)");

        doc.undo();
        assert_eq!(doc.text(), "(a)", "the break split the run in two");
        doc.undo();
        assert_eq!(doc.text(), "()");
    }

    #[test]
    fn a_different_source_ends_the_run_without_being_asked() {
        let mut doc = Doc::new("()");
        doc.type_char(1, "a");
        doc.edit(2, 2, "!", EditSource::Formatter);
        doc.undo();
        assert_eq!(doc.text(), "(a)", "the formatter edit is its own step");
    }

    /// The phase's exit criterion in miniature: an agent's change, however many edits it
    /// took, is one thing to undo.
    #[test]
    fn an_accepted_proposal_undoes_in_one_step() {
        let mut doc = Doc::new("fn main() {}");
        let source = EditSource::Agent(ProposalId::new(1));

        // Two hunks of one proposal share an undo group.
        let group = doc.history.group_for(&source);
        for (from, to, insert) in [(3, 7, "run"), (0, 0, "pub ")] {
            let changes = ChangeSet::replace(doc.text.len_chars(), from, to, insert);
            let tx =
                EditTransaction::new(BufferId::new(1), doc.version, changes, source.clone(), group);
            let inverse = tx.changes.invert(&doc.text);
            doc.text = tx.changes.apply(&doc.text);
            doc.history.push(&tx, inverse);
        }
        assert_eq!(doc.text(), "pub fn run() {}");

        assert!(doc.undo());
        assert_eq!(doc.text(), "fn main() {}", "one undo reverses the whole proposal");
        assert!(!doc.history.can_undo());
    }

    #[test]
    fn undo_then_type_discards_the_redo_stack() {
        let mut doc = Doc::new("a");
        doc.edit(1, 1, "b", EditSource::Paste);
        doc.undo();
        assert!(doc.history.can_redo());

        doc.edit(1, 1, "c", EditSource::Paste);
        assert!(!doc.history.can_redo(), "redoing onto a diverged document is not offered");
        assert_eq!(doc.text(), "ac");
    }

    #[test]
    fn typing_after_an_undo_starts_a_fresh_group() {
        let mut doc = Doc::new("()");
        doc.type_char(1, "a");
        doc.type_char(2, "b");
        doc.undo();
        assert_eq!(doc.text(), "()");

        doc.type_char(1, "z");
        doc.undo();
        assert_eq!(doc.text(), "()", "the new keystroke must not rejoin the reversed group");
    }

    #[test]
    fn many_edits_round_trip_all_the_way_back() {
        let mut doc = Doc::new("start");
        let original = doc.text();
        for (from, to, insert, source) in [
            (5, 5, " middle", EditSource::Paste),
            (0, 5, "BEGIN", EditSource::Replace),
            (5, 12, "", EditSource::Formatter),
        ] {
            doc.edit(from, to, insert, source);
        }
        assert_ne!(doc.text(), original);

        while doc.undo() {}
        assert_eq!(doc.text(), original, "history unwinds completely");

        while doc.redo() {}
        assert_eq!(doc.text(), "BEGIN");
    }

    #[test]
    fn empty_transactions_are_not_recorded() {
        let mut doc = Doc::new("hello");
        doc.edit(2, 2, "", EditSource::Keyboard);
        assert!(!doc.history.can_undo(), "a no-op edit is not an undo step");
    }

    #[test]
    fn an_empty_edit_cannot_swallow_a_pending_group_break() {
        let mut doc = Doc::new("()");
        doc.type_char(1, "a");
        doc.history.break_group(); // a cursor move

        doc.edit(2, 2, "", EditSource::Keyboard); // no-op: asks for a group, records nothing
        doc.type_char(2, "b");

        doc.undo();
        assert_eq!(doc.text(), "(a)", "the break must survive an edit that did nothing");
    }
}
