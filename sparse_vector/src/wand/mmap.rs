//! Cursor over the posting entries of an mmap'd index.
//!
//! The mmap file stores, per entry, a record id, a weight and a
//! `max_next_weight` ceiling. The cursor folds that ceiling into the
//! inclusive form used here (`tail_max = max(weight, max_next_weight)`),
//! which is correct whether the file's ceiling includes the entry itself or
//! only the entries after it.

use crate::mmap_index::{MmapPostingData, PostingEntry};

use super::cursor::PostingCursor;
use super::{DimId, Posting, RecordId, Weight};

/// Cursor over one dimension of an mmap'd index: a position within the
/// dimension's entries, read straight from the mapping.
#[derive(Clone, Debug)]
pub struct MmapCursor<'a> {
    entries: &'a [PostingEntry],
    pos: usize,
}

impl<'a> MmapCursor<'a> {
    /// Cursor at the start of `entries`, which must be sorted by id with no
    /// duplicates (the writer guarantees it).
    pub fn new(entries: &'a [PostingEntry]) -> Self {
        Self { entries, pos: 0 }
    }

    /// Cursor for `dim` (a remapped dimension index), `None` when the
    /// dimension has no postings.
    pub fn open(data: &'a MmapPostingData, dim: DimId) -> Option<Self> {
        let entries = data.entries(dim as usize);
        (!entries.is_empty()).then(|| Self::new(entries))
    }

    #[inline]
    fn fold(e: &PostingEntry) -> Posting {
        Posting {
            id: e.record_id,
            weight: e.weight,
            tail_max: e.weight.max(e.max_next_weight),
        }
    }
}

impl PostingCursor for MmapCursor<'_> {
    #[inline]
    fn peek(&self) -> Option<Posting> {
        self.entries.get(self.pos).map(Self::fold)
    }

    #[inline]
    fn advance(&mut self) {
        if self.pos < self.entries.len() {
            self.pos += 1;
        }
    }

    fn seek(&mut self, target: RecordId) -> Option<Posting> {
        let rest = &self.entries[self.pos.min(self.entries.len())..];
        self.pos += rest.partition_point(|e| e.record_id < target);
        self.peek()
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.entries.len().saturating_sub(self.pos)
    }

    #[inline]
    fn last_id(&self) -> Option<RecordId> {
        self.entries.last().map(|e| e.record_id)
    }

    fn exhaust(&mut self) {
        self.pos = self.entries.len();
    }

    fn drain_through(&mut self, hi: RecordId, mut visit: impl FnMut(RecordId, Weight)) {
        let rest = &self.entries[self.pos.min(self.entries.len())..];
        let mut taken = 0;
        for e in rest {
            if e.record_id > hi {
                break;
            }
            visit(e.record_id, e.weight);
            taken += 1;
        }
        self.pos += taken;
    }
}
