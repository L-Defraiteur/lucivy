//! Bound on the number of merges running at the same time.
//!
//! A v3 merge holds every source sidecar plus the merged token tables and
//! the FST builder's key table in memory — around 500 MB for 14 segments of
//! 40 kernel files. Native machines run the four shards' merges in parallel
//! without noticing; a 4 GB wasm32 address space does not (the first commit
//! of the browser playground died on a 192 MB `realloc` in the FST build
//! with all four running). The limit comes from `LUCIVY_MERGE_CONCURRENCY`,
//! defaults to 1 on wasm32 and to unlimited elsewhere.
//!
//! Waiting for a permit on a scheduler thread keeps that thread useful: it
//! runs other ready work (the permit holder's own DAG nodes among it)
//! instead of blocking, which is what a plain semaphore would do — with four
//! scheduler threads and four waiters, a blocking wait is a deadlock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

fn limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("LUCIVY_MERGE_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(if cfg!(target_arch = "wasm32") { 1 } else { usize::MAX })
    })
}

/// Held for the duration of one merge; releases its slot on drop.
pub struct MergePermit;

impl Drop for MergePermit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Take a merge slot, running other scheduler work while none is free.
pub fn acquire() -> MergePermit {
    let limit = limit();
    let mut idle_rounds = 0u32;
    loop {
        let current = ACTIVE.load(Ordering::Acquire);
        if current < limit
            && ACTIVE
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return MergePermit;
        }
        let worked = crate::actor::scheduler::is_scheduler_thread()
            && crate::actor::scheduler::global_scheduler().run_one_step();
        if worked {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
            std::thread::sleep(std::time::Duration::from_millis(idle_rounds.min(20) as u64));
        }
    }
}

/// Merges currently holding a permit (diagnostics).
pub fn active() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permit_counts_and_releases() {
        let before = active();
        {
            let _a = acquire();
            assert_eq!(active(), before + 1);
            let _b = acquire();
            assert_eq!(active(), before + 2);
        }
        assert_eq!(active(), before);
    }
}

// ─── Segment builds ───────────────────────────────────────────────────────

static BUILDS_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// How many segment builds (FST construction and sidecar writing) may run at
/// once: `LUCIVY_MAX_PENDING_FINALIZE`, 4 on wasm32 and unlimited elsewhere.
///
/// A build's peak scales with its segment's tokens — 170-400 MB for ~250
/// kernel files — and a commit flushes every shard's current segment at
/// once. With four scheduler threads that was at most four builds; with
/// eight (mimalloc made them worthwhile) the first commit of the playground
/// ran eight and died on a 169 MB allocation. The same cooperative wait as
/// the merges: a build waiting for a slot keeps its thread running other
/// ready work, so it can never deadlock the actors it depends on.
fn build_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("LUCIVY_MAX_PENDING_FINALIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(if cfg!(target_arch = "wasm32") { 4 } else { usize::MAX })
    })
}

/// Held for the duration of one segment build; releases its slot on drop.
pub struct BuildPermit;

impl Drop for BuildPermit {
    fn drop(&mut self) {
        BUILDS_ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Take a build slot, running other scheduler work while none is free.
pub fn acquire_build() -> BuildPermit {
    let limit = build_limit();
    let mut idle_rounds = 0u32;
    loop {
        let current = BUILDS_ACTIVE.load(Ordering::Acquire);
        if current < limit
            && BUILDS_ACTIVE
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return BuildPermit;
        }
        let worked = crate::actor::scheduler::is_scheduler_thread()
            && crate::actor::scheduler::global_scheduler().run_one_step();
        if worked {
            idle_rounds = 0;
        } else {
            idle_rounds += 1;
            std::thread::sleep(std::time::Duration::from_millis(idle_rounds.min(20) as u64));
        }
    }
}

/// Segment builds currently holding a permit (diagnostics).
pub fn active_builds() -> usize {
    BUILDS_ACTIVE.load(Ordering::Acquire)
}
