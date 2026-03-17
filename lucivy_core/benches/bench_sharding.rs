//! Benchmark: token-aware sharding vs round-robin on rag3db clone.
//!
//! Uses the same file collection as build_dataset.py (same excludes).
//! Default: all files (~5K). Set MAX_DOCS=N to limit.
//!
//! Run with:
//!   cargo test -p lucivy-core --test bench_sharding -- --nocapture
//!   MAX_DOCS=500 cargo test -p lucivy-core --test bench_sharding -- --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig};
use lucivy_core::sharded_handle::ShardedHandle;
use ld_lucivy::query::HighlightSink;

const RAG3DB_CLONE: &str = "/tmp/rag3db_bench";
const BENCH_BASE: &str = "/tmp/lucivy_bench_sharding";
const MAX_FILE_SIZE: u64 = 100_000;

// ─── File collection (same as build_dataset.py) ────────────────────────────

fn is_text(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else { return false };
    let chunk = &data[..data.len().min(8192)];
    if chunk.contains(&0u8) { return false; }
    std::str::from_utf8(chunk).is_ok()
}

fn collect_files(root: &Path, max_docs: usize) -> Vec<(String, String)> {
    let exclude_dirs = [
        "target", "node_modules", ".git", "build", "build_wasm", "pkg",
        "__pycache__", ".venv", ".pytest_cache", "playground",
    ];
    let exclude_files = ["package-lock.json", ".env", ".gitignore"];
    let mut files = Vec::new();

    fn walk(
        dir: &Path, root: &Path,
        exclude_dirs: &[&str], exclude_files: &[&str],
        files: &mut Vec<(String, String)>,
        max_docs: usize,
    ) {
        if files.len() >= max_docs { return; }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if files.len() >= max_docs { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !exclude_dirs.contains(&name.as_str()) {
                    walk(&path, root, exclude_dirs, exclude_files, files, max_docs);
                }
            } else if path.is_file() {
                if exclude_files.contains(&name.as_str()) { continue; }
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE_SIZE || size == 0 { continue; }
                if !is_text(&path) { continue; }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.trim().is_empty() {
                        files.push((rel, content));
                    }
                }
            }
        }
    }

    walk(root, root, &exclude_dirs, &exclude_files, &mut files, max_docs);
    files
}

// ─── Index creation ────────────────────────────────────────────────────────

fn make_config(shards: usize, balance_weight: f64) -> query::SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "shards": shards,
        "balance_weight": balance_weight
    })).unwrap()
}

fn index_single(files: &[(String, String)], dir: &str) -> (LucivyHandle, f64) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

    let config: query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ]
    })).unwrap();

    let d = lucivy_core::directory::StdFsDirectory::open(dir).unwrap();
    let handle = LucivyHandle::create(d, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    let commit_every = 5000;
    let total = files.len();
    let t0 = Instant::now();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            w.add_document(doc).unwrap();
            if (i + 1) % commit_every == 0 {
                w.commit().unwrap();
                eprintln!("    committed {}/{} ({:.1}s)", i + 1, total, t0.elapsed().as_secs_f64());
            }
        }
        w.commit().unwrap();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    handle.reader.reload().unwrap();
    (handle, elapsed)
}

fn index_sharded(files: &[(String, String)], dir: &str, num_shards: usize, balance_weight: f64) -> (ShardedHandle, f64) {
    let _ = std::fs::remove_dir_all(dir);
    let config = make_config(num_shards, balance_weight);
    let handle = ShardedHandle::create(dir, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    let commit_every = 5000;
    let total = files.len();
    let t0 = Instant::now();
    for (i, (path, content)) in files.iter().enumerate() {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, i as u64);
        doc.add_text(path_f, path);
        doc.add_text(content_f, content);
        handle.add_document(doc, i as u64).unwrap();
        if (i + 1) % commit_every == 0 {
            handle.commit().unwrap();
            eprintln!("    committed {}/{} ({:.1}s)", i + 1, total, t0.elapsed().as_secs_f64());
        }
    }
    handle.commit().unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    (handle, elapsed)
}

// ─── Query timing ──────────────────────────────────────────────────────────

fn time_single_query(handle: &LucivyHandle, config: &QueryConfig) -> (usize, f64) {
    let t0 = Instant::now();
    let query = query::build_query(config, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(20).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

fn time_sharded_query(handle: &ShardedHandle, config: &QueryConfig) -> (usize, f64) {
    let t0 = Instant::now();
    let results = handle.search(config, 20, None).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), ms)
}

// ─── Main bench ────────────────────────────────────────────────────────────

#[test]
fn bench_sharding_comparison() {
    let max_docs: usize = std::env::var("MAX_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    eprintln!("\n=== Collecting files from {} (max {}) ===", RAG3DB_CLONE, max_docs);
    let files = collect_files(Path::new(RAG3DB_CLONE), max_docs);
    let ndocs = files.len();
    eprintln!("Collected {} text files\n", ndocs);

    if ndocs == 0 {
        eprintln!("No files found at {}. Clone rag3db there first.", RAG3DB_CLONE);
        return;
    }

    let num_shards = 4;

    // ── Index ───────────────────────────────────────────────────────────

    eprintln!("=== Indexing: 1 shard (baseline) ===");
    let (single, single_time) = index_single(&files, &format!("{BENCH_BASE}/single"));
    eprintln!("  {} docs in {:.2}s\n", single.reader.searcher().num_docs(), single_time);

    eprintln!("=== Indexing: {} shards token-aware (balance_weight=0.2) ===", num_shards);
    let (sharded_ta, ta_time) = index_sharded(&files, &format!("{BENCH_BASE}/token_aware"), num_shards, 0.2);
    let (ta_counts, _) = sharded_ta.router_stats().unwrap();
    eprintln!("  {} docs in {:.2}s", sharded_ta.num_docs(), ta_time);
    eprintln!("  distribution: {:?}\n", ta_counts);

    eprintln!("=== Indexing: {} shards round-robin (balance_weight=1.0) ===", num_shards);
    let (sharded_rr, rr_time) = index_sharded(&files, &format!("{BENCH_BASE}/round_robin"), num_shards, 1.0);
    let (rr_counts, _) = sharded_rr.router_stats().unwrap();
    eprintln!("  {} docs in {:.2}s", sharded_rr.num_docs(), rr_time);
    eprintln!("  distribution: {:?}\n", rr_counts);

    // ── Queries ─────────────────────────────────────────────────────────

    let queries: Vec<(&str, QueryConfig)> = vec![
        ("contains 'function'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("function".into()),
            ..Default::default()
        }),
        ("contains_split 'create index'", QueryConfig {
            query_type: "contains_split".into(),
            field: Some("content".into()),
            value: Some("create index".into()),
            ..Default::default()
        }),
        // ── Same terms: contains vs startsWith head-to-head ──
        ("contains 'segment'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("segment".into()),
            ..Default::default()
        }),
        ("startsWith 'segment'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("segment".into()),
            ..Default::default()
        }),
        ("contains 'rag3db'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("rag3db".into()),
            ..Default::default()
        }),
        ("startsWith 'rag3db'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("rag3db".into()),
            ..Default::default()
        }),
        ("contains 'kuzu'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("kuzu".into()),
            ..Default::default()
        }),
        ("startsWith 'kuzu'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("kuzu".into()),
            ..Default::default()
        }),
        // ── Other queries ──
        ("contains 'cmake' (path)", QueryConfig {
            query_type: "contains".into(),
            field: Some("path".into()),
            value: Some("cmake".into()),
            ..Default::default()
        }),
    ];

    eprintln!("{:<35} {:>6} {:>10} {:>10} {:>10}", "Query", "Hits", "1-shard", "TA-4sh", "RR-4sh");
    eprintln!("{}", "-".repeat(75));

    for (label, config) in &queries {
        // Warm up
        let _ = time_single_query(&single, config);
        let _ = time_sharded_query(&sharded_ta, config);
        let _ = time_sharded_query(&sharded_rr, config);

        // 3-run average
        let mut single_ms = 0.0;
        let mut ta_ms = 0.0;
        let mut rr_ms = 0.0;
        let mut hits = 0;
        for _ in 0..3 {
            let (h, ms) = time_single_query(&single, config);
            single_ms += ms;
            hits = h;
            let (_, ms) = time_sharded_query(&sharded_ta, config);
            ta_ms += ms;
            let (_, ms) = time_sharded_query(&sharded_rr, config);
            rr_ms += ms;
        }
        eprintln!("{:<35} {:>6} {:>8.1}ms {:>8.1}ms {:>8.1}ms",
            label, hits, single_ms / 3.0, ta_ms / 3.0, rr_ms / 3.0);
    }

    // ── Summary ─────────────────────────────────────────────────────────

    eprintln!("\n=== Summary ===");
    eprintln!("Index time:  1-shard {:.2}s  |  TA-{num_shards}sh {:.2}s  |  RR-{num_shards}sh {:.2}s",
        single_time, ta_time, rr_time);
    eprintln!("TA distribution: {:?}", ta_counts);
    eprintln!("RR distribution: {:?}", rr_counts);

    // Balance metric: stddev of doc counts / mean
    let ta_mean = ta_counts.iter().sum::<u64>() as f64 / ta_counts.len() as f64;
    let ta_stddev = (ta_counts.iter().map(|&c| (c as f64 - ta_mean).powi(2)).sum::<f64>() / ta_counts.len() as f64).sqrt();
    let rr_mean = rr_counts.iter().sum::<u64>() as f64 / rr_counts.len() as f64;
    let rr_stddev = (rr_counts.iter().map(|&c| (c as f64 - rr_mean).powi(2)).sum::<f64>() / rr_counts.len() as f64).sqrt();
    eprintln!("Balance CV:  TA {:.3}  |  RR {:.3}  (lower = more balanced)", ta_stddev / ta_mean, rr_stddev / rr_mean);

    // Cleanup
    let _ = std::fs::remove_dir_all(BENCH_BASE);
}
