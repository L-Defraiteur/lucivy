//! The frontier: the set of active cursors of a query, and what it knows
//! about the records nobody has scored yet.

use super::cursor::PostingCursor;
use super::{RecordId, Weight};

/// One query dimension paired with its cursor.
#[derive(Debug)]
pub struct Lane<C> {
    pub query_weight: Weight,
    pub cursor: C,
}

impl<C: PostingCursor> Lane<C> {
    pub fn new(query_weight: Weight, cursor: C) -> Self {
        Self {
            query_weight,
            cursor,
        }
    }

    /// Id the cursor is on, if any.
    #[inline]
    pub fn current_id(&self) -> Option<RecordId> {
        self.cursor.peek().map(|p| p.id)
    }

    /// The most this lane can add to the score of any record not yet
    /// consumed by its cursor. A record absent from the lane adds nothing,
    /// so the bound is never below zero.
    pub fn headroom(&self) -> f64 {
        let q = self.query_weight as f64;
        let best = if q >= 0.0 {
            q * self.cursor.upper_bound() as f64
        } else {
            q * self.cursor.lower_bound() as f64
        };
        if best.is_nan() {
            // 0 * inf: an empty or unbounded lane with a zero weight.
            0.0
        } else {
            best.max(0.0)
        }
    }
}

/// Outcome of asking the frontier to skip what cannot beat a threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Some unscored record may still beat the threshold; cursors have been
    /// moved so that [`Frontier::min_id`] is the first such candidate.
    Candidates,
    /// No remaining record can beat the threshold: the search is over.
    Nothing,
}

/// Active cursors of a query.
///
/// Exhausted lanes are retired lazily by [`retire_exhausted`](Self::retire_exhausted)
/// (also called by the methods that move cursors), so `len()` counts lanes
/// that still have elements once that has run.
#[derive(Debug)]
pub struct Frontier<C> {
    lanes: Vec<Lane<C>>,
    /// Scratch: lane indices ordered by current id.
    order: Vec<usize>,
}

impl<C: PostingCursor> Frontier<C> {
    /// Build from lanes. Lanes with an exhausted cursor or a zero query
    /// weight are dropped right away.
    pub fn new(lanes: impl IntoIterator<Item = Lane<C>>) -> Self {
        let lanes: Vec<Lane<C>> = lanes
            .into_iter()
            .filter(|l| l.query_weight != 0.0 && !l.cursor.is_exhausted())
            .collect();
        let order = Vec::with_capacity(lanes.len());
        Self { lanes, order }
    }

    pub fn len(&self) -> usize {
        self.lanes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    pub fn lanes(&self) -> &[Lane<C>] {
        &self.lanes
    }

    /// Drop lanes whose cursor is past the end.
    pub fn retire_exhausted(&mut self) {
        self.lanes.retain(|l| !l.cursor.is_exhausted());
    }

    /// Smallest current id across lanes: the next record that could be
    /// scored. `None` when every lane is exhausted.
    pub fn min_id(&self) -> Option<RecordId> {
        self.lanes.iter().filter_map(Lane::current_id).min()
    }

    /// Largest last id across lanes: no record beyond it exists in any lane.
    pub fn max_last_id(&self) -> Option<RecordId> {
        self.lanes.iter().filter_map(|l| l.cursor.last_id()).max()
    }

    /// Upper bound on the score of any record no lane has consumed yet: the
    /// sum of every lane's headroom. `+inf` when a lane is unbounded.
    pub fn best_possible(&self) -> f64 {
        self.lanes.iter().map(Lane::headroom).sum()
    }

    /// Advance cursors past every id whose score cannot strictly exceed
    /// `threshold`.
    ///
    /// Lanes are sorted by current id and their headrooms accumulated in
    /// that order. The pivot is the first lane at which the running total
    /// could beat the threshold. A record with an id below the pivot's
    /// current id can only appear in lanes ordered before the pivot, whose
    /// combined headroom is below the threshold; those lanes are seeked to
    /// the pivot id. Without a pivot, nothing left can qualify.
    pub fn skip_below(&mut self, threshold: f32) -> Skip {
        self.retire_exhausted();
        if self.lanes.is_empty() {
            return Skip::Nothing;
        }

        self.order.clear();
        self.order.extend(0..self.lanes.len());
        let lanes = &self.lanes;
        self.order
            .sort_by_key(|&i| lanes[i].current_id().unwrap_or(RecordId::MAX));

        let mut acc = 0.0f64;
        let mut pivot_pos = None;
        for (pos, &i) in self.order.iter().enumerate() {
            acc += self.lanes[i].headroom();
            if can_beat(acc, threshold) {
                pivot_pos = Some(pos);
                break;
            }
        }

        let Some(pivot_pos) = pivot_pos else {
            return Skip::Nothing;
        };
        let pivot_id = self.lanes[self.order[pivot_pos]]
            .current_id()
            .expect("pivot lane is not exhausted");
        for &i in &self.order[..pivot_pos] {
            self.lanes[i].cursor.seek(pivot_id);
        }
        self.retire_exhausted();
        Skip::Candidates
    }

    /// Consume every element with id in `lo..=hi` from every lane and add
    /// `query_weight * weight` into `scores[id - lo]`, marking
    /// `seen[id - lo]`. Both buffers must be at least `hi - lo + 1` long
    /// and `scores` must be zeroed for the slots that matter. Contributions
    /// are added in lane order, so a record's score is the same f32 sum
    /// regardless of which window it lands in.
    pub fn score_window(&mut self, lo: RecordId, hi: RecordId, scores: &mut [f32], seen: &mut [bool]) {
        debug_assert!(lo <= hi);
        for lane in &mut self.lanes {
            let q = lane.query_weight;
            lane.cursor.drain_through(hi, |id, w| {
                debug_assert!(id >= lo, "cursor positioned before the window");
                let slot = (id - lo) as usize;
                scores[slot] += q * w;
                seen[slot] = true;
            });
        }
    }
}

/// Whether a score bounded above by `bound` could strictly exceed
/// `threshold`. The bound is accumulated in a different order than the
/// scores themselves (and in f64), so a few ulps of slack keep the answer
/// conservative under rounding.
pub(crate) fn can_beat(bound: f64, threshold: f32) -> bool {
    if bound.is_infinite() {
        return bound > 0.0;
    }
    let t = threshold as f64;
    let magnitude = bound.abs().max(t.abs());
    let slack = magnitude * (f32::EPSILON as f64 * 8.0);
    bound + slack > t
}
