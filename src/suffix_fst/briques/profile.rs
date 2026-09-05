//! Opt-in profiling of the v3 literal walk.
//!
//! Answers one question the timings alone cannot: within a relaxed `contains`,
//! which stage actually spends the time — the FST walk, the sibling DFS, or the
//! posting resolution — and, inside the word resolution, whether the cost is the
//! separator verification itself or the nested scan that calls it.
//!
//! Enabled by setting `V3_PROFILE`. When unset, every hook is a single relaxed
//! atomic load, so the instrumented build is the shipped build.
//!
//! Counters are process-global and additive: a prescan runs once per segment, so
//! a single query over 320 segments accumulates 320 contributions. Call
//! [`reset`] before a query and [`dump`] after it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

/// Wall-clock nanoseconds per stage.
#[derive(Default)]
pub struct Counters {
    /// Single-token stage: FST candidates plus their direct posting resolution.
    pub ns_single: AtomicU64,
    /// `verify_literal`: window rebuild + contains check per match.
    pub ns_verify: AtomicU64,
    /// Placing the matches' byte spans (`orchestrator::place_spans`).
    pub ns_place: AtomicU64,
    /// Chunk pipeline: falling walk building cross-chunk chains.
    pub ns_chunk_walk: AtomicU64,
    /// Chunk pipeline: sibling-table DFS supplementing the chains.
    pub ns_chunk_sibling: AtomicU64,
    /// Chunk pipeline: resolving chains against postings (posmap or strict).
    pub ns_chunk_resolve: AtomicU64,
    /// Strict mode: short-head occurrences anchored on their second token.
    pub ns_chunk_anchored: AtomicU64,
    /// Word pipeline: falling walk building cross-word chains.
    pub ns_word_walk: AtomicU64,
    /// Word pipeline: sibling-table DFS supplementing the chains.
    pub ns_word_sibling: AtomicU64,
    /// Word pipeline: resolving chains through WordSfxPost / posmap / termtexts META.
    pub ns_word_resolve: AtomicU64,

    /// Fuzzy pipeline stages.
    /// Time resolving the query's trigrams (or pieces) into hits.
    pub ns_fz_resolve: AtomicU64,
    /// Time grouping trigram hits into per-document candidate chains.
    pub ns_fz_chains: AtomicU64,
    /// Time rebuilding the text window around each candidate chain.
    pub ns_fz_window: AtomicU64,
    /// Time in the edit-distance / Jaro-Winkler alignment over the windows.
    pub ns_fz_dp: AtomicU64,
    /// Trigram hits produced by the resolve stage.
    pub n_fz_hits: AtomicU64,
    /// Candidate chains (hit regions) built from the trigram hits.
    pub n_fz_regions: AtomicU64,
    /// Windows actually aligned (candidate chains above the pigeonhole threshold).
    pub n_fz_windows: AtomicU64,
    /// Windows rejected by the alignment as holding no occurrence.
    pub n_fz_rejected: AtomicU64,
    /// Postings decoded by resolve_doc while rebuilding windows.
    pub n_fz_window_postings: AtomicU64,
    /// Distinct (doc, byte range) fuzzy occurrences reported after verification.
    pub n_fz_spans: AtomicU64,

    /// Relaxed literal: segments where the chunk chains were skipped because
    /// `.termtexts` proves no word exceeds the suffix cap, vs segments that
    /// still had to walk them (long word present, or stat unknown).
    pub n_relaxed_chunk_skipped: AtomicU64,
    /// Relaxed-literal segments where the chunk chains still had to be walked.
    pub n_relaxed_chunk_walked: AtomicU64,

    /// Chains handed to each resolve stage.
    pub n_chunk_chains: AtomicU64,
    /// Word chains (0x02 partition) handed to the word resolve stage.
    pub n_word_chains: AtomicU64,
    /// Postings materialised per chain position in the word resolve.
    pub n_word_entries: AtomicU64,
    /// Iterations of the `active x entries` nested scan.
    pub n_word_pairs: AtomicU64,
    /// Calls to `intermediates_are_pure_sep`, and positions it scanned.
    pub n_puresep_calls: AtomicU64,
    /// Positions scanned across all `intermediates_are_pure_sep` calls.
    pub n_puresep_positions: AtomicU64,

    /// Splits fed to `build_chains_from_splits`, and the FST work they trigger.
    /// Every remainder is a suffix of the query, so `distinct` is bounded by the
    /// query length however many splits there are.
    pub n_bcfs_splits: AtomicU64,
    /// `_reqs` = times a remainder was needed; `_calls` = times it was actually
    /// computed. The gap is what the memo saves.
    pub n_bcfs_fst_reqs: AtomicU64,
    /// FST lookups of a remainder actually computed (memo misses).
    pub n_bcfs_fst_calls: AtomicU64,
    /// Falling walks of a remainder requested.
    pub n_bcfs_walk_reqs: AtomicU64,
    /// Falling walks of a remainder actually computed (memo misses).
    pub n_bcfs_walk_calls: AtomicU64,
    /// Distinct remainders seen, i.e. the size of the FST memo.
    pub n_bcfs_distinct_rem: AtomicU64,

    /// Postings materialised by the chunk chain resolve, and the pair iterations
    /// they feed.
    pub n_chain_first: AtomicU64,
    /// Postings materialised for the non-head positions of chunk chains.
    pub n_chain_entries: AtomicU64,
    /// Pair iterations performed while joining consecutive chunk-chain positions.
    pub n_chain_pairs: AtomicU64,

    /// posmap-driven chain resolution: lookups made, survivors whose posting was
    /// then fetched, and survivors whose posting did NOT hold the position posmap
    /// claimed (must stay 0 — anything else means posmap and sfxpost disagree).
    pub n_posmap_lookups: AtomicU64,
    /// posmap survivors whose posting was then fetched.
    pub n_posmap_survivors: AtomicU64,
    /// posmap survivors whose posting did not hold the claimed position (must stay 0).
    pub n_posmap_mismatch: AtomicU64,
    /// (doc, position) written twice with different ordinals at index time.
    pub n_posmap_collisions: AtomicU64,
    /// Same for word_pos_map: two words starting at one position.
    pub n_wordmap_collisions: AtomicU64,
    /// Word-pipeline resolution through word_pos_map/posmap.
    pub n_wordmap_lookups: AtomicU64,
    /// Word-map survivors whose posting was then fetched.
    pub n_wordmap_survivors: AtomicU64,
    /// Word-map survivors whose posting did not hold the claimed position (must stay 0).
    pub n_wordmap_mismatch: AtomicU64,

    /// Chunk chains before and after structural dedup, and matches emitted.
    pub n_chains_raw: AtomicU64,
    /// Chunk chains remaining after structural dedup.
    pub n_chains_distinct: AtomicU64,
    /// Matches returned by `find_literal_v3`, summed over segments.
    pub n_matches_emitted: AtomicU64,
    /// Strict posmap resolution: chains with a shared first list, groups formed,
    /// and dispatch-map inserts made for them.
    pub n_chains_shared: AtomicU64,
    /// Groups formed from chains sharing their first posting list.
    pub n_groups_shared: AtomicU64,
    /// Entries inserted into the dispatch map for those groups.
    pub n_dispatch_inserts: AtomicU64,

    // ── Dictionary mode: where the per-segment overhead goes ──
    /// `keep_in_segment`: shard-wide items scanned.
    pub n_cut_items: AtomicU64,
    /// `keep_in_segment`: items kept.
    pub n_cut_kept: AtomicU64,
    /// `keep_in_segment`: time.
    pub ns_cut: AtomicU64,
    /// Sibling DFS: steps (pops).
    pub n_sib_steps: AtomicU64,
    /// Sibling DFS: siblings visited.
    pub n_sib_visited: AtomicU64,
    /// Sibling DFS: time in `siblings()`.
    pub ns_sib_lookup: AtomicU64,
    /// Sibling DFS: time in termtexts text + meta.
    pub ns_sib_text: AtomicU64,
    /// Second-token anchored: the FST part (candidates, walk, chains).
    pub ns_anch_fst: AtomicU64,
    /// Second-token anchored: the posmap resolution.
    pub ns_anch_resolve: AtomicU64,
    /// Second-token anchored: the backward check.
    pub ns_anch_back: AtomicU64,
    /// Memo cell lookups.
    pub n_memo_lookups: AtomicU64,
}

fn counters() -> &'static Counters {
    static C: OnceLock<Counters> = OnceLock::new();
    C.get_or_init(Counters::default)
}

/// Whether profiling is on. Read once from the environment, then cached.
pub fn enabled() -> bool {
    static ON: AtomicBool = AtomicBool::new(false);
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        ON.store(std::env::var("V3_PROFILE").is_ok(), Ordering::Relaxed);
    });
    ON.load(Ordering::Relaxed)
}

/// Add `n` to one counter, selected by field accessor.
#[inline]
pub fn bump(field: fn(&Counters) -> &AtomicU64, n: u64) {
    if enabled() {
        field(counters()).fetch_add(n, Ordering::Relaxed);
    }
}

/// A stage timer. `None` when profiling is off, so the call site pays nothing.
///
/// On wasm32 `Instant` is unavailable; the timer degrades to counting only.
pub struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: Option<std::time::Instant>,
}

impl Timer {
    /// Start a timer; a no-op holding `None` when profiling is off.
    #[inline]
    pub fn start() -> Timer {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Timer { start: if enabled() { Some(std::time::Instant::now()) } else { None } }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Timer {}
        }
    }

    /// Charge the elapsed time to one stage and restart the timer: for a
    /// loop that charges each iteration.
    #[inline]
    pub fn stop_keep(&mut self, field: fn(&Counters) -> &AtomicU64) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(t0) = self.start {
            field(counters()).fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.start = Some(std::time::Instant::now());
        }
        #[cfg(target_arch = "wasm32")]
        let _ = field;
    }

    /// Charge the elapsed time to one stage.
    #[inline]
    pub fn stop(self, field: fn(&Counters) -> &AtomicU64) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(t0) = self.start {
            field(counters()).fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = field;
    }
}

/// Zero every counter. Call before the query you want to measure.
pub fn reset() {
    let c = counters();
    for a in [
        &c.ns_single, &c.ns_verify, &c.ns_place, &c.ns_chunk_walk, &c.ns_chunk_sibling, &c.ns_chunk_resolve, &c.ns_chunk_anchored,
        &c.ns_word_walk, &c.ns_word_sibling, &c.ns_word_resolve,
        &c.n_relaxed_chunk_skipped, &c.n_relaxed_chunk_walked,
        &c.n_chunk_chains, &c.n_word_chains, &c.n_word_entries, &c.n_word_pairs,
        &c.n_puresep_calls, &c.n_puresep_positions,
        &c.n_bcfs_splits, &c.n_bcfs_fst_reqs, &c.n_bcfs_fst_calls,
        &c.n_bcfs_walk_reqs, &c.n_bcfs_walk_calls, &c.n_bcfs_distinct_rem,
        &c.n_chain_first, &c.n_chain_entries, &c.n_chain_pairs,
        &c.n_posmap_lookups, &c.n_posmap_survivors, &c.n_posmap_mismatch,
        &c.n_posmap_collisions, &c.n_wordmap_collisions,
        &c.n_wordmap_lookups, &c.n_wordmap_survivors, &c.n_wordmap_mismatch,
        &c.ns_fz_resolve, &c.ns_fz_chains, &c.ns_fz_window, &c.ns_fz_dp,
        &c.n_fz_hits, &c.n_fz_regions, &c.n_fz_windows, &c.n_fz_rejected,
        &c.n_fz_window_postings, &c.n_fz_spans,
        &c.n_chains_raw, &c.n_chains_distinct, &c.n_matches_emitted,
        &c.n_chains_shared, &c.n_groups_shared, &c.n_dispatch_inserts,
        &c.n_cut_items, &c.n_cut_kept, &c.ns_cut,
        &c.n_sib_steps, &c.n_sib_visited, &c.ns_sib_lookup, &c.ns_sib_text,
        &c.ns_anch_fst, &c.ns_anch_resolve, &c.ns_anch_back, &c.n_memo_lookups,
    ] {
        a.store(0, Ordering::Relaxed);
    }
}

/// Human-readable report of everything accumulated since [`reset`].
pub fn dump() -> String {
    let c = counters();
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let ms = |a: &AtomicU64| g(a) as f64 / 1e6;

    let total = ms(&c.ns_single) + ms(&c.ns_verify) + ms(&c.ns_place) + ms(&c.ns_chunk_walk) + ms(&c.ns_chunk_sibling)
        + ms(&c.ns_chunk_resolve) + ms(&c.ns_chunk_anchored) + ms(&c.ns_word_walk) + ms(&c.ns_word_sibling)
        + ms(&c.ns_word_resolve);
    let pct = |v: f64| if total > 0.0 { v / total * 100.0 } else { 0.0 };

    let mut s = String::new();
    s.push_str(&format!("  stage totals (sum over segments), {total:.1}ms accounted\n"));
    for (name, v) in [
        ("single (candidates+resolve)", ms(&c.ns_single)),
        ("verify_literal (window+contains)", ms(&c.ns_verify)),
        ("place_spans (posmap+meta)", ms(&c.ns_place)),
        ("chunk walk", ms(&c.ns_chunk_walk)),
        ("chunk sibling DFS", ms(&c.ns_chunk_sibling)),
        ("chunk resolve", ms(&c.ns_chunk_resolve)),
        ("chunk 2nd-token anchored", ms(&c.ns_chunk_anchored)),
        ("word walk", ms(&c.ns_word_walk)),
        ("word sibling DFS", ms(&c.ns_word_sibling)),
        ("word resolve", ms(&c.ns_word_resolve)),
    ] {
        s.push_str(&format!("    {name:<28} {v:>9.1}ms  {:>5.1}%\n", pct(v)));
    }
    if g(&c.n_fz_windows) > 0 || g(&c.n_fz_hits) > 0 {
        s.push_str(&format!(
            "  fuzzy: resolve {:.1}ms chains {:.1}ms window {:.1}ms dp {:.1}ms | hits={} regions={} windows={} rejected={} window_postings={} spans={}\n",
            ms(&c.ns_fz_resolve), ms(&c.ns_fz_chains), ms(&c.ns_fz_window), ms(&c.ns_fz_dp),
            g(&c.n_fz_hits), g(&c.n_fz_regions), g(&c.n_fz_windows), g(&c.n_fz_rejected),
            g(&c.n_fz_window_postings), g(&c.n_fz_spans)));
    }
    s.push_str(&format!(
        "  chains: chunk={} word={}  word_entries={}  word_pairs={}  relaxed chunk walk: skipped={} walked={}\n",
        g(&c.n_chunk_chains), g(&c.n_word_chains),
        g(&c.n_word_entries), g(&c.n_word_pairs),
        g(&c.n_relaxed_chunk_skipped), g(&c.n_relaxed_chunk_walked),
    ));
    s.push_str(&format!(
        "  intermediates_are_pure_sep: {} calls, {} positions scanned\n",
        g(&c.n_puresep_calls), g(&c.n_puresep_positions),
    ));
    s.push_str(&format!(
        "  build_chains_from_splits: {} splits | fst {}/{} computed/requested | walk {}/{} | {} distinct remainders\n",
        g(&c.n_bcfs_splits),
        g(&c.n_bcfs_fst_calls), g(&c.n_bcfs_fst_reqs),
        g(&c.n_bcfs_walk_calls), g(&c.n_bcfs_walk_reqs),
        g(&c.n_bcfs_distinct_rem),
    ));
    s.push_str(&format!(
        "  chunk resolve: {} first-position postings, {} entries, {} pair iterations\n",
        g(&c.n_chain_first), g(&c.n_chain_entries), g(&c.n_chain_pairs),
    ));
    s.push_str(&format!(
        "  posmap resolve: {} lookups, {} survivors, {} mismatches | {} write collisions\n",
        g(&c.n_posmap_lookups), g(&c.n_posmap_survivors),
        g(&c.n_posmap_mismatch), g(&c.n_posmap_collisions),
    ));
    s.push_str(&format!(
        "  wordmap resolve: {} lookups, {} survivors, {} mismatches | {} write collisions\n",
        g(&c.n_wordmap_lookups), g(&c.n_wordmap_survivors),
        g(&c.n_wordmap_mismatch), g(&c.n_wordmap_collisions),
    ));
    s.push_str(&format!(
        "  chains: {} raw -> {} distinct | {} matches emitted | shared-head {} chains in {} groups, {} dispatch inserts\n",
        g(&c.n_chains_raw), g(&c.n_chains_distinct), g(&c.n_matches_emitted),
        g(&c.n_chains_shared), g(&c.n_groups_shared), g(&c.n_dispatch_inserts),
    ));
    if g(&c.n_memo_lookups) > 0 || g(&c.n_cut_items) > 0 {
        s.push_str(&format!(
            "  dictionary: memo lookups {} | cut {} items -> {} kept in {:.1}ms | sibling DFS {} steps, {} visited, lookups {:.1}ms, texts {:.1}ms | anchored fst {:.1}ms resolve {:.1}ms back {:.1}ms\n",
            g(&c.n_memo_lookups), g(&c.n_cut_items), g(&c.n_cut_kept), ms(&c.ns_cut),
            g(&c.n_sib_steps), g(&c.n_sib_visited), ms(&c.ns_sib_lookup), ms(&c.ns_sib_text),
            ms(&c.ns_anch_fst), ms(&c.ns_anch_resolve), ms(&c.ns_anch_back),
        ));
    }
    s
}
