//! Styled overlays on buffer text (ARCHITECTURE.md §10).
//!
//! Three classes exist here from the first commit — syntax, diagnostics, and **agent
//! proposal hunks** — even though only hunks are wired up in Phase 03. That is deliberate
//! and §10 is explicit about it: designing the decoration system with agent hunks in mind
//! is a Phase-03 requirement, not a Phase-07 afterthought. A layer built for syntax
//! highlighting alone acquires assumptions (spans always exist in the buffer; spans are
//! always recomputable from the text) that agent hunks then violate.
//!
//! Decorations are stored as **char offsets**, like everything else in this crate. The
//! conversion to screen cells happens once, at the render boundary — see `ui::text`.

use termesh_core::ProposalId;

use crate::change::{Assoc, ChangeSet, RangeEffect};
use crate::{ConflictReason, HunkState};

/// Severity of a language-server diagnostic. Rendered in Phase 07; the class exists now
/// so the layer is not shaped around a single consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A syntax token class. Tree-sitter fills these in during the phase's last slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    Keyword,
    StringLit,
    Comment,
    Number,
    Type,
    Function,
}

/// Which side of a proposed change a hunk decoration marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkSide {
    /// Text the proposal would remove or replace. Present in the buffer, so it has a real
    /// range and can be struck through in place.
    Removed,
    /// Text the proposal would add. **Not in the buffer**, so its range is zero-width —
    /// an anchor saying "new text goes here". The content lives on the proposal and is
    /// rendered as a preview line rather than as a span over existing text.
    ///
    /// This is the case a syntax-only decoration model cannot express, and the reason
    /// this layer carries a side at all.
    Added,
}

/// What a decoration is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationClass {
    Syntax(SyntaxKind),
    Diagnostic(Severity),
    Hunk {
        proposal: ProposalId,
        side: HunkSide,
        state: HunkState,
    },
    /// A find/replace hit. `current` is the one the cursor is on.
    Match {
        current: bool,
    },
}

impl DecorationClass {
    /// Whether this decoration is *derived* data its producer can regenerate.
    ///
    /// Syntax and diagnostics are recomputed from the text after every edit, so a stale
    /// one is discarded rather than repaired. A hunk is not derived — it is a pending
    /// proposal, and losing it silently would lose the human's review.
    fn is_derived(&self) -> bool {
        matches!(
            self,
            DecorationClass::Syntax(_)
                | DecorationClass::Diagnostic(_)
                | DecorationClass::Match { .. }
        )
    }
}

/// A styled span over a char range of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoration {
    pub start: usize,
    pub end: usize,
    pub class: DecorationClass,
}

impl Decoration {
    pub fn new(start: usize, end: usize, class: DecorationClass) -> Self {
        Self { start, end, class }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A decoration clipped to one line, with offsets relative to that line's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineDecoration {
    pub start: usize,
    pub end: usize,
    pub class: DecorationClass,
}

/// The decorations attached to a buffer.
#[derive(Debug, Default, Clone)]
pub struct DecorationSet {
    items: Vec<Decoration>,
}

impl DecorationSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, decoration: Decoration) {
        self.items.push(decoration);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Decoration> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Drop every decoration a producer is about to replace.
    ///
    /// Per class, not all-derived-at-once: a re-parse must not wipe the find results, and
    /// a new search must not wipe the highlighting. Each producer clears its own.
    pub fn clear_syntax(&mut self) {
        self.items.retain(|d| !matches!(d.class, DecorationClass::Syntax(_)));
    }

    pub fn clear_matches(&mut self) {
        self.items.retain(|d| !matches!(d.class, DecorationClass::Match { .. }));
    }

    pub fn clear_diagnostics(&mut self) {
        self.items.retain(|d| !matches!(d.class, DecorationClass::Diagnostic(_)));
    }

    /// Drop every derived decoration, leaving pending agent hunks alone.
    pub fn clear_derived(&mut self) {
        self.items.retain(|d| !d.class.is_derived());
    }

    /// Drop every hunk belonging to a proposal — it was accepted, rejected, or withdrawn.
    pub fn remove_proposal(&mut self, proposal: ProposalId) {
        self.items.retain(
            |d| !matches!(d.class, DecorationClass::Hunk { proposal: p, .. } if p == proposal),
        );
    }

    /// Carry every decoration through an applied change.
    ///
    /// Positions map as usual, but *disturbance* is handled per class, and this is where
    /// ADR-0006 §4's policy reaches the screen:
    ///
    /// - **Derived** decorations (syntax, diagnostics) whose range was disturbed are
    ///   dropped. Their producer recomputes them; a half-mapped highlight is worse than
    ///   none, because it colours the wrong text.
    /// - **Hunks** are kept and marked [`HunkState::Conflicted`]. A pending proposal must
    ///   never vanish silently — the human is mid-review, and "your edit collided with
    ///   this change" is information they need.
    pub fn map(&mut self, changes: &ChangeSet) {
        self.items.retain_mut(|d| {
            let effect = changes.touches(d.start, d.end);

            if let DecorationClass::Hunk { state, .. } = &mut d.class {
                if let Some(reason) = ConflictReason::from_effect(effect) {
                    *state = HunkState::Conflicted(reason);
                }
            } else if effect != RangeEffect::Untouched {
                return false;
            }

            // A zero-width anchor uses `After` on both ends so an insertion at exactly
            // that point leaves the anchor after the new text, matching how pending
            // proposal anchors move (ADR-0006 §2).
            d.start = changes.map_pos(d.start, Assoc::After);
            d.end = changes.map_pos(d.end, Assoc::After);
            true
        });
    }

    /// The decorations overlapping `line_start..line_end`, clipped to it and rebased to
    /// offsets relative to `line_start`.
    ///
    /// Sorted by start so the renderer can walk them in order. Zero-width anchors are
    /// kept — they are exactly the "text goes here" markers of [`HunkSide::Added`].
    pub fn for_line(&self, line_start: usize, line_end: usize) -> Vec<LineDecoration> {
        let mut out: Vec<LineDecoration> = self
            .items
            .iter()
            .filter(|d| overlaps(d, line_start, line_end))
            .map(|d| LineDecoration {
                start: d.start.clamp(line_start, line_end) - line_start,
                end: d.end.clamp(line_start, line_end) - line_start,
                class: d.class,
            })
            .collect();
        out.sort_by_key(|d| (d.start, d.end));
        out
    }
}

/// Whether a decoration touches a line's char range.
///
/// A zero-width anchor counts when it sits anywhere in the line *including* its end, so
/// an insertion at the end of a line is drawn on that line rather than disappearing.
fn overlaps(d: &Decoration, line_start: usize, line_end: usize) -> bool {
    if d.is_empty() {
        return d.start >= line_start && d.start <= line_end;
    }
    d.start < line_end && d.end > line_start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(start: usize, end: usize, side: HunkSide) -> Decoration {
        Decoration::new(
            start,
            end,
            DecorationClass::Hunk { proposal: ProposalId::new(1), side, state: HunkState::Clean },
        )
    }

    fn syntax(start: usize, end: usize) -> Decoration {
        Decoration::new(start, end, DecorationClass::Syntax(SyntaxKind::Keyword))
    }

    fn state_of(set: &DecorationSet) -> Option<HunkState> {
        set.iter().find_map(|d| match d.class {
            DecorationClass::Hunk { state, .. } => Some(state),
            _ => None,
        })
    }

    // --- carrying decorations through edits ---------------------------------------

    #[test]
    fn an_edit_before_a_decoration_shifts_it() {
        let mut set = DecorationSet::new();
        set.push(syntax(10, 20));
        set.map(&ChangeSet::replace(40, 0, 0, "abc"));

        let d = set.iter().next().unwrap();
        assert_eq!((d.start, d.end), (13, 23));
    }

    #[test]
    fn an_edit_after_a_decoration_leaves_it_alone() {
        let mut set = DecorationSet::new();
        set.push(syntax(10, 20));
        set.map(&ChangeSet::replace(40, 30, 30, "abc"));

        let d = set.iter().next().unwrap();
        assert_eq!((d.start, d.end), (10, 20));
    }

    /// Derived decorations are regenerated by their producer, so a disturbed one is
    /// dropped rather than repaired — a half-mapped highlight colours the wrong text.
    #[test]
    fn a_disturbed_syntax_span_is_dropped() {
        let mut set = DecorationSet::new();
        set.push(syntax(10, 20));
        set.map(&ChangeSet::replace(40, 12, 18, "x"));
        assert!(set.is_empty(), "stale highlighting is worse than none");
    }

    /// A pending hunk is not derived: losing it silently would lose the human's review.
    #[test]
    fn a_disturbed_hunk_is_kept_and_marked_conflicted() {
        let mut set = DecorationSet::new();
        set.push(hunk(10, 20, HunkSide::Removed));
        set.map(&ChangeSet::replace(40, 12, 18, "mine"));

        assert_eq!(set.len(), 1, "the human is mid-review; it must not vanish");
        assert_eq!(state_of(&set), Some(HunkState::Conflicted(ConflictReason::AnchorDeleted)));
    }

    #[test]
    fn typing_inside_a_hunk_conflicts_it_by_the_right_reason() {
        let mut set = DecorationSet::new();
        set.push(hunk(10, 20, HunkSide::Removed));
        set.map(&ChangeSet::replace(40, 15, 15, "mine"));

        assert_eq!(state_of(&set), Some(HunkState::Conflicted(ConflictReason::EditedInsideRange)));
    }

    #[test]
    fn an_untouched_hunk_stays_clean_and_rides_forward() {
        let mut set = DecorationSet::new();
        set.push(hunk(10, 20, HunkSide::Removed));
        set.map(&ChangeSet::replace(40, 0, 0, "xx"));

        assert_eq!(state_of(&set), Some(HunkState::Clean));
        let d = set.iter().next().unwrap();
        assert_eq!((d.start, d.end), (12, 22));
    }

    #[test]
    fn a_zero_width_insertion_anchor_rides_forward_too() {
        let mut set = DecorationSet::new();
        set.push(hunk(10, 10, HunkSide::Added));
        set.map(&ChangeSet::replace(40, 0, 0, "abc"));

        let d = set.iter().next().unwrap();
        assert_eq!((d.start, d.end), (13, 13), "still zero-width, just moved");
    }

    #[test]
    fn a_conflicted_hunk_does_not_silently_go_clean_again() {
        let mut set = DecorationSet::new();
        set.push(hunk(10, 20, HunkSide::Removed));
        set.map(&ChangeSet::replace(40, 15, 15, "mine")); // conflict
        set.map(&ChangeSet::replace(44, 0, 0, "x")); // an unrelated later edit

        assert!(matches!(state_of(&set), Some(HunkState::Conflicted(_))));
    }

    // --- housekeeping --------------------------------------------------------------

    #[test]
    fn each_producer_clears_only_its_own_class() {
        // A re-parse must not wipe the find results, and a new search must not wipe the
        // highlighting.
        let mut set = DecorationSet::new();
        set.push(syntax(0, 5));
        set.push(Decoration::new(6, 8, DecorationClass::Match { current: true }));
        set.push(hunk(10, 20, HunkSide::Removed));

        set.clear_syntax();
        assert_eq!(set.len(), 2, "the match and the hunk survive a re-parse");

        set.clear_matches();
        assert_eq!(set.len(), 1, "the hunk survives a new search");
        assert!(matches!(set.iter().next().unwrap().class, DecorationClass::Hunk { .. }));
    }

    #[test]
    fn clearing_derived_decorations_leaves_pending_hunks_alone() {
        let mut set = DecorationSet::new();
        set.push(syntax(0, 5));
        set.push(Decoration::new(6, 8, DecorationClass::Diagnostic(Severity::Error)));
        set.push(hunk(10, 20, HunkSide::Removed));

        set.clear_derived();
        assert_eq!(set.len(), 1);
        assert!(matches!(set.iter().next().unwrap().class, DecorationClass::Hunk { .. }));
    }

    #[test]
    fn a_resolved_proposal_takes_only_its_own_hunks() {
        let mut set = DecorationSet::new();
        set.push(hunk(0, 5, HunkSide::Removed));
        set.push(Decoration::new(
            10,
            15,
            DecorationClass::Hunk {
                proposal: ProposalId::new(2),
                side: HunkSide::Removed,
                state: HunkState::Clean,
            },
        ));

        set.remove_proposal(ProposalId::new(1));
        assert_eq!(set.len(), 1, "the other proposal's review is untouched");
    }

    // --- clipping to a line ---------------------------------------------------------

    #[test]
    fn decorations_are_clipped_and_rebased_onto_their_line() {
        // Line spanning chars 10..20.
        let mut set = DecorationSet::new();
        set.push(syntax(5, 14)); // starts before the line
        set.push(syntax(16, 30)); // runs past its end

        let spans = set.for_line(10, 20);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end), (0, 4));
        assert_eq!((spans[1].start, spans[1].end), (6, 10));
    }

    #[test]
    fn decorations_on_other_lines_are_excluded() {
        let mut set = DecorationSet::new();
        set.push(syntax(0, 5));
        set.push(syntax(30, 35));
        assert!(set.for_line(10, 20).is_empty());
    }

    #[test]
    fn spans_come_back_in_order_so_the_renderer_can_walk_them() {
        let mut set = DecorationSet::new();
        set.push(syntax(18, 20));
        set.push(syntax(10, 12));
        set.push(syntax(14, 16));

        let starts: Vec<usize> = set.for_line(10, 20).iter().map(|d| d.start).collect();
        assert_eq!(starts, [0, 4, 8]);
    }

    #[test]
    fn an_insertion_anchor_at_the_end_of_a_line_is_drawn_on_that_line() {
        // Otherwise "add a line after this one" would render nowhere at all.
        let mut set = DecorationSet::new();
        set.push(hunk(20, 20, HunkSide::Added));

        assert_eq!(set.for_line(10, 20).len(), 1, "belongs to the line it ends");
    }

    #[test]
    fn a_decoration_touching_only_a_boundary_does_not_bleed_onto_the_next_line() {
        let mut set = DecorationSet::new();
        set.push(syntax(5, 10)); // ends exactly where the line starts
        assert!(set.for_line(10, 20).is_empty());
    }
}
