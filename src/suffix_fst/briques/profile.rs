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
    pub ns_single: AtomicU64,
    pub ns_chunk_walk: AtomicU64,
    pub ns_chunk_sibling: AtomicU64,
    pub ns_chunk_resolve: AtomicU64,
    pub ns_word_walk: AtomicU64,
    pub ns_word_sibling: AtomicU64,
    pub ns_word_resolve: AtomicU64,

    /// Chains handed to each resolve stage.
    pub n_chunk_chains: AtomicU64,
    pub n_word_chains: AtomicU64,
    /// Postings materialised per chain position in the word resolve.
    pub n_word_entries: AtomicU64,
    /// Iterations of the `active x entries` nested scan.
    pub n_word_pairs: AtomicU64,
    /// Calls to `intermediates_are_pure_sep`, and positions it scanned.
    pub n_puresep_calls: AtomicU64,
    pub n_puresep_positions: AtomicU64,

    /// Splits fed to `build_chains_from_splits`, and the FST work they trigger.
    /// Every remainder is a suffix of the query, so `distinct` is bounded by the
    /// query length however many splits there are.
    pub n_bcfs_splits: AtomicU64,
    /// `_reqs` = times a remainder was needed; `_calls` = times it was actually
    /// computed. The gap is what the memo saves.
    pub n_bcfs_fst_reqs: AtomicU64,
    pub n_bcfs_fst_calls: AtomicU64,
    pub n_bcfs_walk_reqs: AtomicU64,
    pub n_bcfs_walk_calls: AtomicU64,
    pub n_bcfs_distinct_rem: AtomicU64,

    /// Postings materialised by the chunk chain resolve, and the pair iterations
    /// they feed.
    pub n_chain_first: AtomicU64,
    pub n_chain_entries: AtomicU64,
    pub n_chain_pairs: AtomicU64,

    /// posmap-driven chain resolution: lookups made, survivors whose posting was
    /// then fetched, and survivors whose posting did NOT hold the position posmap
    /// claimed (must stay 0 — anything else means posmap and sfxpost disagree).
    pub n_posmap_lookups: AtomicU64,
    pub n_posmap_survivors: AtomicU64,
    pub n_posmap_mismatch: AtomicU64,
    /// (doc, position) written twice with different ordinals at index time.
    pub n_posmap_collisions: AtomicU64,

    /// Chunk chains before and after structural dedup, and matches emitted.
    pub n_chains_raw: AtomicU64,
    pub n_chains_distinct: AtomicU64,
    pub n_matches_emitted: AtomicU64,
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
        &c.ns_single, &c.ns_chunk_walk, &c.ns_chunk_sibling, &c.ns_chunk_resolve,
        &c.ns_word_walk, &c.ns_word_sibling, &c.ns_word_resolve,
        &c.n_chunk_chains, &c.n_word_chains, &c.n_word_entries, &c.n_word_pairs,
        &c.n_puresep_calls, &c.n_puresep_positions,
        &c.n_bcfs_splits, &c.n_bcfs_fst_reqs, &c.n_bcfs_fst_calls,
        &c.n_bcfs_walk_reqs, &c.n_bcfs_walk_calls, &c.n_bcfs_distinct_rem,
        &c.n_chain_first, &c.n_chain_entries, &c.n_chain_pairs,
        &c.n_posmap_lookups, &c.n_posmap_survivors, &c.n_posmap_mismatch,
        &c.n_posmap_collisions,
        &c.n_chains_raw, &c.n_chains_distinct, &c.n_matches_emitted,
    ] {
        a.store(0, Ordering::Relaxed);
    }
}

/// Human-readable report of everything accumulated since [`reset`].
pub fn dump() -> String {
    let c = counters();
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let ms = |a: &AtomicU64| g(a) as f64 / 1e6;

    let total = ms(&c.ns_single) + ms(&c.ns_chunk_walk) + ms(&c.ns_chunk_sibling)
        + ms(&c.ns_chunk_resolve) + ms(&c.ns_word_walk) + ms(&c.ns_word_sibling)
        + ms(&c.ns_word_resolve);
    let pct = |v: f64| if total > 0.0 { v / total * 100.0 } else { 0.0 };

    let mut s = String::new();
    s.push_str(&format!("  stage totals (sum over segments), {total:.1}ms accounted\n"));
    for (name, v) in [
        ("single (candidates+resolve)", ms(&c.ns_single)),
        ("chunk walk", ms(&c.ns_chunk_walk)),
        ("chunk sibling DFS", ms(&c.ns_chunk_sibling)),
        ("chunk resolve", ms(&c.ns_chunk_resolve)),
        ("word walk", ms(&c.ns_word_walk)),
        ("word sibling DFS", ms(&c.ns_word_sibling)),
        ("word resolve", ms(&c.ns_word_resolve)),
    ] {
        s.push_str(&format!("    {name:<28} {v:>9.1}ms  {:>5.1}%\n", pct(v)));
    }
    s.push_str(&format!(
        "  chains: chunk={} word={}  word_entries={}  word_pairs={}\n",
        g(&c.n_chunk_chains), g(&c.n_word_chains),
        g(&c.n_word_entries), g(&c.n_word_pairs),
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
        "  chains: {} raw -> {} distinct | {} matches emitted\n",
        g(&c.n_chains_raw), g(&c.n_chains_distinct), g(&c.n_matches_emitted),
    ));
    s
}
