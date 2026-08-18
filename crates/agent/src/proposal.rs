//! Turning an agent's diff into reviewable hunks (ADR-0007 §5).
//!
//! ACP does not hand us range edits. It hands us **whole-file before-and-after text**
//! (`ToolCallContent::Diff { old_text, new_text }`), so the range edits ARCHITECTURE.md
//! §9.3 originally assumed have to be *derived*. That derivation is here.
//!
//! The other half of the problem is anchoring. `old_text` is the file as the agent saw
//! it, which is not necessarily any revision of ours — the human has probably typed
//! since. Rather than reaching into undo history for the intervening transactions, we
//! synthesise them: diffing the agent's base against the current buffer produces exactly
//! the `ChangeSet` describing "what changed underneath this proposal", and ADR-0006's
//! machinery then carries the hunks across it or marks them conflicted. One diff routine
//! serves both jobs.

use std::path::PathBuf;

use similar::{DiffTag, TextDiff};
use termesh_core::ProposalId;
use termesh_editor::{ChangeSet, ConflictReason, HunkState, Version};

/// One contiguous change the human accepts or rejects on its own.
///
/// `start..end` is a char range in the document the proposal was authored against;
/// `text` replaces it. A pure insertion has `start == end`, a pure deletion has empty
/// `text` — the two cases ARCHITECTURE.md §9.3's "file/range edits" glossed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub state: HunkState,
}

impl Hunk {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { start, end, text: text.into(), state: HunkState::Clean }
    }

    pub fn is_insertion(&self) -> bool {
        self.start == self.end
    }

    pub fn is_deletion(&self) -> bool {
        self.text.is_empty()
    }
}

/// A set of edits to one file, awaiting review.
///
/// **This is the authoritative record of a pending review.** Hunk decorations in the
/// buffer are a *projection* of it, rebuilt by [`Self::refresh`] rather than maintained
/// in parallel — two mechanisms tracking the same state is how you end up with a proposal
/// that reads clean in one place and conflicted in the other.
#[derive(Debug, Clone)]
pub struct EditProposal {
    pub id: ProposalId,
    pub path: PathBuf,
    /// The buffer revision this was anchored to, when we could establish one (§5).
    /// `None` means the agent read the file by some other means and we anchored by
    /// content instead.
    pub base_version: Option<Version>,
    /// The file as the agent saw it. Immutable — ADR-0006 §3 keeps the original so the
    /// agent can always be re-asked with the exact context it authored against, and so
    /// [`Self::refresh`] can recompute from a fixed point rather than accumulating drift.
    pub base_text: String,
    /// The file as the agent proposed it. Immutable, same reasons.
    pub proposed_text: String,
    pub hunks: Vec<Hunk>,
}

impl EditProposal {
    /// Build a proposal from a whole-file diff, anchored onto `current_text`.
    pub fn new(
        id: ProposalId,
        path: PathBuf,
        base_version: Option<Version>,
        base_text: String,
        proposed_text: String,
        current_text: &str,
    ) -> Self {
        let mut proposal =
            Self { id, path, base_version, base_text, proposed_text, hunks: Vec::new() };
        proposal.refresh(current_text);
        proposal
    }

    /// Recompute the hunks against the buffer as it stands now.
    ///
    /// Idempotent, and derived from the immutable original every time rather than from
    /// the last result, so repeated calls cannot accumulate error. Cheap enough to run on
    /// every edit for realistic proposals; ADR-0006 §3 notes it can be made lazy if that
    /// ever stops being true.
    pub fn refresh(&mut self, current_text: &str) {
        self.hunks = hunks_from_diff(&self.base_text, &self.proposed_text);
        rebase_hunks(&mut self.hunks, &self.base_text, current_text);
    }
    /// Hunks that would actually apply if accepted right now.
    pub fn applicable(&self) -> impl Iterator<Item = &Hunk> {
        self.hunks.iter().filter(|h| h.state.is_applicable())
    }

    pub fn has_conflicts(&self) -> bool {
        self.hunks.iter().any(|h| matches!(h.state, HunkState::Conflicted(_)))
    }

    /// Whether there is nothing left to review — every hunk applied or resolved itself.
    pub fn is_settled(&self) -> bool {
        self.hunks.iter().all(|h| h.state == HunkState::Satisfied)
    }
}

/// Char offset of the start of each line, plus a sentinel for the end of the text.
///
/// `split_inclusive('\n')` matches `similar`'s line tokenizer for `\n`-only text, which
/// is what buffers hold — `Buffer::from_text` normalises CRLF on the way in, so the two
/// cannot disagree here.
fn line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut acc = 0;
    for line in text.split_inclusive('\n') {
        offsets.push(acc);
        acc += line.chars().count();
    }
    offsets.push(acc);
    offsets
}

/// Derive review hunks from a whole-file before/after pair.
///
/// Line granularity, because that is the unit a human reviews in — and it is what makes
/// a hunk correspond to something you can point at on screen.
pub fn hunks_from_diff(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let offsets = line_offsets(old);
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    diff.ops()
        .iter()
        .filter_map(|op| {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                return None;
            }
            Some(Hunk::new(
                offsets[old_range.start],
                offsets[old_range.end],
                new_lines[new_range].concat(),
            ))
        })
        .collect()
}

/// The single change that applies `hunks` to a document of `len_before` chars.
///
/// Hunks must be non-overlapping and are applied in document order, so accepting several
/// at once is one transaction — and therefore one undo step (ADR-0006 §6).
pub fn changeset_from_hunks(hunks: &[&Hunk], len_before: usize) -> ChangeSet {
    let mut ordered: Vec<&&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|h| (h.start, h.end));

    let mut builder = ChangeSet::builder(len_before);
    let mut at = 0;
    for hunk in ordered {
        debug_assert!(hunk.start >= at, "hunks overlap: {at} > {}", hunk.start);
        builder.retain(hunk.start.saturating_sub(at));
        builder.delete(hunk.end - hunk.start);
        builder.insert(hunk.text.clone());
        at = hunk.end;
    }
    builder.build()
}

/// Carry hunks authored against `base_text` onto `current_text`.
///
/// The agent's base is not necessarily a revision we hold, so rather than replaying undo
/// history we *synthesise* the intervening change by diffing base against current. The
/// result is an ordinary [`ChangeSet`], which means ADR-0006 §4's overlap rules apply
/// unchanged — including that a hunk the human edited inside is conflicted rather than
/// silently applied over their work.
pub fn rebase_hunks(hunks: &mut [Hunk], base_text: &str, current_text: &str) {
    if base_text == current_text {
        return; // nothing moved
    }

    // What the human did, in hunk form. Both these and the agent's hunks are offsets
    // into `base_text`, which is what makes case 5 below an exact comparison.
    let human = hunks_from_diff(base_text, current_text);
    let catchup =
        changeset_from_hunks(&human.iter().collect::<Vec<_>>(), base_text.chars().count());

    for hunk in hunks.iter_mut() {
        // ADR-0006 §4 case 5, checked *before* conflict classification: making the
        // identical change destroys the anchor, so from position mapping alone it is
        // indistinguishable from a deletion.
        //
        // Comparing whole hunks rather than scanning the document for the replacement
        // text matters. A hunk inserting something as common as "}\n" would match a
        // window almost anywhere, and `Satisfied` means the change disappears with no
        // conflict marker and nothing on screen — the exact inversion of "never silently
        // destroy". An identical hunk is proof; a matching window is a coincidence.
        if human.iter().any(|h| h.start == hunk.start && h.end == hunk.end && h.text == hunk.text) {
            hunk.state = HunkState::Satisfied;
            continue;
        }

        match ConflictReason::from_effect(catchup.touches(hunk.start, hunk.end)) {
            Some(reason) => hunk.state = HunkState::Conflicted(reason),
            None => {
                hunk.start = catchup.map_pos(hunk.start, termesh_editor::Assoc::After);
                hunk.end = catchup.map_pos(hunk.end, termesh_editor::Assoc::After);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(hunks: &[Hunk]) -> Vec<(usize, usize, &str)> {
        hunks.iter().map(|h| (h.start, h.end, h.text.as_str())).collect()
    }

    /// Apply every clean hunk and read the document back.
    fn apply(base: &str, hunks: &[Hunk]) -> String {
        let clean: Vec<&Hunk> = hunks.iter().filter(|h| h.state.is_applicable()).collect();
        let cs = changeset_from_hunks(&clean, base.chars().count());
        cs.apply(&ropey::Rope::from_str(base)).to_string()
    }

    // --- deriving hunks from a whole-file diff -------------------------------------

    #[test]
    fn an_unchanged_file_yields_no_hunks() {
        assert!(hunks_from_diff("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn a_changed_line_becomes_one_hunk_over_its_char_range() {
        let old = "one\ntwo\nthree\n";
        let hunks = hunks_from_diff(old, "one\nTWO\nthree\n");
        assert_eq!(texts(&hunks), [(4, 8, "TWO\n")]);
        assert_eq!(apply(old, &hunks), "one\nTWO\nthree\n");
    }

    #[test]
    fn an_inserted_line_is_a_zero_width_hunk() {
        let old = "one\nthree\n";
        let hunks = hunks_from_diff(old, "one\ntwo\nthree\n");
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].is_insertion(), "nothing is replaced, so the range is empty");
        assert_eq!(apply(old, &hunks), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_deleted_line_is_a_hunk_with_no_replacement() {
        let old = "one\ntwo\nthree\n";
        let hunks = hunks_from_diff(old, "one\nthree\n");
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].is_deletion());
        assert_eq!(apply(old, &hunks), "one\nthree\n");
    }

    #[test]
    fn separate_edits_become_separate_hunks() {
        // The reason review is per-hunk: two unrelated changes, two decisions.
        let old = "one\ntwo\nthree\nfour\n";
        let hunks = hunks_from_diff(old, "ONE\ntwo\nthree\nFOUR\n");
        assert_eq!(hunks.len(), 2);
        assert_eq!(apply(old, &hunks), "ONE\ntwo\nthree\nFOUR\n");
    }

    #[test]
    fn accepting_only_one_hunk_leaves_the_other_alone() {
        let old = "one\ntwo\nthree\nfour\n";
        let hunks = hunks_from_diff(old, "ONE\ntwo\nthree\nFOUR\n");

        let cs = changeset_from_hunks(&[&hunks[0]], old.chars().count());
        assert_eq!(cs.apply(&ropey::Rope::from_str(old)).to_string(), "ONE\ntwo\nthree\nfour\n");
    }

    #[test]
    fn creating_a_file_from_nothing_is_one_insertion() {
        let hunks = hunks_from_diff("", "hello\n");
        assert_eq!(texts(&hunks), [(0, 0, "hello\n")]);
        assert_eq!(apply("", &hunks), "hello\n");
    }

    #[test]
    fn a_file_without_a_trailing_newline_round_trips() {
        let old = "one\ntwo";
        let hunks = hunks_from_diff(old, "one\nTWO");
        assert_eq!(apply(old, &hunks), "one\nTWO");
    }

    #[test]
    fn multibyte_lines_produce_char_offsets_not_byte_offsets() {
        let old = "héllo\nwörld\n";
        let hunks = hunks_from_diff(old, "héllo\nWORLD\n");
        assert_eq!(hunks[0].start, 6, "6 chars, not 7 bytes");
        assert_eq!(apply(old, &hunks), "héllo\nWORLD\n");
    }

    // --- rebasing onto a buffer the human has been typing in ------------------------

    #[test]
    fn an_untouched_proposal_needs_no_rebasing() {
        let base = "one\ntwo\n";
        let mut hunks = hunks_from_diff(base, "one\nTWO\n");
        let before = hunks.clone();
        rebase_hunks(&mut hunks, base, base);
        assert_eq!(hunks, before);
    }

    /// The headline case: the human types elsewhere while the agent is thinking, and the
    /// proposal still lands on the right code.
    #[test]
    fn a_hunk_rides_over_an_edit_made_above_it() {
        let base = "one\ntwo\nthree\n";
        let current = "zero\none\ntwo\nthree\n"; // human added a line at the top
        let mut hunks = hunks_from_diff(base, "one\ntwo\nTHREE\n");

        rebase_hunks(&mut hunks, base, current);

        assert_eq!(hunks[0].state, HunkState::Clean);
        assert_eq!(apply(current, &hunks), "zero\none\ntwo\nTHREE\n");
    }

    /// ADR-0006 §4 case 2, end to end: never silently destroy what the human wrote.
    #[test]
    fn a_hunk_the_human_edited_inside_conflicts() {
        let base = "one\ntwo\nthree\n";
        let current = "one\ntwo EDITED\nthree\n";
        let mut hunks = hunks_from_diff(base, "one\nTWO\nthree\n");

        rebase_hunks(&mut hunks, base, current);

        assert!(matches!(hunks[0].state, HunkState::Conflicted(_)), "got {:?}", hunks[0].state);
        assert_eq!(apply(current, &hunks), current, "and nothing is applied");
    }

    /// ADR-0006 §4 case 5 — the one that stops the loop feeling stupid.
    #[test]
    fn a_change_the_human_already_made_resolves_itself() {
        let base = "one\ntwo\nthree\n";
        let current = "one\nTWO\nthree\n"; // human made the agent's exact change
        let mut hunks = hunks_from_diff(base, "one\nTWO\nthree\n");

        rebase_hunks(&mut hunks, base, current);

        assert_eq!(hunks[0].state, HunkState::Satisfied, "not a conflict — already done");
        assert_eq!(apply(current, &hunks), current, "and applying it would not duplicate it");
    }

    /// A window scan would call this satisfied and drop the change silently: `}` lines
    /// are everywhere, so "the replacement text appears at the mapped position" is a
    /// coincidence, not proof. Only an identical hunk is proof.
    #[test]
    fn common_replacement_text_elsewhere_does_not_count_as_already_done() {
        let base = "fn a() {\n}\nfn b() {\n    x\n}\n";
        // The human deletes the body of `b`, leaving a bare `}` where the agent's
        // inserted `}` would map to.
        let current = "fn a() {\n}\nfn b() {\n}\n";
        // The agent wants to add a line inside `b`.
        let mut hunks = hunks_from_diff(base, "fn a() {\n}\nfn b() {\n    x\n    y\n}\n");

        rebase_hunks(&mut hunks, base, current);

        assert!(
            hunks.iter().all(|h| h.state != HunkState::Satisfied),
            "a coincidental match must not swallow the change: {:?}",
            hunks.iter().map(|h| h.state).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_larger_human_edit_covering_the_same_change_conflicts_rather_than_settling() {
        let base = "one\ntwo\nthree\n";
        // The human changed the agent's line *and* the one after it.
        let current = "one\nTWO\nTHREE\n";
        let mut hunks = hunks_from_diff(base, "one\nTWO\nthree\n");

        rebase_hunks(&mut hunks, base, current);
        assert!(
            matches!(hunks[0].state, HunkState::Conflicted(_)),
            "not an identical change, so it needs a human decision"
        );
    }

    #[test]
    fn one_conflicted_hunk_does_not_invalidate_its_siblings() {
        // ARCHITECTURE §9.3 requires per-hunk granularity, and this is what it buys.
        let base = "one\ntwo\nthree\nfour\n";
        let current = "one\ntwo EDITED\nthree\nfour\n";
        let mut hunks = hunks_from_diff(base, "one\nTWO\nthree\nFOUR\n");
        assert_eq!(hunks.len(), 2);

        rebase_hunks(&mut hunks, base, current);

        assert!(matches!(hunks[0].state, HunkState::Conflicted(_)));
        assert_eq!(hunks[1].state, HunkState::Clean, "the unrelated change still applies");
        assert_eq!(apply(current, &hunks), "one\ntwo EDITED\nthree\nFOUR\n");
    }

    #[test]
    fn a_hunk_whose_lines_were_deleted_conflicts() {
        let base = "one\ntwo\nthree\n";
        let current = "one\nthree\n"; // human deleted the line the agent wanted to change
        let mut hunks = hunks_from_diff(base, "one\nTWO\nthree\n");

        rebase_hunks(&mut hunks, base, current);
        assert!(matches!(hunks[0].state, HunkState::Conflicted(_)));
    }

    #[test]
    fn a_proposal_reports_whether_anything_is_left_to_review() {
        let base = "one\n";
        let mut proposal = EditProposal::new(
            ProposalId::new(1),
            PathBuf::from("/proj/a.rs"),
            Some(Version(3)),
            base.into(),
            "ONE\n".into(),
            base,
        );
        assert!(!proposal.is_settled());
        assert!(!proposal.has_conflicts());
        assert_eq!(proposal.applicable().count(), 1);

        proposal.hunks[0].state = HunkState::Satisfied;
        assert!(proposal.is_settled());
        assert_eq!(proposal.applicable().count(), 0);
    }

    /// The single-owner property: refreshing recomputes from the immutable original, so
    /// the same buffer state always yields the same verdict no matter how it was reached.
    #[test]
    fn refreshing_is_idempotent_and_derived_from_the_original() {
        let base = "one\ntwo\nthree\n";
        let mut proposal = EditProposal::new(
            ProposalId::new(1),
            PathBuf::from("/a"),
            None,
            base.into(),
            "one\nTWO\nthree\n".into(),
            base,
        );
        assert_eq!(proposal.hunks[0].state, HunkState::Clean);

        // The human edits the same line: the proposal must now conflict.
        proposal.refresh("one\ntwo EDITED\nthree\n");
        let conflicted = proposal.hunks.clone();
        assert!(matches!(conflicted[0].state, HunkState::Conflicted(_)));

        // Running it again changes nothing.
        proposal.refresh("one\ntwo EDITED\nthree\n");
        assert_eq!(proposal.hunks, conflicted, "refresh must be idempotent");

        // And undoing their edit brings it back — because it recomputes from the
        // original rather than from the previous verdict.
        proposal.refresh(base);
        assert_eq!(proposal.hunks[0].state, HunkState::Clean, "a conflict is not permanent");
    }
}

/// Why a fragment diff could not be placed in the buffer.
///
/// Both cases mean the same thing operationally — we do not know where the edit goes — but
/// they are worth telling apart, because they tell the human different things about why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorFailure {
    /// `old_text` is not in the buffer. The file moved under the agent.
    NotFound,
    /// `old_text` appears more than once. A short fragment matches in several places and
    /// nothing in the payload says which was meant.
    Ambiguous,
}

/// Rewrite a permission diff into the whole-file form the proposal machinery expects.
///
/// Agents disagree about what `oldText` means. opencode sends the entire document; Codex
/// sends only the lines it is touching. Both arrive as `content[] type: "diff"` with the
/// same two fields, so the shape has to be recovered by comparing against the buffer rather
/// than trusted (ADR-0016 §1a). Getting this backwards is not a rendering bug: feeding a
/// fragment to the whole-file derivation yields "replace the file with these few lines",
/// which renders as a tidy diff and deletes the rest of the file on accept.
///
/// Returns the text the buffer would have if the edit were applied.
pub fn whole_file_from_permission_diff(
    current: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, AnchorFailure> {
    // The whole-file case: the agent handed back the document it read. Nothing to anchor.
    if old_text == current {
        return Ok(new_text.to_string());
    }

    // Anything else is a fragment, and an empty one names every position in the file.
    if old_text.is_empty() {
        return Err(AnchorFailure::Ambiguous);
    }

    let mut matches = current.match_indices(old_text);
    let Some((at, _)) = matches.next() else {
        return Err(AnchorFailure::NotFound);
    };
    if matches.next().is_some() {
        return Err(AnchorFailure::Ambiguous);
    }

    let mut whole = String::with_capacity(current.len() + new_text.len());
    whole.push_str(&current[..at]);
    whole.push_str(new_text);
    whole.push_str(&current[at + old_text.len()..]);
    Ok(whole)
}

#[cfg(test)]
mod permission_diff_tests {
    use super::*;

    const FILE: &str =
        "use crate::cart::Cart;\n\npub fn subtotal() -> u32 { 0 }\n\npub fn vat() -> u32 { 1 }\n";

    /// opencode's shape: `oldText` is the document, byte for byte.
    #[test]
    fn a_whole_file_diff_is_taken_as_the_new_document() {
        let after = "use crate::cart::Cart;\n";
        assert_eq!(whole_file_from_permission_diff(FILE, FILE, after), Ok(after.to_string()));
    }

    /// Codex's shape: a few lines, spliced in place. The rest of the file must survive —
    /// this is the case that deletes a file if it is treated as whole-document text.
    #[test]
    fn a_fragment_is_spliced_and_the_rest_of_the_file_survives() {
        let old = "pub fn vat() -> u32 { 1 }";
        let new = "/// Total incl. VAT.\npub fn vat() -> u32 { 1 }";

        let whole = whole_file_from_permission_diff(FILE, old, new).expect("anchors");

        assert!(whole.starts_with("use crate::cart::Cart;"), "kept the head: {whole:?}");
        assert!(whole.contains("pub fn subtotal"), "kept the untouched function: {whole:?}");
        assert!(whole.contains("/// Total incl. VAT."), "made the edit: {whole:?}");
        assert_eq!(whole.len(), FILE.len() + "/// Total incl. VAT.\n".len());
    }

    #[test]
    fn a_fragment_that_is_not_there_reports_that_rather_than_guessing() {
        let missing = "pub fn shipping() -> u32 { 2 }";
        assert_eq!(
            whole_file_from_permission_diff(FILE, missing, "anything"),
            Err(AnchorFailure::NotFound)
        );
    }

    /// Picking the first of several matches is how the wrong function gets edited.
    #[test]
    fn a_fragment_matching_twice_is_ambiguous_rather_than_the_first_one() {
        let doubled = "fn f() {}\nfn g() {}\nfn f() {}\n";
        assert_eq!(
            whole_file_from_permission_diff(doubled, "fn f() {}", "fn h() {}"),
            Err(AnchorFailure::Ambiguous)
        );
    }

    #[test]
    fn an_empty_fragment_names_every_position_so_it_is_ambiguous() {
        assert_eq!(
            whole_file_from_permission_diff(FILE, "", "hello"),
            Err(AnchorFailure::Ambiguous)
        );
    }

    /// An empty buffer with an empty `oldText` is the whole-file case, not the empty-fragment
    /// one: the agent read nothing because there was nothing, and is writing the first draft.
    #[test]
    fn writing_the_first_content_into_an_empty_file_is_a_whole_file_diff() {
        assert_eq!(
            whole_file_from_permission_diff("", "", "fn main() {}"),
            Ok("fn main() {}".into())
        );
    }
}
