//! Cursors and selections, and how they survive somebody else's edit.
//!
//! A cursor is just a zero-width [`Range`]. V1 ships a single cursor (ARCHITECTURE.md
//! §10's ship discipline), but the model is multi-range from the start so multi-cursor is
//! additive rather than a rewrite.

use crate::change::{Assoc, ChangeSet};

/// A selection with a fixed `anchor` and a moving `head`, in char offsets.
///
/// `head < anchor` is legal and means the selection was dragged backwards — the direction
/// is information (it decides which end moves next), so it is preserved rather than
/// normalized away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub anchor: usize,
    pub head: usize,
}

impl Range {
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// A zero-width range — an ordinary cursor.
    pub fn point(at: usize) -> Self {
        Self { anchor: at, head: at }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn len(&self) -> usize {
        self.end() - self.start()
    }

    /// Carry this range through a change made elsewhere in the document.
    ///
    /// The head maps with [`Assoc::After`] so that typing at the cursor pushes it along
    /// rather than leaving it stranded behind the character just inserted.
    pub fn map(&self, changes: &ChangeSet) -> Self {
        Self {
            anchor: changes.map_pos(self.anchor, Assoc::After),
            head: changes.map_pos(self.head, Assoc::After),
        }
    }
}

/// The set of ranges owned by a buffer. Always non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    ranges: Vec<Range>,
    primary: usize,
}

impl Selection {
    pub fn single(range: Range) -> Self {
        Self { ranges: vec![range], primary: 0 }
    }

    pub fn point(at: usize) -> Self {
        Self::single(Range::point(at))
    }

    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// The range that drives scrolling and single-cursor operations.
    pub fn primary(&self) -> Range {
        self.ranges[self.primary]
    }

    /// Carry every range through a change (ADR-0006 §7).
    pub fn map(&self, changes: &ChangeSet) -> Self {
        Self { ranges: self.ranges.iter().map(|r| r.map(changes)).collect(), primary: self.primary }
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self::point(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_an_empty_range() {
        let c = Range::point(4);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!((c.start(), c.end()), (4, 4));
    }

    #[test]
    fn a_backwards_range_keeps_its_direction_but_orders_its_bounds() {
        let r = Range::new(9, 3);
        assert_eq!((r.start(), r.end()), (3, 9));
        assert_eq!(r.len(), 6);
        assert_eq!(r.head, 3, "direction is information; it must survive");
    }

    #[test]
    fn an_edit_before_the_cursor_pushes_it_along() {
        let cursor = Range::point(10);
        let insert = ChangeSet::replace(20, 0, 0, "abc");
        assert_eq!(cursor.map(&insert), Range::point(13));
    }

    #[test]
    fn an_edit_after_the_cursor_leaves_it_alone() {
        let cursor = Range::point(4);
        let insert = ChangeSet::replace(20, 10, 10, "abc");
        assert_eq!(cursor.map(&insert), Range::point(4));
    }

    #[test]
    fn typing_at_the_cursor_carries_it_forward() {
        // The reason heads map with Assoc::After: otherwise the cursor would sit behind
        // every character you type.
        let cursor = Range::point(5);
        let typed = ChangeSet::replace(10, 5, 5, "x");
        assert_eq!(cursor.map(&typed), Range::point(6));
    }

    #[test]
    fn a_selection_spanning_a_deletion_collapses_onto_it() {
        let selection = Range::new(3, 9);
        let deleted = ChangeSet::replace(20, 2, 12, "");
        assert_eq!(selection.map(&deleted), Range::new(2, 2));
    }

    #[test]
    fn every_range_in_a_selection_maps() {
        let sel = Selection { ranges: vec![Range::point(1), Range::point(8)], primary: 1 };
        let mapped = sel.map(&ChangeSet::replace(20, 0, 0, "xx"));
        assert_eq!(mapped.ranges(), [Range::point(3), Range::point(10)]);
        assert_eq!(mapped.primary(), Range::point(10), "the primary index survives");
    }

    #[test]
    fn a_default_selection_is_a_cursor_at_the_start() {
        assert_eq!(Selection::default().primary(), Range::point(0));
    }
}
