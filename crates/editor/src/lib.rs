//! The editor core and the **shared-state transaction spine** (ARCHITECTURE.md §8).
//!
//! Both the human and the agent change buffers, so there is exactly one edit path: every
//! change is an [`EditTransaction`] stamped with the buffer version it was authored
//! against. That yields one undo history, clean LSP/syntax sync, and — the reason it
//! matters — *safe agent diff-review*: an agent proposal is a [`ChangeSet`] against
//! `base_version`; on accept we apply directly if the buffer is still there, or carry it
//! forward through the intervening edits. Modeled on Helix / CodeMirror 6.
//!
//! The design decisions behind this module — including what "carries forward cleanly"
//! actually means, case by case — are in **ADR-0006**.
//!
//! ```
//! use termesh_editor::{Assoc, ChangeSet};
//! use ropey::Rope;
//!
//! let original = Rope::from_str("fn main() {}");
//! // An agent proposes renaming `main`, anchored at char 3.
//! let proposal = ChangeSet::replace(original.len_chars(), 3, 7, "run");
//!
//! // Meanwhile the human types at the start of the line.
//! let human = ChangeSet::replace(original.len_chars(), 0, 0, "pub ");
//! let current = human.apply(&original);
//!
//! // The proposal's anchor rides forward over the human's edit instead of going stale.
//! assert_eq!(human.map_pos(3, Assoc::After), 7);
//! assert_eq!(current.to_string(), "pub fn main() {}");
//! # let _ = proposal;
//! ```
#![forbid(unsafe_code)]

pub mod buffer;
pub mod change;
pub mod decoration;
pub mod history;
pub mod movement;
pub mod position;
pub mod search;
pub mod selection;
pub mod transaction;

pub use buffer::{Buffer, EditError, EditResult, LineEnding};
pub use change::{Assoc, ChangeSet, ChangeSetBuilder, ChangedSpan, Operation, RangeEffect};
pub use decoration::{
    Decoration, DecorationClass, DecorationSet, HunkSide, LineDecoration, Severity, SyntaxKind,
};
pub use history::History;
pub use search::{find_all, CaseMode, Match};
pub use selection::{Range, Selection};
pub use transaction::{EditSource, EditTransaction, UndoGroupId, Version};

/// Why a proposal hunk could not be carried forward onto the current buffer.
///
/// Each variant names a case from ADR-0006 §4 so the review UI can say *what happened*
/// ("you edited inside this change") rather than reporting a generic failure. That
/// distinction is most of the difference between a reviewable tool and a mysterious one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// ADR-0006 §4 case 2 — the human typed inside text this hunk wanted to replace.
    /// Applying would destroy their edit without ever showing it to them.
    EditedInsideRange,
    /// ADR-0006 §4 case 4 — the text this hunk was anchored to is partly gone, so where
    /// it belongs is guesswork.
    AnchorDeleted,
}

impl ConflictReason {
    /// The conflict implied by what an applied change did to a hunk's range, or `None`
    /// if the hunk can still be carried forward.
    ///
    /// This is ADR-0006 §4's table as code: [`ChangeSet::touches`] reports what happened,
    /// and this decides what it means for review. Case 5 (the human already made the same
    /// change) is deliberately *not* here — it is a content check that runs before this
    /// one, because a satisfied hunk looks exactly like a deleted anchor from here.
    pub fn from_effect(effect: RangeEffect) -> Option<Self> {
        match effect {
            RangeEffect::Untouched => None,
            RangeEffect::InsertedInside => Some(ConflictReason::EditedInsideRange),
            RangeEffect::PartlyDeleted => Some(ConflictReason::AnchorDeleted),
        }
    }
}

impl core::fmt::Display for ConflictReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConflictReason::EditedInsideRange => f.write_str("you edited inside this change"),
            ConflictReason::AnchorDeleted => {
                f.write_str("the code this change referred to is gone")
            }
        }
    }
}

/// Whether a proposal hunk can still be applied (ADR-0006 §4, §5).
///
/// State lives on the *hunk*, never the proposal: a conflict in one hunk must not
/// invalidate its siblings, because ARCHITECTURE.md §9.3 requires per-hunk review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkState {
    /// Applies as-is.
    Clean,
    /// Cannot be applied; the human resolves it or re-asks the agent.
    Conflicted(ConflictReason),
    /// The human already made this change themselves, so there is nothing left to do.
    Satisfied,
}

impl HunkState {
    pub fn is_applicable(&self) -> bool {
        matches!(self, HunkState::Clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_clean_hunks_apply() {
        assert!(HunkState::Clean.is_applicable());
        assert!(!HunkState::Conflicted(ConflictReason::AnchorDeleted).is_applicable());
        assert!(!HunkState::Satisfied.is_applicable(), "already done is not applied again");
    }

    #[test]
    fn a_surviving_range_is_not_a_conflict() {
        assert_eq!(ConflictReason::from_effect(RangeEffect::Untouched), None);
    }

    #[test]
    fn each_overlap_becomes_the_conflict_it_implies() {
        assert_eq!(
            ConflictReason::from_effect(RangeEffect::InsertedInside),
            Some(ConflictReason::EditedInsideRange)
        );
        assert_eq!(
            ConflictReason::from_effect(RangeEffect::PartlyDeleted),
            Some(ConflictReason::AnchorDeleted)
        );
    }

    /// The path a hunk actually takes: a human edit, what it did to the hunk's range,
    /// and the verdict the reviewer sees.
    #[test]
    fn a_human_edit_inside_a_hunk_makes_it_unapplicable() {
        let hunk = (10, 20);
        let typed_inside = ChangeSet::replace(40, 15, 15, "mine");

        let state = match ConflictReason::from_effect(typed_inside.touches(hunk.0, hunk.1)) {
            Some(reason) => HunkState::Conflicted(reason),
            None => HunkState::Clean,
        };

        assert_eq!(state, HunkState::Conflicted(ConflictReason::EditedInsideRange));
        assert!(!state.is_applicable(), "never silently destroy what the human wrote");
    }

    #[test]
    fn every_conflict_reason_explains_itself_to_the_user() {
        for reason in [ConflictReason::EditedInsideRange, ConflictReason::AnchorDeleted] {
            let msg = reason.to_string();
            assert!(!msg.is_empty());
            assert!(!msg.contains("Conflict"), "should read as prose, got {msg:?}");
        }
    }
}
