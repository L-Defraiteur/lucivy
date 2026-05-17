//! Ground truth test: SFX v3 on real rag3db repo.
//!
//! Indexes files from the cloned repo with sfx_version=3,
//! then verifies that contains queries return the same docs as naive grep,
//! and logs highlights + context to a file for investigation.
//!
//! Two modes tested:
//! - strict_sep=true: grep matches the literal query (case-insensitive)
//! - strict_sep=false: grep strips non-content chars from both query and text
//!
//! Run: cargo test -p lucivy-core --test test_sfx_v3_ground_truth -- --nocapture
//! Output: /tmp/v3_ground_truth_report.txt

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

const REPO_PATH: &str = "/tmp/rag3db-bench";
const MAX_FILE_SIZE: u64 = 100_000;
const REPORT_PATH: &str = "/tmp/v3_ground_truth_report.txt";

// ─── Content char (mirrors tokenizer::equal_chunk::is_content_char) ──────

fn is_content_char(c: char) -> bool {
    !c.is_ascii() || c.is_ascii_alphanumeric()
}

/// Strip non-content chars from a string (for sep-agnostic matching).
fn strip_seps(s: &str) -> String {
    s.chars().filter(|c| is_content_char(*c)).collect()
}

// ─── File collection ──────────────────────────────────────────────────────

fn collect_files(max_docs: usize) -> Vec<(String, String)> {
    let root = std::path::Path::new(REPO_PATH);
    if !root.exists() {
        eprintln!("Skipping: clone rag3db to {REPO_PATH} first");
        eprintln!("  git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git {REPO_PATH}");
        return vec![];
    }
    let exclude_dirs = ["target", "node_modules", ".git", "build", "__pycache__", "playground"];
    let mut files = Vec::new();

    fn walk(dir: &std::path::Path, root: &std::path::Path, exclude: &[&str],
            files: &mut Vec<(String, String)>, max: usize) {
        if files.len() >= max { return; }
        let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return };
        for entry in entries.flatten() {
            if files.len() >= max { return; }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, root, exclude, files, max);
                }
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE_SIZE || size == 0 { continue; }
                let bytes = match std::fs::read(&path) { Ok(b) => b, Err(_) => continue };
                if bytes.contains(&0) { continue; }
                let content = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => continue };
                if content.trim().is_empty() { continue; }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                files.push((rel, content));
            }
        }
    }
    walk(root, root, &exclude_dirs, &mut files, max_docs);
    files
}

// ─── Index creation ───────────────────────────────────────────────────────

fn create_v3_index(files: &[(String, String)]) -> LucivyHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": 3
    })).unwrap();

    let dir = ld_lucivy::directory::RamDirectory::default();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        w.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            w.add_document(doc).unwrap();
            if (i + 1) % 500 == 0 {
                w.commit().unwrap();
                eprintln!("  indexed {}/{}", i + 1, files.len());
            }
        }
        w.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

// ─── Ground truth (naive grep) ────────────────────────────────────────────

/// Literal case-insensitive grep (for strict_sep=true).
fn grep_docs_strict(files: &[(String, String)], needle: &str) -> HashSet<usize> {
    let lower = needle.to_lowercase();
    files.iter().enumerate()
        .filter(|(_, (_, c))| c.to_lowercase().contains(&lower))
        .map(|(i, _)| i)
        .collect()
}

/// Sep-agnostic grep (for strict_sep=false).
/// Strips non-content chars from both query and text, then searches.
fn grep_docs_relaxed(files: &[(String, String)], needle: &str) -> HashSet<usize> {
    let stripped_query = strip_seps(&needle.to_lowercase());
    if stripped_query.is_empty() { return HashSet::new(); }
    files.iter().enumerate()
        .filter(|(_, (_, c))| {
            let stripped_content = strip_seps(&c.to_lowercase());
            stripped_content.contains(&stripped_query)
        })
        .map(|(i, _)| i)
        .collect()
}

// ─── Search with highlights ───────────────────────────────────────────────

struct SearchResult {
    doc_indices: HashSet<usize>,
    highlights: Vec<(usize, usize, usize)>,
}

fn search_v3(
    handle: &LucivyHandle,
    files: &[(String, String)],
    value: &str,
    strict_separators: bool,
) -> SearchResult {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict_separators),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(10_000).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();

    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut doc_indices = HashSet::new();
    let mut highlights = Vec::new();

    for (_, addr) in &results {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let file_idx = doc.field_values()
            .find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64())
            .unwrap_or(0) as usize;
        doc_indices.insert(file_idx);

        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        if let Some(hl_map) = sink.get(seg_id, addr.doc_id) {
            if let Some(offsets) = hl_map.get("content") {
                for [start, end] in offsets {
                    highlights.push((file_idx, *start, *end));
                }
            }
        }
    }

    SearchResult { doc_indices, highlights }
}

// ─── Report writing ───────────────────────────────────────────────────────

fn write_report(
    out: &mut dyn Write,
    query: &str,
    mode: &str,
    files: &[(String, String)],
    grep_set: &HashSet<usize>,
    v3_result: &SearchResult,
) {
    let v3_set = &v3_result.doc_indices;
    let only_grep: Vec<usize> = grep_set.difference(v3_set).copied().collect();
    let only_v3: Vec<usize> = v3_set.difference(grep_set).copied().collect();

    writeln!(out, "\n{}", "=".repeat(60)).ok();
    writeln!(out, "Query: {:?} [{}]  grep={} v3={}", query, mode, grep_set.len(), v3_set.len()).ok();

    if !only_grep.is_empty() {
        writeln!(out, "\n  FALSE NEGATIVES (grep found, v3 missed): {} docs", only_grep.len()).ok();
        for &idx in only_grep.iter().take(5) {
            let (path, content) = &files[idx];
            writeln!(out, "    doc={idx} path={path}").ok();
            let lower_content = content.to_lowercase();
            let lower_query = query.to_lowercase();
            if let Some(pos) = lower_content.find(&lower_query) {
                let ctx_start = pos.saturating_sub(30);
                let ctx_end = (pos + query.len() + 30).min(content.len());
                let cs = snap_back(content, ctx_start);
                let ce = snap_fwd(content, ctx_end);
                writeln!(out, "    grep match at byte {pos}: ...{}[{}]{}...",
                    &content[cs..pos], &content[pos..pos+query.len().min(content.len()-pos)],
                    &content[(pos+query.len()).min(content.len())..ce]).ok();
            }
        }
    }

    if !only_v3.is_empty() {
        writeln!(out, "\n  FALSE POSITIVES (v3 found, grep missed): {} docs", only_v3.len()).ok();
        for &idx in only_v3.iter().take(5) {
            let (path, _content) = &files[idx];
            writeln!(out, "    doc={idx} path={path}").ok();
            for &(fidx, bf, bt) in &v3_result.highlights {
                if fidx == idx {
                    let content = &files[idx].1;
                    let bf_s = snap_back(content, bf.saturating_sub(20));
                    let bt_e = snap_fwd(content, (bt + 20).min(content.len()));
                    let bf_c = bf.min(content.len());
                    let bt_c = bt.min(content.len());
                    writeln!(out, "    highlight [{bf}..{bt}]: ...{}>>{}<<{}...",
                        &content[bf_s..bf_c], &content[bf_c..bt_c], &content[bt_c..bt_e]).ok();
                }
            }
        }
    }

    if only_grep.is_empty() && only_v3.is_empty() {
        writeln!(out, "  OK — perfect match").ok();
    }

    if !v3_result.highlights.is_empty() {
        writeln!(out, "\n  Sample highlights (first 3):").ok();
        for &(fidx, bf, bt) in v3_result.highlights.iter().take(3) {
            let content = &files[fidx].1;
            let bf_c = bf.min(content.len());
            let bt_c = bt.min(content.len());
            let cs = snap_back(content, bf_c.saturating_sub(20));
            let ce = snap_fwd(content, (bt_c + 20).min(content.len()));
            writeln!(out, "    [{bf}..{bt}] ...{}>>{}<<{}...",
                &content[cs..bf_c], &content[bf_c..bt_c], &content[bt_c..ce]).ok();
        }
    }
}

fn snap_back(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

fn snap_fwd(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
    p
}

// ─── Query definitions ──────────────────────────────────────────────────

struct GroundTruthQuery {
    text: &'static str,
    strict_sep: bool,
}

impl GroundTruthQuery {
    fn strict(text: &'static str) -> Self { Self { text, strict_sep: true } }
    fn relaxed(text: &'static str) -> Self { Self { text, strict_sep: false } }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[test]
fn v3_ground_truth_contains() {
    let files = collect_files(500);
    if files.is_empty() { return; }
    eprintln!("\n=== V3 Ground Truth: {} files ===\n", files.len());

    let t0 = std::time::Instant::now();
    let handle = create_v3_index(&files);
    let index_time = t0.elapsed().as_secs_f64();
    eprintln!("Index time: {:.1}s", index_time);

    let mut report = std::fs::File::create(REPORT_PATH).unwrap();
    writeln!(report, "V3 Ground Truth Report — {} files, indexed in {:.1}s\n", files.len(), index_time).ok();

    // Each query tested in both modes:
    // strict_sep=true: literal grep comparison
    // strict_sep=false: sep-agnostic grep comparison
    let queries: Vec<GroundTruthQuery> = vec![
        // Simple words — both modes should agree
        GroundTruthQuery::strict("function"),
        GroundTruthQuery::relaxed("function"),
        GroundTruthQuery::strict("return"),
        GroundTruthQuery::strict("struct"),
        GroundTruthQuery::strict("void"),
        GroundTruthQuery::strict("rag3db"),
        GroundTruthQuery::strict("include"),
        // Queries with separators — test both modes explicitly
        GroundTruthQuery::strict("uint64_t"),
        GroundTruthQuery::relaxed("uint64_t"),
        GroundTruthQuery::strict("std::unique_ptr"),
        GroundTruthQuery::relaxed("std::unique_ptr"),
        GroundTruthQuery::strict("ku_dynamic_cast"),
        GroundTruthQuery::relaxed("ku_dynamic_cast"),
        // Mixed case — relaxed mode expands matches
        GroundTruthQuery::strict("TableFunction"),
        GroundTruthQuery::relaxed("TableFunction"),
    ];

    let mut pass = 0u32;
    let mut fail = 0u32;

    eprintln!("{:<35} {:>5} {:>8} {:>8} {:>8}", "Query", "Mode", "Grep", "V3", "Status");
    eprintln!("{}", "-".repeat(70));

    for q in &queries {
        let mode_label = if q.strict_sep { "strict" } else { "relax" };
        let t = std::time::Instant::now();

        let grep_set = if q.strict_sep {
            grep_docs_strict(&files, q.text)
        } else {
            grep_docs_relaxed(&files, q.text)
        };
        let v3_result = search_v3(&handle, &files, q.text, q.strict_sep);
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let status = if v3_result.doc_indices == grep_set { "OK" } else { "FAIL" };
        eprintln!("{:<35} {:>5} {:>8} {:>8} {:>6} ({:.1}ms)",
            q.text, mode_label, grep_set.len(), v3_result.doc_indices.len(), status, ms);

        write_report(&mut report, q.text, mode_label, &files, &grep_set, &v3_result);

        if v3_result.doc_indices == grep_set { pass += 1; } else { fail += 1; }
    }

    eprintln!("\n{pass} pass, {fail} fail");
    eprintln!("Report: {REPORT_PATH}");
    assert_eq!(fail, 0, "ground truth mismatch — see {REPORT_PATH}");
}

/// Debug targeted: for a specific query, dump chains + matches for FP docs.
/// Run: cargo test -p lucivy-core --test test_sfx_v3_ground_truth debug_struct -- --nocapture
#[test]
fn debug_struct_fp() {
    let files = collect_files(500);
    if files.is_empty() { return; }

    let handle = create_v3_index(&files);
    let query = "struct";
    let grep_set = grep_docs_strict(&files, query);
    let v3_result = search_v3(&handle, &files, query, true);

    let fp_docs: Vec<usize> = v3_result.doc_indices.difference(&grep_set).copied().collect();
    eprintln!("\n=== DEBUG struct strict ===");
    eprintln!("grep={} v3={} FP={}", grep_set.len(), v3_result.doc_indices.len(), fp_docs.len());

    let mut dbg = std::fs::File::create("/tmp/v3_debug_struct.txt").unwrap();
    writeln!(dbg, "FP docs for 'struct' strict: {} docs\n", fp_docs.len()).ok();

    for &doc_idx in fp_docs.iter().take(5) {
        let (path, content) = &files[doc_idx];
        writeln!(dbg, "--- doc={doc_idx} path={path} ---").ok();

        // Show highlights for this doc
        for &(fidx, bf, bt) in &v3_result.highlights {
            if fidx == doc_idx {
                let bf_c = bf.min(content.len());
                let bt_c = bt.min(content.len());
                let ctx_s = snap_back(content, bf_c.saturating_sub(30));
                let ctx_e = snap_fwd(content, (bt_c + 30).min(content.len()));
                writeln!(dbg, "  highlight [{bf}..{bt}] span={}: ...{}>>{}<<{}...",
                    bt - bf,
                    &content[ctx_s..bf_c], &content[bf_c..bt_c], &content[bt_c..ctx_e]).ok();
            }
        }
        writeln!(dbg).ok();
    }
    eprintln!("Debug output: /tmp/v3_debug_struct.txt");
}
