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

const RAG3DB_CLONE: &str = "/tmp/rag3db_bench";
const LINUX_CLONE: &str = "/home/luciedefraiteur/linux_bench";
const BENCH_BASE: &str = "/home/luciedefraiteur/lucivy_bench_sharding";
const MAX_FILE_SIZE: u64 = 100_000;

// ─── File collection (same as build_dataset.py) ────────────────────────────

fn is_text(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else { return false };
    let chunk = &data[..data.len().min(8192)];
    if chunk.contains(&0u8) { return false; }
    std::str::from_utf8(chunk).is_ok()
}

fn collect_files(root: &Path, max_docs: usize) -> Vec<(String, String)> {
    use std::collections::HashSet;

    let exclude_dirs = [
        "target", "node_modules", ".git", "build", "build_wasm", "pkg",
        "__pycache__", ".venv", ".pytest_cache", "playground",
    ];
    let exclude_files = ["package-lock.json", ".env", ".gitignore"];
    let mut files = Vec::new();
    let mut visited = HashSet::new();

    fn walk(
        dir: &Path, root: &Path,
        exclude_dirs: &[&str], exclude_files: &[&str],
        files: &mut Vec<(String, String)>,
        visited: &mut HashSet<std::path::PathBuf>,
        max_docs: usize,
    ) {
        if files.len() >= max_docs { return; }
        // Resolve symlinks and skip already-visited dirs
        let canonical = match dir.canonicalize() {
            Ok(p) => p,
            Err(_) => return,
        };
        if !visited.insert(canonical) { return; }

        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if files.len() >= max_docs { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() || path.is_symlink() && path.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                if !exclude_dirs.contains(&name.as_str()) {
                    walk(&path, root, exclude_dirs, exclude_files, files, visited, max_docs);
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

    walk(root, root, &exclude_dirs, &exclude_files, &mut files, &mut visited, max_docs);
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
    // Final commit: rebuild all deferred FSTs
    let t_final = std::time::Instant::now();
    handle.commit().unwrap();
    eprintln!("    final commit ({:.1}s)", t_final.elapsed().as_secs_f64());
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

fn time_sharded_query_traced(handle: &ShardedHandle, config: &QueryConfig, label: &str) -> (usize, f64) {
    let rx = luciole::subscribe_dag_events();
    let t0 = Instant::now();
    let results = handle.search(config, 20, None).unwrap();
    let ms = t0.elapsed().as_secs_f64() * 1000.0;

    eprintln!("\n  [TRACE] {} — total {:.1}ms", label, ms);
    while let Some(event) = rx.try_recv() {
        match event {
            luciole::DagEvent::NodeCompleted { node, duration_ms, metrics, .. } => {
                let metrics_str = metrics.iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>().join(" ");
                eprintln!("    {:20} {:>8.1}ms  {}", node, duration_ms, metrics_str);
            }
            luciole::DagEvent::LevelCompleted { level, duration_ms, .. } => {
                eprintln!("    --- level {} --- {:>8.1}ms", level, duration_ms);
            }
            luciole::DagEvent::DagCompleted { total_ms, .. } => {
                eprintln!("    === DAG total === {:>6.1}ms", total_ms);
            }
            _ => {}
        }
    }
    (results.len(), ms)
}

// ─── Main bench ────────────────────────────────────────────────────────────

#[test]
fn bench_sharding_comparison() {
    let max_docs: usize = std::env::var("MAX_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    let dataset = std::env::var("BENCH_DATASET").unwrap_or_else(|_| LINUX_CLONE.to_string());
    eprintln!("\n=== Collecting files from {} (max {}) ===", dataset, max_docs);
    let files = collect_files(Path::new(&dataset), max_docs);
    let ndocs = files.len();
    eprintln!("Collected {} text files\n", ndocs);

    if ndocs == 0 {
        eprintln!("No files found at {}. Clone rag3db there first.", RAG3DB_CLONE);
        return;
    }

    let num_shards = 4;

    // ── Index ───────────────────────────────────────────────────────────

    // BENCH_MODE env: "SINGLE", "TA", "RR", or combine with "|" e.g. "SINGLE|TA"
    // Default: all three.
    let bench_mode = std::env::var("BENCH_MODE").unwrap_or_else(|_| "SINGLE|TA|RR".into());
    let do_single = bench_mode.contains("SINGLE");
    let do_ta = bench_mode.contains("TA");
    let do_rr = bench_mode.contains("RR");

    let (single, single_time) = if do_single {
        eprintln!("=== Indexing: 1 shard (baseline) ===");
        let (s, t) = index_single(&files, &format!("{BENCH_BASE}/single"));
        eprintln!("  {} docs in {:.2}s\n", s.reader.searcher().num_docs(), t);
        (Some(s), t)
    } else {
        eprintln!("=== Skipping 1 shard ===\n");
        (None, 0.0)
    };

    let (sharded_ta, ta_time, ta_counts) = if do_ta {
        eprintln!("=== Indexing: {} shards token-aware (balance_weight=0.2) ===", num_shards);
        let (h, t) = index_sharded(&files, &format!("{BENCH_BASE}/token_aware"), num_shards, 0.2);
        let (counts, _) = h.router_stats().unwrap();
        eprintln!("  {} docs in {:.2}s", h.num_docs(), t);
        eprintln!("  distribution: {:?}\n", counts);
        (Some(h), t, counts)
    } else {
        eprintln!("=== Skipping TA ===\n");
        (None, 0.0, vec![])
    };

    let (sharded_rr, rr_time, rr_counts) = if do_rr {
        eprintln!("=== Indexing: {} shards round-robin (balance_weight=1.0) ===", num_shards);
        let (h, t) = index_sharded(&files, &format!("{BENCH_BASE}/round_robin"), num_shards, 1.0);
        let (counts, _) = h.router_stats().unwrap();
        eprintln!("  {} docs in {:.2}s", h.num_docs(), t);
        eprintln!("  distribution: {:?}\n", counts);
        (Some(h), t, counts)
    } else {
        eprintln!("=== Skipping RR ===\n");
        (None, 0.0, vec![])
    };

    // ── Queries ─────────────────────────────────────────────────────────

    let queries: Vec<(&str, QueryConfig)> = vec![
        ("contains 'mutex_lock' [1]", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("mutex_lock".into()),
            ..Default::default()
        }),
        ("contains 'function'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("function".into()),
            ..Default::default()
        }),
        ("contains 'mutex_lock' [2]", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("mutex_lock".into()),
            ..Default::default()
        }),
        ("contains 'function' [dup]", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("function".into()),
            ..Default::default()
        }),
        ("contains_split 'struct device'", QueryConfig {
            query_type: "contains_split".into(),
            field: Some("content".into()),
            value: Some("struct device".into()),
            ..Default::default()
        }),
        // ── Same terms: contains vs startsWith head-to-head ──
        ("contains 'sched'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("sched".into()),
            ..Default::default()
        }),
        ("startsWith 'sched'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("sched".into()),
            ..Default::default()
        }),
        ("contains 'printk'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("printk".into()),
            ..Default::default()
        }),
        ("startsWith 'printk'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("printk".into()),
            ..Default::default()
        }),
        // ── Fuzzy contains ──
        ("fuzzy 'schdule' (d=1)", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("schdule".into()),
            distance: Some(1),
            ..Default::default()
        }),
        // ── Path search ──
        ("contains 'drivers' (path)", QueryConfig {
            query_type: "contains".into(),
            field: Some("path".into()),
            value: Some("drivers".into()),
            ..Default::default()
        }),
    ];

    eprintln!("{:<35} {:>6} {:>10} {:>10} {:>10}", "Query", "Hits", "1-shard", "TA-4sh", "RR-4sh");
    eprintln!("{}", "-".repeat(75));

    // Traced run: one per query on RR to see per-node timing
    if do_rr {
        eprintln!("\n=== DAG node timing (one run per query) ===");
        for (label, config) in &queries {
            if let Some(ref s) = sharded_rr {
                let _ = time_sharded_query_traced(s, config, label);
            }
        }
        eprintln!();
    }

    for (label, config) in &queries {
        // Warm up
        if let Some(ref s) = single { let _ = time_single_query(s, config); }
        if let Some(ref s) = sharded_ta { let _ = time_sharded_query(s, config); }
        if let Some(ref s) = sharded_rr { let _ = time_sharded_query(s, config); }

        // 3-run average
        let mut single_ms = 0.0;
        let mut ta_ms = 0.0;
        let mut rr_ms = 0.0;
        let mut hits = 0;
        for _ in 0..3 {
            if let Some(ref s) = single {
                let (h, ms) = time_single_query(s, config);
                single_ms += ms;
                hits = h;
            }
            if let Some(ref s) = sharded_ta {
                let (h, ms) = time_sharded_query(s, config);
                ta_ms += ms;
                if hits == 0 { hits = h; }
            }
            if let Some(ref s) = sharded_rr {
                let (h, ms) = time_sharded_query(s, config);
                rr_ms += ms;
                if hits == 0 { hits = h; }
            }
        }
        eprintln!("{:<35} {:>6} {:>8.1}ms {:>8.1}ms {:>8.1}ms",
            label, hits, single_ms / 3.0, ta_ms / 3.0, rr_ms / 3.0);
    }

    let verify = std::env::var("LUCIVY_VERIFY").map(|v| v == "1").unwrap_or(false);

    // ── Sanity check: run ALL queries with highlights ─────────────────
    {
        let handle = sharded_rr.as_ref().or(sharded_ta.as_ref());
        let mode = if sharded_rr.is_some() { "RR" } else { "TA" };
        if let Some(handle) = handle {
            for (label, qconfig) in &queries {
                let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
                let results = handle.search(qconfig, 2, Some(Arc::clone(&sink))).unwrap();
                let _query_val = qconfig.value.as_deref().unwrap_or("?");
                eprintln!("\n--- {label} on {mode}: {} hits ---", results.len());
                for r in results.iter().take(2) {
                    let shard = handle.shard(r.shard_id).unwrap();
                    let searcher = shard.reader.searcher();
                    let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
                    let highlights = sink.get(seg_id, r.doc_address.doc_id);
                    let stored = searcher.doc::<ld_lucivy::LucivyDocument>(r.doc_address).ok();

                    // Extract first highlight snippet
                    let snippet = highlights.as_ref()
                        .and_then(|h| h.iter().next())
                        .and_then(|(field_name, offsets)| {
                            offsets.first().map(|[s, e]| {
                                stored.as_ref().and_then(|doc| {
                                    let field = handle.field(field_name)?;
                                    doc.field_values()
                                        .find(|(f, _)| *f == field)
                                        .and_then(|(_, v)| {
                                            use ld_lucivy::schema::document::Value;
                                            v.as_value().as_str().map(|text| {
                                                let clamp = |pos: usize| -> usize {
                                                    let mut p = pos.min(text.len());
                                                    while p > 0 && !text.is_char_boundary(p) { p -= 1; }
                                                    p
                                                };
                                                let cs = clamp(s.saturating_sub(20));
                                                let hs = clamp(*s);
                                                let he = clamp(*e);
                                                let ce = clamp((*e + 20).min(text.len()));
                                                format!("...{}«{}»{}...",
                                                    &text[cs..hs], &text[hs..he], &text[he..ce])
                                            })
                                        })
                                }).unwrap_or_else(|| format!("@{}..{}", s, e))
                            })
                        })
                        .unwrap_or_else(|| "(no highlight)".into());
                    eprintln!("  shard={} doc={} score={:.3} → {}",
                        r.shard_id, r.doc_address.doc_id, r.score, snippet);
                }
            }
        }
    }

    // ── Diagnostic: multi-token vs single-token hit count ─────────────
    if verify { if let Some(handle) = sharded_rr.as_ref().or(sharded_ta.as_ref()) {
        eprintln!("\n=== Query diagnostic ===");
        let diag_queries = vec![
            ("contains 'mutex' (single)", "contains", "mutex"),
            ("contains 'mutex_lock' (multi)", "contains", "mutex_lock"),
            ("contains 'lock' (single)", "contains", "lock"),
            ("contains_split 'mutex lock'", "contains_split", "mutex lock"),
        ];
        for (label, qtype, value) in &diag_queries {
            let config = QueryConfig {
                query_type: qtype.to_string(),
                field: Some("content".into()),
                value: Some(value.to_string()),
                ..Default::default()
            };
            let results = handle.search(&config, 1000, None).unwrap();
            eprintln!("  {}: {} hits", label, results.len());
        }

        // Post-mortem: inspect term in all shards
        eprintln!("\n=== Post-mortem: term inspection {} ===",
            if verify { "(with ground truth)" } else { "" });
        for term in &["mutex", "lock", "function", "printk"] {
            let reports = if verify {
                lucivy_core::diagnostics::inspect_term_sharded_verified(handle, "content", term)
            } else {
                lucivy_core::diagnostics::inspect_term_sharded(handle, "content", term)
            };
            let total: u32 = reports.iter().map(|(_, r)| r.total_doc_freq).sum();
            let gt: Option<u32> = if verify {
                Some(reports.iter().map(|(_, r)| r.ground_truth_count.unwrap_or(0)).sum())
            } else { None };
            let gt_str = gt.map(|g| {
                let status = if g == total { "MATCH" } else { "MISMATCH" };
                format!(" | ground_truth={} ({})", g, status)
            }).unwrap_or_default();
            eprintln!("\n  Term {:?} — total doc_freq: {}{}", term, total, gt_str);
            for (shard_id, report) in &reports {
                let seg_found: usize = report.segments.iter().filter(|s| s.term_found).count();
                let seg_total = report.segments.len();
                let shard_df = report.total_doc_freq;
                eprintln!("    shard_{}: doc_freq={} ({}/{} segments have term)",
                    shard_id, shard_df, seg_found, seg_total);
                // Show segments where term is NOT found (suspicious)
                for seg in &report.segments {
                    if !seg.term_found && seg.num_docs > 100 {
                        eprintln!("      MISSING in {} ({} docs, sfx={})",
                            &seg.segment_id[..8], seg.num_docs,
                            if seg.has_sfx { "ok" } else { "NONE" });
                    }
                }
            }
        }

        // Dump term dict vs FST keys for shard 0 (to debug format mismatches)
        if let Some(shard) = handle.shard(0) {
            let dump = lucivy_core::diagnostics::dump_segment_keys(shard, "content", 5);
            eprintln!("\n=== Key dump shard_0 ==={}", dump);
        }

        // Deep ordinal comparison for shard 0
        if let Some(shard) = handle.shard(0) {
            for term in &["mutex", "lock", "function"] {
                let cmp = lucivy_core::diagnostics::compare_postings_vs_sfxpost(shard, "content", term);
                eprintln!("{}", cmp);
            }
        }

        // SFX search vs ground truth (direct Count collector)
        eprintln!("\n=== Real search vs ground truth ===");
        for term in &["mutex", "lock", "function", "printk", "sched"] {
            let mut total_search = 0usize;
            for shard_idx in 0..4 {
                if let Some(shard) = handle.shard(shard_idx) {
                    let searcher = shard.reader.searcher();
                    let field = shard.field("content").unwrap();
                    let q = ld_lucivy::query::SuffixContainsQuery::new(
                        field, term.to_string(),
                    );
                    total_search += searcher.search(&q, &ld_lucivy::collector::Count).unwrap_or(0);
                }
            }

            // Compare with ground truth
            let gt_status = if verify {
                let gt_reports = lucivy_core::diagnostics::inspect_term_sharded_verified(handle, "content", term);
                let gt: u32 = gt_reports.iter().map(|(_, r)| r.ground_truth_count.unwrap_or(0)).sum();
                if total_search as u32 == gt { format!(" | ground_truth={gt} ✓ MATCH") }
                else { format!(" | ground_truth={gt} ✗ DIFF={}", (gt as i64 - total_search as i64).abs()) }
            } else { String::new() };
            eprintln!("\n  search {:?}: {} docs{}", term, total_search, gt_status);
        }

        // Missing docs trace removed — was using DiagBus which doesn't emit
        // SearchMatch events from the scorer path. If ground truth shows DIFF,
        // use lucivy_core::diagnostics::trace_search() directly.
    } } // if verify + if let Some(handle)

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
    // Keep the index for post-mortem inspection
    eprintln!("\n=== Index preserved at {} ===", BENCH_BASE);
}

// ═══════════════════════════════════════════════════════════════════════════
// Ground truth exhaustive: reuse persisted index, test all query variants
// ═══════════════════════════════════════════════════════════════════════════

/// Ground truth: count docs containing `needle` (case-insensitive) across all shards.
fn ground_truth_substring(handle: &ShardedHandle, field_name: &str, needle: &str) -> usize {
    let needle_lower = needle.to_lowercase();
    let mut count = 0usize;
    for shard_idx in 0..handle.num_shards() {
        let shard = handle.shard(shard_idx).unwrap();
        let searcher = shard.reader.searcher();
        let field = shard.field(field_name).unwrap();
        for sr in searcher.segment_readers() {
            if let Ok(store) = sr.get_store_reader(0) {
                for did in 0..sr.max_doc() {
                    if sr.alive_bitset().map_or(true, |bs| bs.is_alive(did)) {
                        if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(did) {
                            for (f, val) in doc.field_values() {
                                if f == field {
                                    use ld_lucivy::schema::document::Value;
                                    if let Some(text) = val.as_value().as_str() {
                                        if text.to_lowercase().contains(&needle_lower) {
                                            count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    count
}

/// Ground truth: count docs where any token STARTS with `prefix` (case-insensitive).
fn ground_truth_starts_with(handle: &ShardedHandle, field_name: &str, prefix: &str) -> usize {
    let prefix_lower = prefix.to_lowercase();
    let mut count = 0usize;
    for shard_idx in 0..handle.num_shards() {
        let shard = handle.shard(shard_idx).unwrap();
        let searcher = shard.reader.searcher();
        let field = shard.field(field_name).unwrap();
        for sr in searcher.segment_readers() {
            if let Ok(store) = sr.get_store_reader(0) {
                for did in 0..sr.max_doc() {
                    if sr.alive_bitset().map_or(true, |bs| bs.is_alive(did)) {
                        if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(did) {
                            for (f, val) in doc.field_values() {
                                if f == field {
                                    use ld_lucivy::schema::document::Value;
                                    if let Some(text) = val.as_value().as_str() {
                                        // Tokenize like SimpleTokenizer: split on non-alphanumeric
                                        let has_match = text.to_lowercase()
                                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                                            .any(|tok| tok.starts_with(&prefix_lower));
                                        if has_match {
                                            count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    count
}

/// Search count via ShardedHandle DAG (the real search path).
fn search_count(handle: &ShardedHandle, config: &QueryConfig) -> usize {
    handle.search(config, 100_000, None).unwrap().len()
}

/// Search with highlights + stored text snippets. Returns (count, snippet_lines).
fn search_with_snippets(
    handle: &ShardedHandle, config: &QueryConfig, top_k: usize, context_chars: usize,
) -> (usize, Vec<String>) {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let results = handle.search(config, top_k, Some(Arc::clone(&sink))).unwrap();
    let count = results.len();
    let field_name = config.field.as_deref().unwrap_or("content");

    let mut lines = Vec::new();
    for r in &results {
        let shard = handle.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg_reader = searcher.segment_reader(r.doc_address.segment_ord);
        let seg_id = seg_reader.segment_id();

        // Get stored text
        let stored_text = searcher.doc::<ld_lucivy::LucivyDocument>(r.doc_address).ok()
            .and_then(|doc| {
                let field = handle.field(field_name)?;
                doc.field_values()
                    .find(|(f, _)| *f == field)
                    .and_then(|(_, v)| {
                        use ld_lucivy::schema::document::Value;
                        v.as_value().as_str().map(|s| s.to_string())
                    })
            });

        let highlights = sink.get(seg_id, r.doc_address.doc_id);

        let snippet = if let (Some(text), Some(hl_map)) = (&stored_text, &highlights) {
            if let Some(offsets) = hl_map.get(field_name) {
                if let Some([s, e]) = offsets.first() {
                    // Clamp to char boundaries
                    let clamp = |pos: usize| -> usize {
                        let mut p = pos.min(text.len());
                        while p > 0 && !text.is_char_boundary(p) { p -= 1; }
                        p
                    };
                    let cs = clamp(s.saturating_sub(context_chars));
                    let hs = clamp(*s);
                    let he = clamp(*e);
                    let ce = clamp((*e + context_chars).min(text.len()));
                    let before = text[cs..hs].replace('\n', " ");
                    let matched = text[hs..he].replace('\n', " ");
                    let after = text[he..ce].replace('\n', " ");
                    format!("...{}«{}»{}...", before.trim(), matched, after.trim())
                } else {
                    "(no offsets)".into()
                }
            } else {
                "(no field highlights)".into()
            }
        } else {
            "(no stored text)".into()
        };

        lines.push(format!("  [{:>2}] score={:.4}  {}", r.shard_id, r.score, snippet));
    }
    (count, lines)
}

/// Search count via direct SuffixContainsQuery on each shard (bypasses DAG).
fn search_count_direct(handle: &ShardedHandle, field_name: &str, term: &str) -> usize {
    let mut total = 0;
    for shard_idx in 0..handle.num_shards() {
        let shard = handle.shard(shard_idx).unwrap();
        let searcher = shard.reader.searcher();
        let field = shard.field(field_name).unwrap();
        let q = ld_lucivy::query::SuffixContainsQuery::new(field, term.to_string());
        total += searcher.search(&q, &ld_lucivy::collector::Count).unwrap_or(0);
    }
    total
}

#[test]
fn ground_truth_exhaustive() {
    let index_dir = format!("{}/round_robin", BENCH_BASE);
    if !std::path::Path::new(&index_dir).exists() {
        eprintln!("Skipping: no persisted index at {}", index_dir);
        eprintln!("Run the main bench first: MAX_DOCS=90000 cargo test ...");
        return;
    }

    let handle = ShardedHandle::open(&index_dir).unwrap();
    let num_docs = handle.num_docs();
    eprintln!("\n=== Ground truth exhaustive on {} docs ===\n", num_docs);

    let terms = &["mutex", "lock", "function", "printk", "sched", "device", "error"];
    let mut pass = 0u32;
    let mut fail = 0u32;

    let show_top = 3; // snippets to display per query variant

    for term in terms {
        eprintln!("--- term: {:?} ---", term);

        // 1. contains (substring match)
        let gt_contains = ground_truth_substring(&handle, "content", term);
        let contains_cfg = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(term.to_string()),
            ..Default::default()
        };
        let search_contains = search_count(&handle, &contains_cfg);
        let (_, snippets) = search_with_snippets(&handle, &contains_cfg, 20, 60);
        let direct_contains = search_count_direct(&handle, "content", term);
        let status_dag = if search_contains == gt_contains { pass += 1; "MATCH" } else { fail += 1; "FAIL" };
        let status_direct = if direct_contains == gt_contains { pass += 1; "MATCH" } else { fail += 1; "FAIL" };
        eprintln!("  contains DAG:    {:>6} vs gt {:>6}  {}", search_contains, gt_contains, status_dag);
        eprintln!("  contains direct: {:>6} vs gt {:>6}  {}", direct_contains, gt_contains, status_direct);
        for s in snippets.iter().take(show_top) { eprintln!("{}", s); }

        // 2. startsWith (token prefix match)
        let starts_cfg = QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some(term.to_string()),
            ..Default::default()
        };
        let search_starts = search_count(&handle, &starts_cfg);
        let (_, snippets) = search_with_snippets(&handle, &starts_cfg, 20, 60);
        let gt_starts = ground_truth_starts_with(&handle, "content", term);
        let status = if search_starts >= gt_starts { pass += 1; "OK (≥ gt)" } else { fail += 1; "FAIL (< gt!)" };
        eprintln!("  startsWith DAG:  {:>6} vs gt {:>6}  {}", search_starts, gt_starts, status);
        for s in snippets.iter().take(show_top) { eprintln!("{}", s); }

        // 3. fuzzy d=1 contains
        let fuzzy1_cfg = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(term.to_string()),
            distance: Some(1),
            ..Default::default()
        };
        let search_fuzzy1 = search_count(&handle, &fuzzy1_cfg);
        let (_, snippets) = search_with_snippets(&handle, &fuzzy1_cfg, 20, 60);
        let status = if search_fuzzy1 >= search_contains { pass += 1; "OK (≥ exact)" } else { fail += 1; "FAIL (< exact!)" };
        eprintln!("  fuzzy d=1 DAG:   {:>6} vs exact {:>6}  {}", search_fuzzy1, search_contains, status);
        for s in snippets.iter().take(show_top) { eprintln!("{}", s); }

        // 4. fuzzy d=2 contains
        let fuzzy2_cfg = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(term.to_string()),
            distance: Some(2),
            ..Default::default()
        };
        let search_fuzzy2 = search_count(&handle, &fuzzy2_cfg);
        let (_, snippets) = search_with_snippets(&handle, &fuzzy2_cfg, 20, 60);
        let status = if search_fuzzy2 >= search_fuzzy1 { pass += 1; "OK (≥ d=1)" } else { fail += 1; "FAIL (< d=1!)" };
        eprintln!("  fuzzy d=2 DAG:   {:>6} vs d=1 {:>6}  {}", search_fuzzy2, search_fuzzy1, status);
        for s in snippets.iter().take(show_top) { eprintln!("{}", s); }

        eprintln!();
    }

    // 5. contains_split ground truth
    let split_terms = &[("struct device", "content"), ("mutex lock", "content")];
    for (phrase, field) in split_terms {
        eprintln!("--- contains_split: {:?} ---", phrase);
        let words: Vec<&str> = phrase.split_whitespace().collect();

        // Ground truth: doc contains ALL words as substrings
        let gt = {
            let mut count = 0usize;
            for shard_idx in 0..handle.num_shards() {
                let shard = handle.shard(shard_idx).unwrap();
                let searcher = shard.reader.searcher();
                let f = shard.field(field).unwrap();
                for sr in searcher.segment_readers() {
                    if let Ok(store) = sr.get_store_reader(0) {
                        for did in 0..sr.max_doc() {
                            if sr.alive_bitset().map_or(true, |bs| bs.is_alive(did)) {
                                if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(did) {
                                    for (ff, val) in doc.field_values() {
                                        if ff == f {
                                            use ld_lucivy::schema::document::Value;
                                            if let Some(text) = val.as_value().as_str() {
                                                let lower = text.to_lowercase();
                                                if words.iter().all(|w| lower.contains(&w.to_lowercase())) {
                                                    count += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            count
        };

        let split_cfg = QueryConfig {
            query_type: "contains_split".into(),
            field: Some(field.to_string()),
            value: Some(phrase.to_string()),
            ..Default::default()
        };
        let search = search_count(&handle, &split_cfg);
        let (_, snippets) = search_with_snippets(&handle, &split_cfg, 20, 60);
        // contains_split uses boolean SHOULD (OR), not AND — so search >= gt
        let status = if search >= gt { pass += 1; "OK (≥ AND)" } else { fail += 1; "FAIL (< AND!)" };
        eprintln!("  DAG:    {:>6} vs gt(AND) {:>6}  {}", search, gt, status);
        for s in snippets.iter().take(show_top) { eprintln!("{}", s); }
        eprintln!();
    }

    eprintln!("=== Results: {} pass, {} fail ===", pass, fail);
    assert_eq!(fail, 0, "{} ground truth checks FAILED", fail);
}

// ═══════════════════════════════════════════════════════════════════════════
// Quick query timing: reuse persisted index, no ground truth, no events
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn bench_query_times() {
    let index_dir = format!("{}/round_robin", BENCH_BASE);
    if !std::path::Path::new(&index_dir).exists() {
        eprintln!("Skipping: no persisted index at {}", index_dir);
        return;
    }

    let handle = ShardedHandle::open(&index_dir).unwrap();
    eprintln!("\n=== Query timing on {} docs (3-run avg) ===\n", handle.num_docs());

    let queries: Vec<(&str, QueryConfig)> = vec![
        ("contains 'mutex_lock'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("mutex_lock".into()), ..Default::default()
        }),
        ("contains 'function'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("function".into()), ..Default::default()
        }),
        ("contains 'sched'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("sched".into()), ..Default::default()
        }),
        ("contains 'printk'", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("printk".into()), ..Default::default()
        }),
        ("startsWith 'sched'", QueryConfig {
            query_type: "startsWith".into(), field: Some("content".into()),
            value: Some("sched".into()), ..Default::default()
        }),
        ("startsWith 'printk'", QueryConfig {
            query_type: "startsWith".into(), field: Some("content".into()),
            value: Some("printk".into()), ..Default::default()
        }),
        ("contains_split 'struct device'", QueryConfig {
            query_type: "contains_split".into(), field: Some("content".into()),
            value: Some("struct device".into()), ..Default::default()
        }),
        ("fuzzy 'schdule' (d=1)", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("schdule".into()), distance: Some(1), ..Default::default()
        }),
        ("fuzzy 'mutex' (d=2)", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("mutex".into()), distance: Some(2), ..Default::default()
        }),
        ("contains 'drivers' (path)", QueryConfig {
            query_type: "contains".into(), field: Some("path".into()),
            value: Some("drivers".into()), ..Default::default()
        }),
        // ── Phrase queries ──
        ("phrase 'mutex lock'", QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(vec!["mutex".into(), "lock".into()]), ..Default::default()
        }),
        ("phrase 'struct device'", QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(vec!["struct".into(), "device".into()]), ..Default::default()
        }),
        ("phrase 'return error'", QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(vec!["return".into(), "error".into()]), ..Default::default()
        }),
        ("term 'mutex'", QueryConfig {
            query_type: "term".into(), field: Some("content".into()),
            value: Some("mutex".into()), ..Default::default()
        }),
        // ── New query types ──
        ("phrase_prefix 'mutex loc'", QueryConfig {
            query_type: "phrase_prefix".into(), field: Some("content".into()),
            value: Some("mutex loc".into()), ..Default::default()
        }),
        ("phrase_prefix 'struct dev'", QueryConfig {
            query_type: "phrase_prefix".into(), field: Some("content".into()),
            value: Some("struct dev".into()), ..Default::default()
        }),
        ("more_like_this 'mutex..'", QueryConfig {
            query_type: "more_like_this".into(), field: Some("content".into()),
            value: Some("mutex lock synchronization primitives kernel".into()),
            min_doc_frequency: Some(1), min_term_frequency: Some(1),
            min_word_length: Some(3), ..Default::default()
        }),
        ("dismax term×2 fields", QueryConfig {
            query_type: "disjunction_max".into(),
            queries: Some(vec![
                QueryConfig {
                    query_type: "term".into(), field: Some("content".into()),
                    value: Some("mutex".into()), ..Default::default()
                },
                QueryConfig {
                    query_type: "term".into(), field: Some("path".into()),
                    value: Some("mutex".into()), ..Default::default()
                },
            ]),
            tie_breaker: Some(0.1),
            ..Default::default()
        }),
    ];

    eprintln!("{:<35} {:>6} {:>10}", "Query", "Hits", "Time");
    eprintln!("{}", "-".repeat(55));

    for (label, config) in &queries {
        // Warmup
        let _ = time_sharded_query(&handle, config);

        // 3-run average
        let mut total_ms = 0.0;
        let mut hits = 0;
        for _ in 0..3 {
            let (h, ms) = time_sharded_query(&handle, config);
            total_ms += ms;
            hits = h;
        }
        eprintln!("{:<35} {:>6} {:>8.1}ms", label, hits, total_ms / 3.0);
    }

    // Highlight timing: term + phrase with highlight_sink
    eprintln!("\n{:<35} {:>6} {:>10}", "With highlights", "Hits", "Time");
    eprintln!("{}", "-".repeat(55));
    let hl_queries: Vec<(&str, QueryConfig)> = vec![
        ("term 'mutex' +hl", QueryConfig {
            query_type: "term".into(), field: Some("content".into()),
            value: Some("mutex".into()), ..Default::default()
        }),
        ("phrase 'mutex lock' +hl", QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(vec!["mutex".into(), "lock".into()]), ..Default::default()
        }),
        ("contains 'mutex' +hl", QueryConfig {
            query_type: "contains".into(), field: Some("content".into()),
            value: Some("mutex".into()), ..Default::default()
        }),
    ];
    for (label, config) in &hl_queries {
        let _ = search_with_snippets(&handle, config, 20, 60); // warmup
        let t0 = Instant::now();
        let (hits, snippets) = search_with_snippets(&handle, config, 20, 60);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!("{:<35} {:>6} {:>8.1}ms", label, hits, ms);
        for s in snippets.iter().take(2) { eprintln!("{}", s); }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: sfx:false mode — indexation + term/phrase OK + contains error
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_sfx_disabled() {
    let test_dir = "/tmp/lucivy_test_sfx_disabled";
    let _ = std::fs::remove_dir_all(test_dir);

    // Index with sfx:false, 4 shards
    let config: query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "title", "type": "text", "stored": true},
            {"name": "body", "type": "text", "stored": true}
        ],
        "shards": 4,
        "balance_weight": 1.0,
        "sfx": false
    })).unwrap();

    let handle = ShardedHandle::create(test_dir, &config).unwrap();
    let title_f = handle.field("title").unwrap();
    let body_f = handle.field("body").unwrap();
    let nid_f = handle.field(lucivy_core::handle::NODE_ID_FIELD).unwrap();

    // Add some documents
    let docs = vec![
        ("Mutex Design", "The mutex_lock function provides mutual exclusion for shared resources."),
        ("Scheduler Overview", "The scheduler manages process scheduling and CPU allocation."),
        ("Device Driver Guide", "Writing device drivers requires understanding struct device patterns."),
        ("Error Handling", "Return error codes from functions to signal failure conditions."),
        ("Lock Implementation", "Spinlocks and mutex locks are fundamental synchronization primitives."),
        ("Memory Management", "The kernel memory allocator handles page allocation and deallocation."),
    ];

    for (i, (title, body)) in docs.iter().enumerate() {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, i as u64);
        doc.add_text(title_f, title);
        doc.add_text(body_f, body);
        handle.add_document(doc, i as u64).unwrap();
    }
    handle.commit().unwrap();

    eprintln!("\n=== sfx:false test ({} docs, 4 shards) ===\n", handle.num_docs());

    // Verify no .sfx files were created
    let sfx_count: usize = (0..4).map(|i| {
        let shard_dir = format!("{}/shard_{}", test_dir, i);
        std::fs::read_dir(&shard_dir).map(|entries| {
            entries.flatten().filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".sfx") || name.ends_with(".sfxpost")
            }).count()
        }).unwrap_or(0)
    }).sum();
    assert_eq!(sfx_count, 0, "Expected no .sfx/.sfxpost files with sfx:false");
    eprintln!("  .sfx/.sfxpost files: {} (expected 0) ✓", sfx_count);

    // term query should work
    let term_results = handle.search(&QueryConfig {
        query_type: "term".into(), field: Some("body".into()),
        value: Some("mutex".into()), ..Default::default()
    }, 20, None).unwrap();
    assert!(!term_results.is_empty(), "term query should find results");
    eprintln!("  term 'mutex': {} hits ✓", term_results.len());

    // phrase query should work
    let phrase_results = handle.search(&QueryConfig {
        query_type: "phrase".into(), field: Some("body".into()),
        terms: Some(vec!["device".into(), "drivers".into()]), ..Default::default()
    }, 20, None).unwrap();
    assert!(!phrase_results.is_empty(), "phrase query should find results");
    eprintln!("  phrase 'device drivers': {} hits ✓", phrase_results.len());

    // fuzzy query should work
    let fuzzy_results = handle.search(&QueryConfig {
        query_type: "fuzzy".into(), field: Some("body".into()),
        value: Some("mutx".into()), distance: Some(1), ..Default::default()
    }, 20, None).unwrap();
    assert!(!fuzzy_results.is_empty(), "fuzzy query should find results");
    eprintln!("  fuzzy 'mutx' d=1: {} hits ✓", fuzzy_results.len());

    // regex query should work
    let regex_results = handle.search(&QueryConfig {
        query_type: "regex".into(), field: Some("body".into()),
        pattern: Some("sched.*".into()), ..Default::default()
    }, 20, None).unwrap();
    assert!(!regex_results.is_empty(), "regex query should find results");
    eprintln!("  regex 'sched.*': {} hits ✓", regex_results.len());

    // parse query should work
    let parse_results = handle.search(&QueryConfig {
        query_type: "parse".into(), field: Some("body".into()),
        value: Some("mutex AND lock".into()), ..Default::default()
    }, 20, None).unwrap();
    assert!(!parse_results.is_empty(), "parse query should find results");
    eprintln!("  parse 'mutex AND lock': {} hits ✓", parse_results.len());

    // phrase_prefix should work
    let pp_results = handle.search(&QueryConfig {
        query_type: "phrase_prefix".into(), field: Some("body".into()),
        value: Some("device driv".into()), ..Default::default()
    }, 20, None).unwrap();
    assert!(!pp_results.is_empty(), "phrase_prefix should find results");
    eprintln!("  phrase_prefix 'device driv': {} hits ✓", pp_results.len());

    // more_like_this should work
    let mlt_results = handle.search(&QueryConfig {
        query_type: "more_like_this".into(), field: Some("body".into()),
        value: Some("mutex lock synchronization primitives".into()),
        min_doc_frequency: Some(1), min_term_frequency: Some(1),
        min_word_length: Some(3),
        ..Default::default()
    }, 20, None).unwrap();
    eprintln!("  more_like_this: {} hits ✓", mlt_results.len());

    // contains should ERROR
    let contains_err = handle.search(&QueryConfig {
        query_type: "contains".into(), field: Some("body".into()),
        value: Some("mutex".into()), ..Default::default()
    }, 20, None);
    assert!(contains_err.is_err(), "contains should error with sfx:false");
    eprintln!("  contains 'mutex': error ✓ ({})", contains_err.unwrap_err());

    // startsWith should ERROR
    let sw_err = handle.search(&QueryConfig {
        query_type: "startsWith".into(), field: Some("body".into()),
        value: Some("sched".into()), ..Default::default()
    }, 20, None);
    assert!(sw_err.is_err(), "startsWith should error with sfx:false");
    eprintln!("  startsWith 'sched': error ✓ ({})", sw_err.unwrap_err());

    // Cleanup
    let _ = std::fs::remove_dir_all(test_dir);
    eprintln!("\n  All checks passed! ✓");
}

// ═══════════════════════════════════════════════════════════════════════════
// Score consistency: single-shard vs 4-shard top-1 scores must match
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_score_consistency_single_vs_sharded() {
    let single_dir = format!("{}/single", BENCH_BASE);
    let sharded_dir = format!("{}/round_robin", BENCH_BASE);

    if !std::path::Path::new(&single_dir).exists() || !std::path::Path::new(&sharded_dir).join("_shard_config.json").exists() {
        eprintln!("Skipping: need both single + round_robin indexes at {}", BENCH_BASE);
        eprintln!("Run: BENCH_MODE=\"SINGLE|RR\" MAX_DOCS=90000 cargo test ...");
        return;
    }

    // Open both indexes
    let d = lucivy_core::directory::StdFsDirectory::open(&single_dir).unwrap();
    let single = LucivyHandle::open(d).unwrap();
    let lv1_ndocs = single.reader.searcher().num_docs();

    for p in std::fs::read_dir(&sharded_dir).into_iter().flatten().flatten() {
        if p.file_name().to_string_lossy().ends_with(".lock") {
            let _ = std::fs::remove_file(p.path());
        }
    }
    let sharded = ShardedHandle::open(&sharded_dir).unwrap();
    let lv4_ndocs = sharded.num_docs();

    eprintln!("\n=== Score consistency: single ({} docs) vs 4-shard ({} docs) ===\n",
        lv1_ndocs, lv4_ndocs);

    let queries: Vec<(&str, QueryConfig)> = vec![
        ("term 'mutex'", QueryConfig {
            query_type: "term".into(), field: Some("content".into()),
            value: Some("mutex".into()), ..Default::default()
        }),
        ("phrase 'struct device'", QueryConfig {
            query_type: "phrase".into(), field: Some("content".into()),
            terms: Some(vec!["struct".into(), "device".into()]), ..Default::default()
        }),
        ("fuzzy 'schdule' d=1", QueryConfig {
            query_type: "fuzzy".into(), field: Some("content".into()),
            value: Some("schdule".into()), distance: Some(1), ..Default::default()
        }),
        ("regex 'mutex.*'", QueryConfig {
            query_type: "regex".into(), field: Some("content".into()),
            pattern: Some("mutex.*".into()), ..Default::default()
        }),
        ("parse 'mutex AND lock'", QueryConfig {
            query_type: "parse".into(), field: Some("content".into()),
            value: Some("mutex AND lock".into()), ..Default::default()
        }),
    ];

    let mut pass = 0u32;
    let mut fail = 0u32;
    let tolerance = 0.01; // 1% score difference allowed

    for (label, config) in &queries {
        // Single shard: direct search
        let q = query::build_query(config, &single.schema, &single.index, None).unwrap();
        let searcher = single.reader.searcher();
        let collector = ld_lucivy::collector::TopDocs::with_limit(1).order_by_score();
        let single_results = searcher.search(&*q, &collector).unwrap();
        let score_1 = single_results.first().map(|(s, _)| *s).unwrap_or(0.0);

        // 4-shard: DAG search
        let sharded_results = sharded.search(config, 1, None).unwrap();
        let score_4 = sharded_results.first().map(|r| r.score).unwrap_or(0.0);

        let diff = (score_1 - score_4).abs();
        let rel_diff = if score_1 > 0.0 { diff / score_1 } else { diff };

        if rel_diff <= tolerance {
            pass += 1;
            eprintln!("  {:<30} single={:.4}  4sh={:.4}  diff={:.4} ({:.1}%) ✓",
                label, score_1, score_4, diff, rel_diff * 100.0);
        } else {
            fail += 1;
            eprintln!("  {:<30} single={:.4}  4sh={:.4}  diff={:.4} ({:.1}%) FAIL",
                label, score_1, score_4, diff, rel_diff * 100.0);
        }
    }

    eprintln!("\n=== Score consistency: {} pass, {} fail ===", pass, fail);
    assert_eq!(fail, 0, "{} score consistency checks FAILED", fail);
}
