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
        ("contains 'mutex_lock'", QueryConfig {
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

    // ── Sanity check: run ALL queries with highlights ─────────────────
    {
        let handle = sharded_rr.as_ref().or(sharded_ta.as_ref());
        let mode = if sharded_rr.is_some() { "RR" } else { "TA" };
        if let Some(handle) = handle {
            for (label, qconfig) in &queries {
                let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
                let results = handle.search(qconfig, 2, Some(Arc::clone(&sink))).unwrap();
                let query_val = qconfig.value.as_deref().unwrap_or("?");
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
    if let Some(handle) = sharded_rr.as_ref().or(sharded_ta.as_ref()) {
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
        let verify = std::env::var("LUCIVY_VERIFY").map(|v| v == "1").unwrap_or(false);
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

        // SFX path diagnostic: trace prefix_walk → parents → sfxpost → doc_ids
        eprintln!("\n=== Real search vs ground truth (via DiagBus) ===");
        for term in &["mutex", "lock", "function", "printk", "sched"] {
            // Subscribe to search events BEFORE the search
            let rx = ld_lucivy::diag::diag_bus().subscribe(
                ld_lucivy::diag::DiagFilter::SfxTerm(term.to_string()),
            );

            // Run the real search via each shard
            for shard_idx in 0..4 {
                if let Some(shard) = handle.shard(shard_idx) {
                    let searcher = shard.reader.searcher();
                    let field = shard.field("content").unwrap();
                    let q = ld_lucivy::query::SuffixContainsQuery::new(
                        field, term.to_string(),
                    );
                    let _ = searcher.search(&q, &ld_lucivy::collector::Count);
                }
            }

            // Collect totals from SearchComplete events (one per segment)
            let mut total_search = 0u32;
            while let Ok(event) = rx.try_recv() {
                if let ld_lucivy::diag::DiagEvent::SearchComplete { total_docs, .. } = event {
                    total_search += total_docs;
                }
            }

            ld_lucivy::diag::diag_bus().clear();

            // Compare with ground truth
            let gt_status = if verify {
                let gt_reports = lucivy_core::diagnostics::inspect_term_sharded_verified(handle, "content", term);
                let gt: u32 = gt_reports.iter().map(|(_, r)| r.ground_truth_count.unwrap_or(0)).sum();
                if total_search == gt { format!(" | ground_truth={gt} ✓ MATCH") }
                else { format!(" | ground_truth={gt} ✗ DIFF={}", (gt as i64 - total_search as i64).abs()) }
            } else { String::new() };
            eprintln!("\n  search {:?}: {} docs{}", term, total_search, gt_status);
        }

        // Trace missing docs for terms with DIFF
        if verify {
            eprintln!("\n=== Missing docs trace ===");
            for term in &["sched", "lock"] {
              for shard_idx in 0..4 {
                // Use trace_search on missing docs
                if let Some(shard) = handle.shard(shard_idx) {
                    let searcher = shard.reader.searcher();
                    let field = shard.field("content").unwrap();
                    let search_lower = term.to_lowercase();

                    // Collect search doc_ids via DiagBus
                    let rx = ld_lucivy::diag::diag_bus().subscribe(
                        ld_lucivy::diag::DiagFilter::SfxTerm(term.to_string()),
                    );
                    let q = ld_lucivy::query::SuffixContainsQuery::new(field, term.to_string());
                    let _ = searcher.search(&q, &ld_lucivy::collector::Count);
                    let mut search_docs = std::collections::HashSet::new();
                    while let Ok(event) = rx.try_recv() {
                        if let ld_lucivy::diag::DiagEvent::SearchMatch { doc_id, .. } = event {
                            search_docs.insert(doc_id);
                        }
                    }
                    ld_lucivy::diag::diag_bus().clear();

                    // Find ground truth docs NOT in search results, trace first 2
                    let mut traced = 0;
                    for sr in searcher.segment_readers() {
                        if let Ok(store) = sr.get_store_reader(0) {
                            for did in 0..sr.max_doc() {
                                if sr.alive_bitset().map_or(true, |bs| bs.is_alive(did)) {
                                    if let Ok(doc) = store.get::<ld_lucivy::LucivyDocument>(did) {
                                        for (f, val) in doc.field_values() {
                                            if f == field {
                                                use ld_lucivy::schema::document::Value;
                                                if let Some(text) = val.as_value().as_str() {
                                                    if text.to_lowercase().contains(&search_lower) && !search_docs.contains(&did) {
                                                        if traced < 2 {
                                                            let trace = lucivy_core::diagnostics::trace_search(shard, "content", term, did);
                                                            eprintln!("\n{}", trace);
                                                            traced += 1;
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
                    if traced == 0 {
                        eprintln!("  {:?}: no missing docs in shard {}", term, shard_idx);
                    }
                }
              } // end shard loop
            }
        }
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
    // Keep the index for post-mortem inspection
    eprintln!("\n=== Index preserved at {} ===", BENCH_BASE);
}
