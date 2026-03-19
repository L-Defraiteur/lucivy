//! Standalone diagnostics on a preserved bench index.
//!
//! Runs ground truth verification and SFX diagnostics on an existing index
//! without re-indexing. Point BENCH_INDEX to the index directory.
//!
//! Usage:
//!   cargo test -p lucivy-core --test test_diagnostics --release -- --nocapture
//!   BENCH_INDEX=/path/to/index cargo test -p lucivy-core --test test_diagnostics --release -- --nocapture

use lucivy_core::handle::LucivyHandle;
use lucivy_core::diagnostics;
use lucivy_core::directory::StdFsDirectory;
use ld_lucivy::Index;

const DEFAULT_INDEX: &str = "/home/luciedefraiteur/lucivy_bench_sharding";

#[test]
fn verify_index() {
    let base = std::env::var("BENCH_INDEX")
        .unwrap_or_else(|_| DEFAULT_INDEX.to_string());

    let shards_dir = format!("{}/round_robin", base);
    if !std::path::Path::new(&shards_dir).exists() {
        eprintln!("No index found at {}. Run the bench first.", shards_dir);
        return;
    }

    // Find all shard directories
    let mut shard_dirs: Vec<String> = Vec::new();
    for i in 0..16 {
        let dir = format!("{}/shard_{}", shards_dir, i);
        if std::path::Path::new(&dir).exists() {
            shard_dirs.push(dir);
        } else {
            break;
        }
    }

    eprintln!("=== Index verification: {} shards at {} ===\n", shard_dirs.len(), base);

    let terms_to_check = vec!["mutex", "lock", "function", "printk", "sched", "struct"];

    for (shard_id, dir) in shard_dirs.iter().enumerate() {
        let directory = match StdFsDirectory::open(dir) {
            Ok(d) => d,
            Err(e) => { eprintln!("shard_{}: can't open: {}", shard_id, e); continue; }
        };
        let handle = match LucivyHandle::open(directory) {
            Ok(h) => h,
            Err(e) => { eprintln!("shard_{}: can't open handle: {}", shard_id, e); continue; }
        };

        eprintln!("--- shard_{} ({} docs, {} segments) ---",
            shard_id,
            handle.reader.searcher().num_docs(),
            handle.reader.searcher().segment_readers().len());

        // Term dict + ground truth
        for term in &terms_to_check {
            let report = diagnostics::inspect_term_verified(&handle, "content", term);
            let gt = report.ground_truth_count.unwrap_or(0);
            let df = report.total_doc_freq;
            let status = if gt == df { "MATCH" } else { "MISMATCH" };
            eprintln!("  {:12} doc_freq={:5}  ground_truth={:5}  ({})", term, df, gt, status);
        }

        // SFX diagnostic
        for term in &["mutex", "lock"] {
            let sfx = diagnostics::inspect_sfx(&handle, "content", term);
            eprintln!("  SFX {:12} → {} docs", term, sfx.total_sfx_docs);
        }

        eprintln!();
    }
}
