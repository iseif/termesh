//! [`ChangeSet`] — the position-composable change representation (ADR-0006 §1).
//!
//! A changeset is a *complete traversal* of a document: every character of the input is
//! either retained or deleted, and new text is inserted between. That totality is what
//! makes the two operations we actually care about possible — [`ChangeSet::compose`]
//! (fold two changes into one) and [`ChangeSet::map_pos`] (where does this position end
//! up?). The Phase-00 stub stored absolute `from`/`to` offsets, which cannot express
//! either, which is why its `rebase_onto` was a hardcoded `Err`.
//!
//! All lengths are **char** counts, matching `ropey`'s native indexing — never bytes,
//! never graphemes (ADR-0006 §1).

use ropey::Rope;

/// Which side of an insertion a mapped position lands on (ADR-0006 §2).
///
/// Only matters when text is inserted at *exactly* the position being mapped. Pending
/// proposal anchors always map with [`Assoc::After`], so agent text lands after text the
/// human already typed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assoc {
    Before,
    After,
}

/// What an applied change did to a range something else was anchored to.
///
/// The input to ADR-0006 §4's overlap policy: `Untouched` rebases, the other two are the
/// conflicting cases. Returned by [`ChangeSet::touches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeEffect {
    /// The range survived; mapping its endpoints is enough.
    Untouched,
    /// New text landed strictly inside the range (ADR-0006 §4 case 2). Applying a hunk
    /// over this would delete text the human typed without ever showing it to them.
    InsertedInside,
    /// Some of the range was deleted (ADR-0006 §4 case 4), so where the hunk belongs is
    /// no longer determined by the text it was written against.
    PartlyDeleted,
}

/// One step of a document traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Advance `n` chars unchanged.
    Retain(usize),
    /// Drop the next `n` chars.
    Delete(usize),
    /// Insert text at the current position.
    Insert(String),
}

/// An ordered, position-composable set of changes.
///
/// Construct with [`ChangeSet::builder`]; the ops are private because the canonical form
/// (merged runs, `Delete` before `Insert` at a replacement site) is an invariant that
/// [`compose`](Self::compose) and [`map_pos`](Self::map_pos) rely on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    ops: Vec<Operation>,
    len_before: usize,
    len_after: usize,
}

/// The minimal span this change set replaces, in pre-image and post-image char
/// offsets. `None` for an identity change set.
///
/// One span rather than one event per operation: it is what a single incremental
/// `didChange` needs, it is cheap on the typing hot path, and it is testable with no
/// server in the loop (ADR-0011 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangedSpan {
    pub before_start: usize,
    pub before_end: usize,
    pub after_start: usize,
    pub after_end: usize,
}

impl ChangeSet {
    pub fn builder(len_before: usize) -> ChangeSetBuilder {
        ChangeSetBuilder { ops: Vec::new(), len_before, consumed: 0 }
    }

    /// The no-op change over a document of `len` chars.
    pub fn identity(len: usize) -> Self {
        ChangeSet::builder(len).build()
    }

    /// A single replacement: replace chars `from..to` with `text`. The common shape for a
    /// keystroke, a paste, or one hunk of an agent diff.
    pub fn replace(len_before: usize, from: usize, to: usize, text: impl Into<String>) -> Self {
        let mut b = ChangeSet::builder(len_before);
        b.retain(from).delete(to - from).insert(text);
        b.build()
    }

    pub fn len_before(&self) -> usize {
        self.len_before
    }

    pub fn len_after(&self) -> usize {
        self.len_after
    }

    pub fn ops(&self) -> &[Operation] {
        &self.ops
    }

    /// Whether this changes nothing (every char retained, nothing inserted).
    pub fn is_identity(&self) -> bool {
        self.ops.iter().all(|op| matches!(op, Operation::Retain(_)))
    }

    pub fn changed_span(&self) -> Option<ChangedSpan> {
        let mut before = 0;
        let mut after = 0;
        let mut start = None;
        let mut end = (0, 0);

        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    before += n;
                    after += n;
                }
                Operation::Delete(n) => {
                    start.get_or_insert((before, after));
                    before += n;
                    end = (before, after);
                }
                Operation::Insert(text) => {
                    start.get_or_insert((before, after));
                    after += text.chars().count();
                    end = (before, after);
                }
            }
        }

        start.map(|(before_start, after_start)| ChangedSpan {
            before_start,
            before_end: end.0,
            after_start,
            after_end: end.1,
        })
    }

    /// Produce the changed document. Does not mutate the input — the caller decides when
    /// a new revision becomes current.
    pub fn apply(&self, text: &Rope) -> Rope {
        debug_assert_eq!(
            text.len_chars(),
            self.len_before,
            "changeset applied to a document it was not authored against"
        );

        let mut out = Rope::new();
        let mut pos = 0;
        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    out.append(Rope::from(text.slice(pos..pos + n)));
                    pos += n;
                }
                Operation::Delete(n) => pos += n,
                Operation::Insert(s) => {
                    let end = out.len_chars();
                    out.insert(end, s);
                }
            }
        }
        out
    }

    /// The changeset that undoes this one.
    ///
    /// Needs the pre-image because [`Operation::Delete`] does not record what it deleted.
    /// Per ADR-0006 §6 this is called at *apply* time, while `original` is still the live
    /// document — calling it at undo time would hand it the post-image.
    pub fn invert(&self, original: &Rope) -> ChangeSet {
        debug_assert_eq!(original.len_chars(), self.len_before, "invert needs the pre-image");

        let mut b = ChangeSet::builder(self.len_after);
        let mut pos = 0;
        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    b.retain(*n);
                    pos += n;
                }
                // What we deleted has to come back, so we need its text.
                Operation::Delete(n) => {
                    b.insert(original.slice(pos..pos + n).to_string());
                    pos += n;
                }
                Operation::Insert(s) => {
                    b.delete(s.chars().count());
                }
            }
        }
        b.build()
    }

    /// Where `pos` (an index into the *pre*-image) lands in the post-image.
    ///
    /// Positions inside a deleted range collapse to the start of the deletion — the text
    /// they pointed at is gone, and the start of what replaced it is the only honest
    /// answer. Callers that need to *detect* that case compare against the deletion
    /// instead of relying on the mapped value (ADR-0006 §4).
    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        let mut old = 0;
        let mut new = 0;

        for op in &self.ops {
            match op {
                Operation::Retain(n) => {
                    if pos < old + n {
                        return new + (pos - old);
                    }
                    old += n;
                    new += n;
                }
                Operation::Delete(n) => {
                    if pos < old + n {
                        return new;
                    }
                    old += n;
                }
                Operation::Insert(s) => {
                    let len = s.chars().count();
                    // Insertion exactly at the position we are mapping: the tie-break in
                    // ADR-0006 §2 decides, and it is the only place `assoc` is consulted.
                    if pos == old {
                        return match assoc {
                            Assoc::Before => new,
                            Assoc::After => new + len,
                        };
                    }
                    new += len;
                }
            }
        }
        // `pos` at or past the end of the pre-image.
        new + pos.saturating_sub(old)
    }

    /// What this change did to `from..to` — a range something *else* is anchored to.
    ///
    /// [`map_pos`](Self::map_pos) cannot answer this, by construction. It collapses a
    /// position inside a deleted range onto the deletion point, which is the same value
    /// it returns for a position at the deletion's start that was never destroyed; and it
    /// sees nothing at all when text is inserted *inside* a range, since both endpoints
    /// shift cleanly. Both are exactly the signals ADR-0006 §4 cases 2 and 4 turn on, so
    /// they get their own traversal rather than being re-derived from the outside.
    ///
    /// A zero-width range is an anchor: it counts as deleted only if the deletion
    /// strictly contains it, since a deletion merely *starting* there leaves it standing.
    pub fn touches(&self, from: usize, to: usize) -> RangeEffect {
        debug_assert!(from <= to, "range bounds reversed");

        let mut old = 0;
        let mut effect = RangeEffect::Untouched;

        for op in &self.ops {
            match op {
                Operation::Retain(n) => old += n,
                Operation::Delete(n) => {
                    let (start, end) = (old, old + n);
                    let overlaps = if from == to {
                        start < from && from < end
                    } else {
                        // Half-open intersection: touching at a boundary is adjacency,
                        // and ADR-0006 §4 case 6 says adjacency is not overlap.
                        start < to && from < end
                    };
                    if overlaps {
                        // Strictly worse than an insertion, so it wins immediately.
                        return RangeEffect::PartlyDeleted;
                    }
                    old = end;
                }
                Operation::Insert(_) => {
                    if from < old && old < to {
                        effect = RangeEffect::InsertedInside;
                    }
                }
            }
        }
        effect
    }

    /// The single changeset equivalent to applying `self` and then `other`.
    ///
    /// This is what lets a burst of keystrokes collapse into one undo step, and what a
    /// proposal is mapped through when the human has typed several times since it arrived.
    pub fn compose(&self, other: &ChangeSet) -> ChangeSet {
        assert_eq!(
            self.len_after, other.len_before,
            "cannot compose: the second changeset was authored against a different document"
        );

        let mut b = ChangeSet::builder(self.len_before);
        let mut a_iter = self.ops.iter().cloned();
        let mut b_iter = other.ops.iter().cloned();
        let mut a = a_iter.next();
        let mut c = b_iter.next();

        loop {
            match (a.take(), c.take()) {
                (None, None) => break,

                // Text `self` removed from the original never reaches `other`, so it is
                // deleted regardless of what `other` is doing. Must be tested before the
                // insert arm below, or a delete would be starved by a run of inserts.
                (Some(Operation::Delete(n)), rest) => {
                    b.delete(n);
                    a = a_iter.next();
                    c = rest;
                }

                // Text `other` adds is new to both — nothing in `self` corresponds to it.
                (rest, Some(Operation::Insert(s))) => {
                    b.insert(s);
                    a = rest;
                    c = b_iter.next();
                }

                (Some(Operation::Retain(i)), Some(Operation::Retain(j))) => {
                    let n = i.min(j);
                    b.retain(n);
                    a = carry(Operation::Retain(i - n), &mut a_iter);
                    c = carry(Operation::Retain(j - n), &mut b_iter);
                }

                // `self` kept it, `other` drops it.
                (Some(Operation::Retain(i)), Some(Operation::Delete(j))) => {
                    let n = i.min(j);
                    b.delete(n);
                    a = carry(Operation::Retain(i - n), &mut a_iter);
                    c = carry(Operation::Delete(j - n), &mut b_iter);
                }

                // `self` inserted it and `other` keeps it: it survives into the result.
                (Some(Operation::Insert(s)), Some(Operation::Retain(j))) => {
                    let len = s.chars().count();
                    let n = len.min(j);
                    b.insert(take_chars(&s, n));
                    a = carry_insert(&s, n, &mut a_iter);
                    c = carry(Operation::Retain(j - n), &mut b_iter);
                }

                // `self` inserted it and `other` deletes it: it never existed as far as
                // the composed change is concerned, so nothing is emitted at all.
                (Some(Operation::Insert(s)), Some(Operation::Delete(j))) => {
                    let len = s.chars().count();
                    let n = len.min(j);
                    a = carry_insert(&s, n, &mut a_iter);
                    c = carry(Operation::Delete(j - n), &mut b_iter);
                }

                // Both traversals cover the same document, so one running dry while the
                // other still has retains/deletes means the lengths lied.
                (None, Some(op)) | (Some(op), None) => {
                    unreachable!("changeset length mismatch, stranded {op:?}")
                }
            }
        }

        b.build()
    }
}

/// Keep `op` as the next item if it still has length, otherwise pull from `iter`.
fn carry(op: Operation, iter: &mut impl Iterator<Item = Operation>) -> Option<Operation> {
    match &op {
        Operation::Retain(0) | Operation::Delete(0) => iter.next(),
        _ => Some(op),
    }
}

/// The remainder of an insertion after `consumed` chars have been accounted for.
fn carry_insert(
    s: &str,
    consumed: usize,
    iter: &mut impl Iterator<Item = Operation>,
) -> Option<Operation> {
    let rest: String = s.chars().skip(consumed).collect();
    if rest.is_empty() {
        iter.next()
    } else {
        Some(Operation::Insert(rest))
    }
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Builds a [`ChangeSet`] in canonical form.
///
/// Canonical means: no zero-length or adjacent same-kind operations, and `Delete` always
/// precedes `Insert` at a replacement site. Both orderings describe the same edit, but
/// fixing one keeps [`ChangeSet::compose`] and [`ChangeSet::map_pos`] free of
/// order-dependent special cases.
#[derive(Debug)]
pub struct ChangeSetBuilder {
    ops: Vec<Operation>,
    len_before: usize,
    consumed: usize,
}

impl ChangeSetBuilder {
    pub fn retain(&mut self, n: usize) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.consumed += n;
        if let Some(Operation::Retain(prev)) = self.ops.last_mut() {
            *prev += n;
        } else {
            self.ops.push(Operation::Retain(n));
        }
        self
    }

    pub fn delete(&mut self, n: usize) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.consumed += n;

        // Slide in front of a trailing insert to keep the canonical Delete-then-Insert
        // ordering at a replacement site.
        let at = match self.ops.last() {
            Some(Operation::Insert(_)) => self.ops.len() - 1,
            _ => self.ops.len(),
        };
        if at > 0 {
            if let Some(Operation::Delete(prev)) = self.ops.get_mut(at - 1) {
                *prev += n;
                return self;
            }
        }
        self.ops.insert(at, Operation::Delete(n));
        self
    }

    pub fn insert(&mut self, text: impl Into<String>) -> &mut Self {
        let text = text.into();
        if text.is_empty() {
            return self;
        }
        if let Some(Operation::Insert(prev)) = self.ops.last_mut() {
            prev.push_str(&text);
        } else {
            self.ops.push(Operation::Insert(text));
        }
        self
    }

    /// Finish, implicitly retaining any unconsumed tail of the document.
    pub fn build(mut self) -> ChangeSet {
        let tail = self
            .len_before
            .checked_sub(self.consumed)
            .expect("changeset consumed more of the document than it has");
        self.retain(tail);

        let len_after = self
            .ops
            .iter()
            .map(|op| match op {
                Operation::Retain(n) => *n,
                Operation::Delete(_) => 0,
                Operation::Insert(s) => s.chars().count(),
            })
            .sum();

        ChangeSet { ops: self.ops, len_before: self.len_before, len_after }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    /// Apply a changeset and read the result back as a `String`.
    fn applied(text: &str, cs: &ChangeSet) -> String {
        cs.apply(&rope(text)).to_string()
    }

    #[test]
    fn identity_changes_nothing() {
        let cs = ChangeSet::identity(5);
        assert!(cs.is_identity());
        assert_eq!(applied("hello", &cs), "hello");
        assert_eq!(cs.len_after(), 5);
    }

    #[test]
    fn replace_swaps_a_range() {
        let cs = ChangeSet::replace(11, 6, 11, "there");
        assert_eq!(applied("hello world", &cs), "hello there");
    }

    #[test]
    fn insertion_and_deletion_report_their_output_length() {
        let mut b = ChangeSet::builder(5);
        b.retain(2).insert("XY").delete(3);
        let cs = b.build();
        assert_eq!(cs.len_before(), 5);
        assert_eq!(cs.len_after(), 4); // 2 retained + 2 inserted
        assert_eq!(applied("hello", &cs), "heXY");
    }

    #[test]
    fn the_builder_merges_runs_and_orders_delete_before_insert() {
        let mut b = ChangeSet::builder(10);
        b.retain(1).retain(1).insert("a").insert("b").delete(2).delete(1);
        let cs = b.build();

        // Adjacent same-kind ops merge; the delete slides in front of the insert.
        assert_eq!(
            cs.ops(),
            [
                Operation::Retain(2),
                Operation::Delete(3),
                Operation::Insert("ab".into()),
                Operation::Retain(5),
            ]
        );
    }

    #[test]
    fn zero_length_operations_are_dropped() {
        let mut b = ChangeSet::builder(3);
        b.retain(0).delete(0).insert("");
        assert!(b.build().is_identity());
    }

    #[test]
    fn an_insert_reports_a_zero_width_span_at_the_insert_point() {
        let mut builder = ChangeSet::builder(5);
        builder.retain(2).insert("xy").retain(3);
        let changes = builder.build();
        let span = changes.changed_span().expect("not identity");
        assert_eq!((span.before_start, span.before_end), (2, 2));
        assert_eq!((span.after_start, span.after_end), (2, 4));
    }

    #[test]
    fn a_delete_reports_the_removed_span_and_an_empty_replacement() {
        let mut builder = ChangeSet::builder(5);
        builder.retain(1).delete(2).retain(2);
        let changes = builder.build();
        let span = changes.changed_span().expect("not identity");
        assert_eq!((span.before_start, span.before_end), (1, 3));
        assert_eq!((span.after_start, span.after_end), (1, 1));
    }

    #[test]
    fn a_replace_reports_both_spans() {
        let changes = ChangeSet::replace(5, 1, 3, "abc");
        let span = changes.changed_span().expect("not identity");
        assert_eq!((span.before_start, span.before_end), (1, 3));
        assert_eq!((span.after_start, span.after_end), (1, 4));
    }

    #[test]
    fn several_edits_collapse_into_one_covering_span() {
        // Coarse on purpose: one content change per transaction (ADR-0011 §6).
        let mut builder = ChangeSet::builder(9);
        builder.retain(1).delete(1).retain(3).insert("zz").retain(4);
        let changes = builder.build();
        let span = changes.changed_span().expect("not identity");
        assert_eq!(span.before_start, 1);
        assert_eq!(span.before_end, 5);
    }

    #[test]
    fn an_identity_changeset_has_no_span() {
        assert!(ChangeSet::identity(4).changed_span().is_none());
    }

    // --- multi-byte safety -------------------------------------------------------
    //
    // Char indices, not bytes (ADR-0006 §1). These would panic or corrupt if any
    // arithmetic here were byte-based.

    #[test]
    fn positions_are_chars_not_bytes() {
        // "héllo" is 5 chars but 6 bytes.
        let cs = ChangeSet::replace(5, 1, 2, "e");
        assert_eq!(applied("héllo", &cs), "hello");
    }

    #[test]
    fn multibyte_text_survives_insertion_and_inversion() {
        let original = rope("naïve");
        let cs = ChangeSet::replace(5, 0, 0, "très ");
        let changed = cs.apply(&original);
        assert_eq!(changed.to_string(), "très naïve");
        assert_eq!(cs.invert(&original).apply(&changed).to_string(), "naïve");
    }

    // --- invert ------------------------------------------------------------------

    #[test]
    fn invert_round_trips_every_edit_shape() {
        for (text, cs) in [
            ("hello world", ChangeSet::replace(11, 6, 11, "there")), // replace
            ("hello", ChangeSet::replace(5, 5, 5, "!")),             // pure insert
            ("hello", ChangeSet::replace(5, 0, 2, "")),              // pure delete
            ("hello", ChangeSet::identity(5)),                       // no-op
        ] {
            let original = rope(text);
            let changed = cs.apply(&original);
            let undone = cs.invert(&original).apply(&changed);
            assert_eq!(undone.to_string(), text, "round-trip failed for {cs:?}");
        }
    }

    #[test]
    fn invert_of_invert_is_the_original_change() {
        let original = rope("hello world");
        let cs = ChangeSet::replace(11, 0, 5, "goodbye");
        let changed = cs.apply(&original);

        let back = cs.invert(&original);
        let forward = back.invert(&changed);
        assert_eq!(forward.apply(&original).to_string(), changed.to_string());
    }

    // --- compose -----------------------------------------------------------------

    #[test]
    fn compose_matches_applying_both_in_order() {
        let original = rope("hello world");
        let first = ChangeSet::replace(11, 0, 5, "goodbye"); // "goodbye world"
        let mid = first.apply(&original);
        let second = ChangeSet::replace(mid.len_chars(), 8, 13, "everyone"); // "goodbye everyone"

        let composed = first.compose(&second);
        assert_eq!(composed.apply(&original).to_string(), "goodbye everyone");
        assert_eq!(composed.len_before(), 11);
        assert_eq!(composed.len_after(), "goodbye everyone".chars().count());
    }

    #[test]
    fn compose_drops_text_that_was_inserted_then_deleted() {
        let original = rope("ac");
        let first = ChangeSet::replace(2, 1, 1, "b"); // "abc"
        let second = ChangeSet::replace(3, 1, 2, ""); // back to "ac"

        let composed = first.compose(&second);
        assert_eq!(composed.apply(&original).to_string(), "ac");
        assert!(composed.is_identity(), "the round trip should compose away entirely");
    }

    #[test]
    fn compose_is_associative() {
        let original = rope("abcdef");
        let a = ChangeSet::replace(6, 0, 1, "X"); // Xbcdef
        let b = ChangeSet::replace(6, 2, 3, "Y"); // XbYdef
        let c = ChangeSet::replace(6, 4, 5, "Z"); // XbYdZf

        let left = a.compose(&b).compose(&c);
        let right = a.compose(&b.compose(&c));
        assert_eq!(left.apply(&original).to_string(), right.apply(&original).to_string());
        assert_eq!(left, right, "composition should be associative on the nose");
    }

    #[test]
    fn compose_with_identity_is_a_no_op() {
        let cs = ChangeSet::replace(5, 1, 3, "XY");
        assert_eq!(ChangeSet::identity(5).compose(&cs), cs);
        assert_eq!(cs.compose(&ChangeSet::identity(cs.len_after())), cs);
    }

    #[test]
    fn typing_one_character_at_a_time_composes_into_one_change() {
        // The undo-grouping case: three keystrokes fold into a single changeset.
        let original = rope("() {}");
        let mut composed = ChangeSet::identity(original.len_chars());
        let mut text = original.clone();
        for (i, ch) in "abc".chars().enumerate() {
            let cs = ChangeSet::replace(text.len_chars(), 1 + i, 1 + i, ch.to_string());
            text = cs.apply(&text);
            composed = composed.compose(&cs);
        }
        assert_eq!(text.to_string(), "(abc) {}");
        assert_eq!(composed.apply(&original).to_string(), "(abc) {}");
    }

    #[test]
    #[should_panic(expected = "authored against a different document")]
    fn composing_mismatched_changesets_is_a_programming_error() {
        let a = ChangeSet::identity(5);
        let b = ChangeSet::identity(9);
        let _ = a.compose(&b);
    }

    // --- map_pos -----------------------------------------------------------------

    #[test]
    fn positions_before_a_change_are_untouched() {
        let cs = ChangeSet::replace(11, 6, 11, "there");
        assert_eq!(cs.map_pos(3, Assoc::After), 3);
    }

    #[test]
    fn positions_after_an_insertion_shift_by_its_length() {
        let cs = ChangeSet::replace(11, 0, 0, "abc");
        assert_eq!(cs.map_pos(5, Assoc::After), 8);
    }

    #[test]
    fn assoc_decides_only_at_the_insertion_point() {
        let cs = ChangeSet::replace(10, 4, 4, "XY");
        assert_eq!(cs.map_pos(4, Assoc::Before), 4, "Before stays put");
        assert_eq!(cs.map_pos(4, Assoc::After), 6, "After moves past the insert");
        // Neighbours are unambiguous, so assoc must not matter there.
        assert_eq!(cs.map_pos(3, Assoc::Before), cs.map_pos(3, Assoc::After));
        assert_eq!(cs.map_pos(5, Assoc::Before), cs.map_pos(5, Assoc::After));
    }

    #[test]
    fn positions_inside_a_deleted_range_collapse_to_its_start() {
        let cs = ChangeSet::replace(10, 2, 6, "");
        for pos in 2..6 {
            assert_eq!(cs.map_pos(pos, Assoc::After), 2, "pos {pos} should collapse");
        }
        assert_eq!(cs.map_pos(6, Assoc::After), 2, "the end of the range lands there too");
        assert_eq!(cs.map_pos(7, Assoc::After), 3, "past it, positions shift back");
    }

    #[test]
    fn the_end_of_the_document_maps_to_the_new_end() {
        let cs = ChangeSet::replace(5, 2, 5, "XY");
        assert_eq!(cs.map_pos(5, Assoc::After), cs.len_after());
    }

    // --- touches: the ADR-0006 §4 overlap table ----------------------------------
    //
    // One test per row. These are the inputs the rebase policy branches on, and
    // ARCHITECTURE.md §18 names this as a required test class.

    /// Case 1 — the human edited somewhere else entirely.
    #[test]
    fn an_edit_outside_the_range_leaves_it_untouched() {
        let hunk = (10, 20);
        for elsewhere in [
            ChangeSet::replace(40, 0, 5, "x"),   // before
            ChangeSet::replace(40, 25, 30, "x"), // after
        ] {
            assert_eq!(elsewhere.touches(hunk.0, hunk.1), RangeEffect::Untouched);
        }
    }

    /// Case 2 — the human typed inside text the hunk wants to replace. The
    /// non-negotiable one: applying over this would destroy their edit unseen.
    #[test]
    fn an_insertion_inside_the_range_is_detected() {
        let typed_inside = ChangeSet::replace(40, 15, 15, "hello");
        assert_eq!(typed_inside.touches(10, 20), RangeEffect::InsertedInside);
    }

    /// Case 3 — an insertion at the boundary is not inside. Assoc handles it, not this.
    #[test]
    fn an_insertion_at_either_boundary_is_not_inside() {
        for at in [10, 20] {
            let cs = ChangeSet::replace(40, at, at, "x");
            assert_eq!(
                cs.touches(10, 20),
                RangeEffect::Untouched,
                "insertion at {at} is a boundary case, resolved by Assoc"
            );
        }
    }

    /// Case 4 — the anchor is partly gone.
    #[test]
    fn a_deletion_overlapping_the_range_is_detected() {
        for (from, to) in [
            (5, 15),  // straddles the start
            (15, 25), // straddles the end
            (12, 18), // strictly inside
            (5, 25),  // swallows it whole
            (10, 20), // exactly the range
        ] {
            let cs = ChangeSet::replace(40, from, to, "");
            assert_eq!(
                cs.touches(10, 20),
                RangeEffect::PartlyDeleted,
                "deletion {from}..{to} should be seen"
            );
        }
    }

    /// Case 6 — adjacency is not overlap.
    #[test]
    fn a_deletion_ending_or_starting_at_the_boundary_does_not_overlap() {
        for (from, to) in [(5, 10), (20, 25)] {
            let cs = ChangeSet::replace(40, from, to, "");
            assert_eq!(
                cs.touches(10, 20),
                RangeEffect::Untouched,
                "deletion {from}..{to} only touches the boundary"
            );
        }
    }

    #[test]
    fn a_zero_width_anchor_survives_a_deletion_that_merely_starts_there() {
        let cs = ChangeSet::replace(40, 10, 15, "");
        assert_eq!(cs.touches(10, 10), RangeEffect::Untouched, "the anchor still stands");

        let containing = ChangeSet::replace(40, 8, 15, "");
        assert_eq!(containing.touches(10, 10), RangeEffect::PartlyDeleted, "swallowed");
    }

    #[test]
    fn deletion_outranks_insertion_when_a_change_does_both() {
        // A replace inside the range is a delete *and* an insert; the worse signal wins,
        // because it is the one that decides the hunk cannot be applied.
        let cs = ChangeSet::replace(40, 12, 18, "replacement");
        assert_eq!(cs.touches(10, 20), RangeEffect::PartlyDeleted);
    }

    #[test]
    fn an_untouched_range_is_exactly_what_map_pos_can_be_trusted_for() {
        // The pairing this API exists for: `touches` says whether the answer is
        // meaningful, `map_pos` says what it is.
        let cs = ChangeSet::replace(40, 0, 5, "xx");
        assert_eq!(cs.touches(10, 20), RangeEffect::Untouched);
        assert_eq!((cs.map_pos(10, Assoc::After), cs.map_pos(20, Assoc::After)), (7, 17));
    }

    /// ADR-0006 §2's worked example, asserted rather than assumed.
    ///
    /// A pending agent hunk anchored at 10 must ride forward over three human keystrokes
    /// at the same offset and end up *after* the typed text — and mapping through each
    /// keystroke individually must agree with mapping through the composed change. That
    /// equivalence is what lets ADR-0006 §3 rebase eagerly on every transaction.
    #[test]
    fn a_pending_anchor_rides_forward_over_keystrokes_at_the_same_offset() {
        let original = rope("0123456789tail");
        let mut text = original.clone();
        let mut anchor = 10;
        let mut composed = ChangeSet::identity(original.len_chars());

        for ch in "abc".chars() {
            let cs = ChangeSet::replace(text.len_chars(), anchor, anchor, ch.to_string());
            anchor = cs.map_pos(anchor, Assoc::After);
            composed = composed.compose(&cs);
            text = cs.apply(&text);
        }

        assert_eq!(text.to_string(), "0123456789abctail");
        assert_eq!(anchor, 13, "the hunk sits after the typed text, not inside it");
        assert_eq!(
            composed.map_pos(10, Assoc::After),
            anchor,
            "per-keystroke and batched rebasing must agree"
        );
    }
}
