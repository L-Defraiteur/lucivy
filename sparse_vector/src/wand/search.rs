//! The search loop: window-batched scoring with WAND pruning.

use super::cursor::PostingCursor;
use super::frontier::{Frontier, Lane, Skip};
use super::sink::{ScoreSink, TopKSink};
use super::{DimId, RecordId, Weight};

/// Largest accepted [`SearchOptions::window`]: bounds the scratch buffers
/// (4 M slots, 20 MB) whatever the caller asks for.
pub const MAX_WINDOW: u64 = 1 << 22;

/// Tunables of the search loop.
#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    /// Skip id ranges that cannot reach the top-k. Disabling it scores
    /// every record present in any query lane; results are identical.
    pub pruning: bool,
    /// Width, in record ids, of one scoring window (clamped to
    /// `1..=MAX_WINDOW`). Each window costs a pass over the scores buffer,
    /// so it should stay small when ids are sparse and large when they are
    /// dense. The default of 4096 suits dense, contiguous ids.
    pub window: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            pruning: true,
            window: 4096,
        }
    }
}

impl SearchOptions {
    pub fn exhaustive() -> Self {
        Self {
            pruning: false,
            ..Self::default()
        }
    }
}

/// Reusable buffers for window scoring. Keep one per thread and pass it to
/// [`search_with`] to avoid allocating on every query.
#[derive(Debug, Default)]
pub struct Scratch {
    scores: Vec<f32>,
    seen: Vec<bool>,
    lanes: Vec<(DimId, Weight)>,
}

impl Scratch {
    pub fn new() -> Self {
        Self::default()
    }

    fn prepare(&mut self, len: usize) {
        self.scores.clear();
        self.scores.resize(len, 0.0);
        self.seen.clear();
        self.seen.resize(len, false);
    }

    /// Fold the query into one `(dimension, weight)` per dimension: repeated
    /// dimensions have their weights summed (a query is a sparse vector, and
    /// a sparse vector has one coordinate per dimension), and dimensions
    /// whose weight is zero are dropped since they cannot change a score.
    fn merge_query(&mut self, query: &[(DimId, Weight)]) -> &[(DimId, Weight)] {
        self.lanes.clear();
        self.lanes.extend_from_slice(query);
        self.lanes.sort_by_key(|&(dim, _)| dim);
        self.lanes.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                earlier.1 += later.1;
                true
            } else {
                false
            }
        });
        self.lanes.retain(|&(_, w)| w != 0.0);
        &self.lanes
    }
}

/// Top-k search by dot product.
///
/// `query` is a list of `(dimension, weight)`; `cursors` resolves a
/// dimension to a cursor over its posting list (`None` when the dimension is
/// unknown or empty). Records rejected by `filter` are never returned. The
/// result holds at most `top_k` `(id, score)` pairs, score descending then
/// id ascending, where the score is the exact f32 sum over query dimensions
/// of `query_weight * record_weight`, accumulated in dimension order.
///
/// A dimension listed several times in the query counts once, with the sum
/// of its weights (see [`search_with`]).
pub fn search<C, F, R>(
    query: &[(DimId, Weight)],
    top_k: usize,
    filter: F,
    cursors: R,
) -> Vec<(RecordId, f32)>
where
    C: PostingCursor,
    F: Fn(RecordId) -> bool,
    R: FnMut(DimId) -> Option<C>,
{
    let mut scratch = Scratch::new();
    search_with(
        query,
        filter,
        cursors,
        TopKSink::new(top_k),
        SearchOptions::default(),
        &mut scratch,
    )
}

/// [`search`] with an explicit sink, options and scratch buffers.
///
/// Before any lane is built the query is normalised: duplicated dimensions
/// are merged by summing their weights and zero weights are dropped, so
/// `[(3, 1.0), (3, 0.5)]` scores exactly like `[(3, 1.5)]`.
///
/// Per window, the loop scores every record present in a lane, then offers
/// to the sink only the records whose score strictly exceeds the sink's
/// current threshold — the threshold only rises, so a record that cannot
/// beat it now never could — and only those go through `filter`. Pruning
/// (a sort of the lanes) is attempted once per distinct threshold value: a
/// window that did not move the threshold does not pay for it.
pub fn search_with<C, F, R, S>(
    query: &[(DimId, Weight)],
    filter: F,
    mut cursors: R,
    mut sink: S,
    options: SearchOptions,
    scratch: &mut Scratch,
) -> Vec<(RecordId, f32)>
where
    C: PostingCursor,
    F: Fn(RecordId) -> bool,
    R: FnMut(DimId) -> Option<C>,
    S: ScoreSink,
{
    let lanes = scratch
        .merge_query(query)
        .iter()
        .filter_map(|&(dim, w)| cursors(dim).map(|c| Lane::new(w, c)));
    let mut frontier = Frontier::new(lanes);
    let window = options.window.clamp(1, MAX_WINDOW);
    // Threshold the frontier was last pruned against.
    let mut pruned_at: Option<f32> = None;

    loop {
        frontier.retire_exhausted();
        if frontier.is_empty() {
            break;
        }

        if options.pruning {
            if let Some(threshold) = sink.threshold() {
                if pruned_at != Some(threshold) {
                    pruned_at = Some(threshold);
                    if frontier.skip_below(threshold) == Skip::Nothing {
                        break;
                    }
                }
            }
        }

        let Some(lo) = frontier.min_id() else {
            break;
        };
        // No lane holds an id beyond its last one, so the window never
        // needs to reach further than that.
        let last = frontier.max_last_id().unwrap_or(lo).max(lo);
        let hi = lo.saturating_add(window - 1).min(last);
        let len = (hi - lo) as usize + 1;
        scratch.prepare(len);
        frontier.score_window(lo, hi, &mut scratch.scores, &mut scratch.seen);

        // Records must strictly beat the threshold; `NEG_INFINITY` while the
        // sink still welcomes everything.
        let mut floor = sink.threshold().unwrap_or(f32::NEG_INFINITY);
        for slot in 0..len {
            if !scratch.seen[slot] {
                continue;
            }
            let score = scratch.scores[slot];
            if score <= floor {
                continue;
            }
            let id = lo + slot as RecordId;
            if filter(id) {
                sink.offer(id, score);
                floor = sink.threshold().unwrap_or(f32::NEG_INFINITY);
            }
        }
    }

    sink.into_results()
}
