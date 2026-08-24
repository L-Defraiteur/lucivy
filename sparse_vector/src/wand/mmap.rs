//! Adapter exposing `mmap_index` posting data as a [`PostingCursor`].
//!
//! The mmap file stores, per entry, a record id, a weight and a
//! `max_next_weight` ceiling. The adapter folds that ceiling into the
//! inclusive form used here (`tail_max = max(weight, max_next_weight)`),
//! which is correct whether the file's ceiling includes the entry itself or
//! only the entries after it.

use crate::mmap_index::{MmapPostingData, MmapPostingListIterator};
use crate::posting_list_common::PostingListIter;

use super::cursor::PostingCursor;
use super::{DimId, Posting, RecordId, Weight};

/// Cursor over one dimension of an mmap'd index.
pub struct MmapCursor<'a> {
    inner: MmapPostingListIterator<'a>,
    /// Cached current element, refreshed after every move so that `peek`
    /// needs no mutable access to the underlying iterator.
    current: Option<Posting>,
    last_id: Option<RecordId>,
}

impl<'a> MmapCursor<'a> {
    /// Wrap an iterator obtained from [`MmapPostingData::iter`].
    pub fn new(mut inner: MmapPostingListIterator<'a>) -> Self {
        let last_id = inner.last_id();
        let current = Self::snapshot(&mut inner);
        Self {
            inner,
            current,
            last_id,
        }
    }

    /// Cursor for `dim` (a remapped dimension index), `None` when the
    /// dimension has no postings.
    pub fn open(data: &'a MmapPostingData, dim: DimId) -> Option<Self> {
        data.iter(dim as usize).map(Self::new)
    }

    fn snapshot(inner: &mut MmapPostingListIterator<'a>) -> Option<Posting> {
        inner.peek().map(|e| Posting {
            id: e.record_id,
            weight: e.weight,
            tail_max: e.weight.max(e.max_next_weight),
        })
    }

    fn refresh(&mut self) {
        self.current = Self::snapshot(&mut self.inner);
    }
}

impl PostingCursor for MmapCursor<'_> {
    #[inline]
    fn peek(&self) -> Option<Posting> {
        self.current
    }

    fn advance(&mut self) {
        if let Some(cur) = self.current {
            // Consuming everything up to the current id consumes exactly
            // the current element, since ids are unique and sorted.
            self.inner.for_each_till_id(cur.id, &mut (), |_, _, _| {});
            self.refresh();
        }
    }

    fn seek(&mut self, target: RecordId) -> Option<Posting> {
        match self.current {
            Some(cur) if cur.id >= target => Some(cur),
            Some(_) => {
                // Positions on the exact id, or on the next larger one.
                let _ = self.inner.skip_to(target);
                self.refresh();
                self.current
            }
            None => None,
        }
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.inner.len_to_end()
    }

    #[inline]
    fn last_id(&self) -> Option<RecordId> {
        self.last_id
    }

    fn exhaust(&mut self) {
        self.inner.skip_to_end();
        self.current = None;
    }

    fn drain_through(&mut self, hi: RecordId, mut visit: impl FnMut(RecordId, Weight)) {
        if self.current.is_none() {
            return;
        }
        self.inner
            .for_each_till_id(hi, &mut (), |_, id, w| visit(id, w));
        self.refresh();
    }
}
