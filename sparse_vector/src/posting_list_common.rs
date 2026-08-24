// Based on Qdrant sparse index (https://github.com/qdrant/qdrant)
// Copyright 2021-2026 Qdrant Team <info@qdrant.tech>
// Licensed under Apache License 2.0
// Modified for rag3db sparse-vector extension

//! Common types shared between posting list variants.

/// Default max_next_weight for the last element in a posting list.
pub const DEFAULT_MAX_NEXT_WEIGHT: f32 = f32::NEG_INFINITY;

/// A posting element with pre-computed pruning info.
#[derive(Debug, Clone, PartialEq)]
#[repr(C)]
pub struct PostingElementEx {
    /// Record ID (internal node offset)
    pub record_id: u64,
    /// Weight of the record in this dimension
    pub weight: f32,
    /// Max weight of all subsequent elements in the posting list.
    /// Used for WAND-like pruning during search.
    pub max_next_weight: f32,
}

impl PostingElementEx {
    /// Initialize with negative infinity as max_next_weight.
    /// Must be updated at insertion time.
    pub fn new(record_id: u64, weight: f32) -> Self {
        Self {
            record_id,
            weight,
            max_next_weight: DEFAULT_MAX_NEXT_WEIGHT,
        }
    }
}

/// A simple posting element without pruning info.
#[derive(Debug, Clone, PartialEq)]
#[repr(C)]
pub struct PostingElement {
    pub record_id: u64,
    pub weight: f32,
}

impl From<PostingElementEx> for PostingElement {
    fn from(e: PostingElementEx) -> Self {
        Self {
            record_id: e.record_id,
            weight: e.weight,
        }
    }
}

/// Trait for iterating over posting list elements with skip abilities.
pub trait PostingListIter {
    /// Peek at the current element without advancing.
    fn peek(&mut self) -> Option<PostingElementEx>;

    /// Returns the last (largest) record_id in the posting list.
    fn last_id(&self) -> Option<u64>;

    /// Skip to the element with the given record_id.
    /// If not found, advances to the next element with id > record_id.
    /// Returns the found element, or None.
    fn skip_to(&mut self, record_id: u64) -> Option<PostingElementEx>;

    /// Skip to the end of the posting list.
    fn skip_to_end(&mut self);

    /// Number of elements remaining from current position.
    fn len_to_end(&self) -> usize;

    /// Current position index.
    fn current_index(&self) -> usize;

    /// Iterate over elements up to (inclusive) the given id.
    /// Calls `f(ctx, record_id, weight)` for each element.
    fn for_each_till_id<Ctx: ?Sized>(
        &mut self,
        id: u64,
        ctx: &mut Ctx,
        f: impl FnMut(&mut Ctx, u64, f32),
    );

    /// Whether this iterator provides reliable max_next_weight values.
    fn reliable_max_next_weight() -> bool;
}
