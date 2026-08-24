//! Where scored records go: a top-k tracker or a plain collector.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use ordered_float::OrderedFloat;

use super::RecordId;

/// A scored record. Ordered so that a *greater* hit is a better one: higher
/// score first, then lower id.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub id: RecordId,
    pub score: f32,
}

impl Eq for Hit {}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        OrderedFloat(self.score)
            .cmp(&OrderedFloat(other.score))
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// Consumer of `(id, score)` pairs produced by the search loop.
///
/// The loop offers records in increasing id order and asks for the
/// [`threshold`](Self::threshold) before scoring a window: a record whose
/// score cannot *strictly* exceed it is not worth scoring. Because later
/// records have larger ids and ties go to the lower id, a score equal to the
/// threshold can never displace what the sink already holds.
pub trait ScoreSink {
    /// Offer one scored record.
    fn offer(&mut self, id: RecordId, score: f32);

    /// Score a new record must strictly beat to be retained, or `None` if
    /// every record is still welcome.
    fn threshold(&self) -> Option<f32>;

    /// Retained records, sorted by score descending then id ascending.
    fn into_results(self) -> Vec<(RecordId, f32)>;
}

/// Keeps the `k` best hits (highest score, ties to the lower id).
#[derive(Debug)]
pub struct TopKSink {
    k: usize,
    /// Min-heap on `Hit`: the top is the worst retained hit.
    heap: BinaryHeap<Reverse<Hit>>,
}

impl TopKSink {
    pub fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.saturating_add(1)),
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.k
    }

    fn worst(&self) -> Option<Hit> {
        self.heap.peek().map(|r| r.0)
    }
}

impl ScoreSink for TopKSink {
    fn offer(&mut self, id: RecordId, score: f32) {
        if self.k == 0 {
            return;
        }
        let hit = Hit { id, score };
        if self.heap.len() < self.k {
            self.heap.push(Reverse(hit));
        } else if let Some(worst) = self.worst() {
            if hit > worst {
                self.heap.pop();
                self.heap.push(Reverse(hit));
            }
        }
    }

    fn threshold(&self) -> Option<f32> {
        if self.k == 0 {
            return Some(f32::INFINITY);
        }
        if self.heap.len() < self.k {
            None
        } else {
            self.worst().map(|h| h.score)
        }
    }

    fn into_results(self) -> Vec<(RecordId, f32)> {
        let mut hits: Vec<Hit> = self.heap.into_iter().map(|r| r.0).collect();
        hits.sort_by(|a, b| b.cmp(a));
        hits.into_iter().map(|h| (h.id, h.score)).collect()
    }
}

/// Keeps everything it is offered.
#[derive(Debug, Default)]
pub struct CollectAll {
    hits: Vec<Hit>,
}

impl CollectAll {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

impl ScoreSink for CollectAll {
    fn offer(&mut self, id: RecordId, score: f32) {
        self.hits.push(Hit { id, score });
    }

    fn threshold(&self) -> Option<f32> {
        None
    }

    fn into_results(mut self) -> Vec<(RecordId, f32)> {
        self.hits.sort_by(|a, b| b.cmp(a));
        self.hits.into_iter().map(|h| (h.id, h.score)).collect()
    }
}
