//! WAND search over sparse-vector posting lists.
//!
//! This module is a self-contained implementation of the inverted-index side
//! of sparse-vector retrieval: sorted posting lists carrying weight ceilings,
//! cursors over them, a query frontier that knows how good any not-yet-scored
//! record could still be, and a batch search loop that scores windows of
//! record ids and prunes the ranges that can no longer reach the top-k.
//!
//! Layout:
//!
//! - [`Posting`] — one element of a list: `id`, `weight`, `tail_max`.
//! - [`cursor`] — the [`PostingCursor`] trait (peek / advance / seek /
//!   remaining / last_id / upper_bound) and [`SliceCursor`] over a slice of
//!   postings.
//! - [`postings`] — [`Postings`], the in-RAM list with upsert / delete and
//!   ceiling maintenance, plus [`PostingsBuilder`].
//! - [`mmap`] — [`MmapCursor`], a cursor over `mmap_index` posting entries.
//! - [`frontier`] — [`Frontier`], the set of active cursors for a query.
//! - [`sink`] — the [`ScoreSink`] trait, [`TopKSink`] and [`CollectAll`].
//! - [`search`] — [`search`](search::search) and [`search_with`].
//!
//! # Ceiling invariant
//!
//! Every posting stores `tail_max`, the maximum weight over *itself and every
//! element after it* in the list (an inclusive suffix maximum). Hence for
//! every position `i`, `tail_max[i] >= weight[j]` for all `j >= i`, and the
//! sequence `tail_max` is non-increasing. A cursor positioned at `i` exposes
//! `tail_max[i]` as its `upper_bound()`: no element it has not consumed yet
//! has a larger weight.
//!
//! # Pruning
//!
//! Records are scored in increasing id order, so the k-th best score seen so
//! far is a threshold that a later record must *strictly* exceed to enter
//! (ties are resolved in favour of the lower id, and the lower id was scored
//! first). The frontier bounds the score of any unscored record by the sum,
//! over lanes, of `max(0, query_weight * upper_bound)`; a record absent from
//! a lane contributes nothing, hence the clamp at zero. Sorting lanes by their
//! current id and accumulating those bounds gives a pivot: every id below the
//! pivot lane's current id lives only in lanes whose accumulated bound is
//! below the threshold, so those lanes can be seeked forward to the pivot in
//! one move. When even the full sum cannot beat the threshold, the search
//! ends.
//!
//! Negative query weights need a *lower* bound on the weights of a list to be
//! bounded; cursors report `f32::NEG_INFINITY` by default, which makes such a
//! lane's contribution unbounded and disables pruning for it while keeping
//! the result exact.
//!
//! # Ordering
//!
//! Results are sorted by score descending, then by record id ascending.
//!
//! # Queries
//!
//! A query is a list of `(dimension, weight)`. A dimension repeated in the
//! query is merged by summing its weights before the lanes are built, and
//! zero weights are dropped. Weights stored in posting lists are expected
//! to be non-zero (the index strips zeros at insert time); a stored zero is
//! still a presence and would be returned with a zero score.

pub mod cursor;
pub mod frontier;
pub mod mmap;
pub mod postings;
pub mod search;
pub mod sink;

#[cfg(test)]
mod tests;

pub use cursor::{PostingCursor, SliceCursor};
pub use frontier::{Frontier, Lane};
pub use mmap::MmapCursor;
pub use postings::{Postings, PostingsBuilder};
pub use search::{search, search_with, Scratch, SearchOptions};
pub use sink::{CollectAll, ScoreSink, TopKSink};

/// Identifier of an indexed record (document, node, row).
pub type RecordId = u64;

/// Identifier of a dimension of the sparse space (token id, feature id).
pub type DimId = u32;

/// A weight stored in a posting or carried by a query dimension.
pub type Weight = f32;

/// One element of a posting list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Posting {
    /// Record the weight belongs to.
    pub id: RecordId,
    /// Weight of the record on the list's dimension.
    pub weight: Weight,
    /// Maximum weight over this element and every element after it in the
    /// list. See the module documentation for the invariant.
    pub tail_max: Weight,
}

impl Posting {
    /// A posting whose ceiling is its own weight (a list of one element).
    pub fn solo(id: RecordId, weight: Weight) -> Self {
        Self {
            id,
            weight,
            tail_max: weight,
        }
    }
}
