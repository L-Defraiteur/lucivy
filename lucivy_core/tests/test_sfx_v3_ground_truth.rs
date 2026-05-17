//! Ground truth test: SFX v3 on real rag3db repo.
//!
//! Indexes files from the cloned repo with sfx_version=3,
//! then verifies that contains queries return the same docs as naive grep,
//! and logs highlights + context to a file for investigation.
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

/// Returns set of file indices that contain needle (case-insensitive substring).
fn grep_docs(files: &[(String, String)], needle: &str) -> HashSet<usize> {
    let lower = needle.to_lowercase();
    files.iter().enumerate()
        .filter(|(_, (_, c))| c.to_lowercase().contains(&lower))
        .map(|(i, _)| i)
        .collect()
}

// ─── Search with highlights ───────────────────────────────────────────────

struct SearchResult {
    /// File indices (into the files vec) that matched.
    doc_indices: HashSet<usize>,
    /// Per-match: (file_index, byte_from, byte_to).
    highlights: Vec<(usize, usize, usize)>,
}

fn search_v3(handle: &LucivyHandle, files: &[(String, String)], value: &str) -> SearchResult {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
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
    files: &[(String, String)],
    grep_set: &HashSet<usize>,
    v3_result: &SearchResult,
) {
    let v3_set = &v3_result.doc_indices;
    let only_grep: Vec<usize> = grep_set.difference(v3_set).copied().collect();
    let only_v3: Vec<usize> = v3_set.difference(grep_set).copied().collect();

    writeln!(out, "\n{}", "=".repeat(60)).ok();
    writeln!(out, "Query: {:?}  grep={} v3={}", query, grep_set.len(), v3_set.len()).ok();

    if !only_grep.is_empty() {
        writeln!(out, "\n  FALSE NEGATIVES (grep found, v3 missed): {} docs", only_grep.len()).ok();
        for &idx in only_grep.iter().take(5) {
            let (path, content) = &files[idx];
            writeln!(out, "    doc={idx} path={path}").ok();
            // Show where grep finds the match
            let lower_content = content.to_lowercase();
            let lower_query = query.to_lowercase();
            if let Some(pos) = lower_content.find(&lower_query) {
                let ctx_start = pos.saturating_sub(30);
                let ctx_end = (pos + query.len() + 30).min(content.len());
                // Snap to char boundaries
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
            // Show v3 highlights for this doc
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

    // Show a few highlight samples
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

    let queries = &[
        "function",
        "return",
        "include",
        "struct",
        "void",
        "uint64_t",
        "std::unique_ptr",
        "ku_dynamic_cast",
        "TableFunction",
        "rag3db",
    ];

    let mut pass = 0u32;
    let mut fail = 0u32;

    eprintln!("{:<30} {:>8} {:>8} {:>8}", "Query", "Grep", "V3", "Status");
    eprintln!("{}", "-".repeat(60));

    for q in queries {
        let t = std::time::Instant::now();
        let grep_set = grep_docs(&files, q);
        let v3_result = search_v3(&handle, &files, q);
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let status = if v3_result.doc_indices == grep_set { "OK" } else { "FAIL" };
        eprintln!("{:<30} {:>8} {:>8} {:>6} ({:.1}ms)",
            q, grep_set.len(), v3_result.doc_indices.len(), status, ms);

        write_report(&mut report, q, &files, &grep_set, &v3_result);

        if v3_result.doc_indices == grep_set { pass += 1; } else { fail += 1; }
    }

    eprintln!("\n{pass} pass, {fail} fail");
    eprintln!("Report: {REPORT_PATH}");
    assert_eq!(fail, 0, "ground truth mismatch — see {REPORT_PATH}");
}
