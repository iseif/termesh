//! [`EditTransaction`] — the one and only way a buffer changes (ARCHITECTURE.md §8).
//!
//! Every mutation, whoever authored it, is stamped with the buffer revision it was
//! written against. That stamp is what makes agent diff-review safe: a proposal authored
//! at version `N` can be carried forward through whatever the human typed since, instead
//! of being written blind over their work.

use termesh_core::{BufferId, ProposalId};

use crate::change::ChangeSet;
use crate::selection::Selection;

/// Monotonic buffer revision. Bumps on every applied transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Version(pub u64);

impl Version {
    pub fn next(self) -> Self {
        Version(self.0 + 1)
    }
}

/// Groups transactions that undo together (ADR-0006 §6).
///
/// The unit the user thinks in: a run of typing is one group, and an accepted agent
/// proposal is one group however many hunks it touched — so "accept, undo" undoes *the
/// agent's change*, not one insertion of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct UndoGroupId(pub u64);

/// Where an edit came from. `Agent` carries the [`ProposalId`] so accepted agent edits
/// stay traceable through undo history and review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditSource {
    Keyboard,
    Paste,
    Formatter,
    Lsp,
    Agent(ProposalId),
    Replace,
}

impl EditSource {
    /// Whether consecutive edits from this source may merge into one undo step.
    ///
    /// Only free-running typing does. A paste, a format, an LSP fix, and an agent
    /// proposal are each a discrete act the user should be able to undo on its own.
    pub fn coalesces(&self) -> bool {
        matches!(self, EditSource::Keyboard)
    }
}

/// A change to one buffer, stamped with the revision it was authored against.
#[derive(Debug, Clone)]
pub struct EditTransaction {
    pub buffer: BufferId,
    pub base_version: Version,
    pub changes: ChangeSet,
    pub source: EditSource,
    pub undo_group: UndoGroupId,
    /// Where the cursor should end up. `None` means "derive it" — map the current
    /// selection through `changes`, which is right for edits made somewhere else in the
    /// document (an agent hunk, a formatter run) where the cursor should simply hold its
    /// place.
    ///
    /// ARCHITECTURE.md §8 calls this field a `SelectionMap`; mapping *is* the default
    /// behaviour here, and the `Some` case exists because an author sometimes knows
    /// better — after typing, the cursor belongs after the inserted text, which is not
    /// something position mapping can infer.
    pub selection: Option<Selection>,
}

impl EditTransaction {
    /// A transaction from `source` against `base_version`, deriving the resulting
    /// selection by mapping.
    pub fn new(
        buffer: BufferId,
        base_version: Version,
        changes: ChangeSet,
        source: EditSource,
        undo_group: UndoGroupId,
    ) -> Self {
        Self { buffer, base_version, changes, source, undo_group, selection: None }
    }

    /// Pin the post-edit selection explicitly.
    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = Some(selection);
        self
    }

    /// Whether this transaction changes nothing.
    pub fn is_empty(&self) -> bool {
        self.changes.is_identity()
    }

    /// The proposal this edit came from, if any.
    pub fn proposal(&self) -> Option<ProposalId> {
        match self.source {
            EditSource::Agent(id) => Some(id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(source: EditSource) -> EditTransaction {
        EditTransaction::new(
            BufferId::new(1),
            Version(3),
            ChangeSet::replace(10, 0, 0, "x"),
            source,
            UndoGroupId(7),
        )
    }

    #[test]
    fn agent_edits_are_traceable_through_source() {
        let t = tx(EditSource::Agent(ProposalId::new(42)));
        assert_eq!(t.proposal(), Some(ProposalId::new(42)));
        assert_eq!(t.base_version, Version(3));
    }

    #[test]
    fn non_agent_edits_carry_no_proposal() {
        assert_eq!(tx(EditSource::Keyboard).proposal(), None);
    }

    #[test]
    fn only_typing_coalesces_into_one_undo_step() {
        assert!(EditSource::Keyboard.coalesces());
        for source in [
            EditSource::Paste,
            EditSource::Formatter,
            EditSource::Lsp,
            EditSource::Replace,
            EditSource::Agent(ProposalId::new(1)),
        ] {
            assert!(!source.coalesces(), "{source:?} should be its own undo step");
        }
    }

    #[test]
    fn selection_defaults_to_derived_and_can_be_pinned() {
        let t = tx(EditSource::Keyboard);
        assert!(t.selection.is_none(), "derived by mapping unless told otherwise");

        let pinned = tx(EditSource::Keyboard).with_selection(Selection::point(4));
        assert_eq!(pinned.selection.unwrap().primary().head, 4);
    }

    #[test]
    fn an_identity_change_is_an_empty_transaction() {
        let t = EditTransaction::new(
            BufferId::new(1),
            Version(0),
            ChangeSet::identity(5),
            EditSource::Keyboard,
            UndoGroupId(0),
        );
        assert!(t.is_empty());
    }

    #[test]
    fn versions_advance_monotonically() {
        assert_eq!(Version(4).next(), Version(5));
        assert!(Version(4) < Version(5));
    }
}
