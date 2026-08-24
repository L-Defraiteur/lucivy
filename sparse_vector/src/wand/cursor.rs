//! Cursors: forward-only readers over a sorted posting list.

use super::{Posting, RecordId, Weight};

/// A forward-only reader over one posting list, sorted by record id.
///
/// A cursor is always positioned either *on* an element (the current one,
/// returned by [`peek`](Self::peek)) or past the end. It only ever moves
/// forward: [`seek`](Self::seek) with a target at or below the current id
/// is a no-op that returns the current element, and moving backwards is
/// never required by the search loop.
///
/// Implementations back the cursor with any storage — a `Vec` in RAM, an
/// mmap'd slice, a decoded block — as long as they honour the ceiling
/// invariant: [`upper_bound`](Self::upper_bound) is at least the weight of
/// every element from the current position to the end of the list.
pub trait PostingCursor {
    /// The current element, or `None` once the cursor is past the end.
    fn peek(&self) -> Option<Posting>;

    /// Move to the next element (no-op past the end).
    fn advance(&mut self);

    /// Move forward to the first element whose id is `>= target` and return
    /// it. Returns `None` when no such element exists; the cursor is then
    /// past the end. Never moves backwards.
    fn seek(&mut self, target: RecordId) -> Option<Posting>;

    /// Number of elements not yet consumed, the current one included.
    fn remaining(&self) -> usize;

    /// Id of the last element of the whole list (regardless of position);
    /// `None` for an empty list.
    fn last_id(&self) -> Option<RecordId>;

    /// Move past the end.
    fn exhaust(&mut self);

    /// An upper bound on the weights of the elements not yet consumed, the
    /// current one included. `f32::NEG_INFINITY` once past the end.
    fn upper_bound(&self) -> Weight {
        self.peek().map_or(Weight::NEG_INFINITY, |p| p.tail_max)
    }

    /// A lower bound on the weights of the elements not yet consumed. The
    /// default is unbounded, which is always correct; storages that track a
    /// suffix minimum can tighten it so negative query weights prune too.
    fn lower_bound(&self) -> Weight {
        Weight::NEG_INFINITY
    }

    /// Consume every element with id `<= hi`, handing each `(id, weight)` to
    /// `visit` in id order. Leaves the cursor on the first element above
    /// `hi` (or past the end).
    fn drain_through(&mut self, hi: RecordId, mut visit: impl FnMut(RecordId, Weight)) {
        while let Some(p) = self.peek() {
            if p.id > hi {
                break;
            }
            visit(p.id, p.weight);
            self.advance();
        }
    }

    /// True once the cursor is past the end.
    fn is_exhausted(&self) -> bool {
        self.peek().is_none()
    }
}

/// Cursor over a slice of [`Posting`]s sorted by id.
#[derive(Clone, Debug)]
pub struct SliceCursor<'a> {
    items: &'a [Posting],
    pos: usize,
}

impl<'a> SliceCursor<'a> {
    /// Cursor at the start of `items`. The slice must be sorted by id with
    /// no duplicates and satisfy the ceiling invariant.
    pub fn new(items: &'a [Posting]) -> Self {
        debug_assert!(items.windows(2).all(|w| w[0].id < w[1].id));
        Self { items, pos: 0 }
    }

    /// Index of the current element within the slice.
    pub fn position(&self) -> usize {
        self.pos
    }
}

impl PostingCursor for SliceCursor<'_> {
    #[inline]
    fn peek(&self) -> Option<Posting> {
        self.items.get(self.pos).copied()
    }

    #[inline]
    fn advance(&mut self) {
        if self.pos < self.items.len() {
            self.pos += 1;
        }
    }

    fn seek(&mut self, target: RecordId) -> Option<Posting> {
        let rest = &self.items[self.pos.min(self.items.len())..];
        // First index in `rest` whose id is >= target.
        let step = rest.partition_point(|p| p.id < target);
        self.pos += step;
        self.peek()
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.items.len().saturating_sub(self.pos)
    }

    #[inline]
    fn last_id(&self) -> Option<RecordId> {
        self.items.last().map(|p| p.id)
    }

    fn exhaust(&mut self) {
        self.pos = self.items.len();
    }

    fn drain_through(&mut self, hi: RecordId, mut visit: impl FnMut(RecordId, Weight)) {
        let rest = &self.items[self.pos.min(self.items.len())..];
        let mut taken = 0;
        for p in rest {
            if p.id > hi {
                break;
            }
            visit(p.id, p.weight);
            taken += 1;
        }
        self.pos += taken;
    }
}
