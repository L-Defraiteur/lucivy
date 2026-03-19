//! Diagnostic: compare contains search hits vs ground truth on preserved index.

use lucivy_core::sharded_handle::ShardedHandle;
use lucivy_core::query::QueryConfig;

const BENCH_DIR: &str = "/home/luciedefraiteur/lucivy_bench_sharding/token_aware";

#[test]
fn diagnose_search_vs_ground_truth() {
    let handle = match ShardedHandle::open(BENCH_DIR) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Cannot open index: {} — run bench first", e);
            return;
        }
    };

    // Subscribe to DAG events for observability
    let dag_events = luciole::subscribe_dag_events();

    let terms = ["mutex", "lock", "function", "printk", "sched"];

    for term in &terms {
        eprintln!("\n=== {:?} ===", term);

        // Contains search: uses sfx + ngrams
        let config = QueryConfig {
            query_type: "contains".to_string(),
            field: Some("content".to_string()),
            value: Some(term.to_string()),
            ..Default::default()
        };
        let results = match handle.search(&config, 100000, None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  search error: {}", e);
                continue;
            }
        };
        let search_hits = results.len();

        // Ground truth: use the diagnostics module
        let reports = lucivy_core::diagnostics::inspect_term_sharded_verified(
            &handle, "content", term,
        );
        let gt: u32 = reports.iter().map(|(_, r)| r.ground_truth_count.unwrap_or(0)).sum();
        let df: u32 = reports.iter().map(|(_, r)| r.total_doc_freq).sum();

        eprintln!("  contains search: {} hits", search_hits);
        eprintln!("  term dict df:    {}", df);
        eprintln!("  ground truth:    {} (substring in stored docs)", gt);

        if search_hits as u32 != gt {
            eprintln!("  MISMATCH: search={} vs ground_truth={} (diff={})",
                search_hits, gt, gt as i64 - search_hits as i64);
        }

        // Collect DAG events from this search
        let mut events = Vec::new();
        while let Some(evt) = dag_events.try_recv() {
            events.push(evt);
        }
        if !events.is_empty() {
            let completions: Vec<_> = events.iter()
                .filter_map(|e| match e {
                    luciole::DagEvent::NodeCompleted { node, duration_ms, metrics, .. } =>
                        Some(format!("  {} {}ms {:?}", node, duration_ms, metrics)),
                    _ => None,
                })
                .collect();
            for c in &completions {
                eprintln!("  DAG: {}", c);
            }
        }
    }

    handle.close().ok();
}
