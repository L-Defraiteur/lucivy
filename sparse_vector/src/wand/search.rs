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
    /// dense.
    pub window: u64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            pruning: true,
            window: 1024,
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
}

/// Top-k search by dot product.
///
/// `query` is a list of `(dimension, weight)`; `cursors` resolves a
/// dimension to a cursor over its posting list (`None` when the dimension is
/// unknown or empty). Records rejected by `filter` are never returned. The
/// result holds at most `top_k` `(id, score)` pairs, score descending then
/// id ascending, where the score is the exact f32 sum over query dimensions
/// of `query_weight * record_weight`, accumulated in query order.
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
    let lanes = query
        .iter()
        .filter(|&&(_, w)| w != 0.0)
        .filter_map(|&(dim, w)| cursors(dim).map(|c| Lane::new(w, c)));
    let mut frontier = Frontier::new(lanes);
    let window = options.window.clamp(1, MAX_WINDOW);

    loop {
        frontier.retire_exhausted();
        if frontier.is_empty() {
            break;
        }

        if options.pruning {
            if let Some(threshold) = sink.threshold() {
                if frontier.skip_below(threshold) == Skip::Nothing {
                    break;
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

        for slot in 0..len {
            if !scratch.seen[slot] {
                continue;
            }
            let id = lo + slot as RecordId;
            if filter(id) {
                sink.offer(id, scratch.scores[slot]);
            }
        }
    }

    sink.into_results()
}
