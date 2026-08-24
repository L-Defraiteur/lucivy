// Simple top-K tracker using a BinaryHeap (min-heap via Reverse).
// Replaces Qdrant's common::top_k::TopK.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use ordered_float::OrderedFloat;

/// A scored point: record_id + score.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPointOffset {
    pub score: f32,
    pub idx: u64,
}

impl Eq for ScoredPointOffset {}

impl PartialOrd for ScoredPointOffset {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredPointOffset {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        OrderedFloat(self.score)
            .cmp(&OrderedFloat(other.score))
            .then(self.idx.cmp(&other.idx))
    }
}

/// Tracks the top-K highest-scoring elements.
/// Uses a min-heap so the lowest score is always accessible.
pub struct TopK {
    capacity: usize,
    heap: BinaryHeap<Reverse<ScoredPointOffset>>,
}

impl TopK {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            heap: BinaryHeap::with_capacity(capacity + 1),
        }
    }

    /// Push a scored point. If over capacity, evicts the lowest.
    pub fn push(&mut self, point: ScoredPointOffset) {
        if self.heap.len() < self.capacity {
            self.heap.push(Reverse(point));
        } else if let Some(min) = self.heap.peek() {
            if point.score > min.0.score {
                self.heap.pop();
                self.heap.push(Reverse(point));
            }
        }
    }

    /// Current threshold: the minimum score to beat to enter the top-K.
    /// Returns f32::MIN if not yet full.
    pub fn threshold(&self) -> f32 {
        if self.heap.len() < self.capacity {
            f32::MIN
        } else {
            self.heap.peek().map(|r| r.0.score).unwrap_or(f32::MIN)
        }
    }

    /// Number of elements currently tracked.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Convert to a sorted Vec (highest score first).
    pub fn into_vec(self) -> Vec<ScoredPointOffset> {
        let mut results: Vec<ScoredPointOffset> = self.heap.into_iter().map(|r| r.0).collect();
        results.sort_by(|a, b| OrderedFloat(b.score).cmp(&OrderedFloat(a.score)));
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_top_k() {
        let mut top = TopK::new(3);
        top.push(ScoredPointOffset { score: 1.0, idx: 1 });
        top.push(ScoredPointOffset { score: 5.0, idx: 2 });
        top.push(ScoredPointOffset { score: 3.0, idx: 3 });
        top.push(ScoredPointOffset { score: 2.0, idx: 4 });
        top.push(ScoredPointOffset { score: 7.0, idx: 5 });

        let results = top.into_vec();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].idx, 5); // 7.0
        assert_eq!(results[1].idx, 2); // 5.0
        assert_eq!(results[2].idx, 3); // 3.0
    }

    #[test]
    fn threshold() {
        let mut top = TopK::new(2);
        assert_eq!(top.threshold(), f32::MIN);
        top.push(ScoredPointOffset { score: 1.0, idx: 1 });
        assert_eq!(top.threshold(), f32::MIN); // not yet full
        top.push(ScoredPointOffset { score: 3.0, idx: 2 });
        assert_eq!(top.threshold(), 1.0); // now full, threshold = min
        top.push(ScoredPointOffset { score: 5.0, idx: 3 });
        assert_eq!(top.threshold(), 3.0); // evicted 1.0
    }
}
