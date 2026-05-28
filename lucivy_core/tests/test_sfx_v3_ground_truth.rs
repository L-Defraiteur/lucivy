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
///
/// Matches by ADJACENT WORDS, not linear concatenation of the whole file.
/// This mirrors v3 semantics: separators are ignored but word boundaries matter.
///
/// Algorithm: split text into words (runs of content chars), strip each word,
/// then use a sliding window of adjacent stripped words to find the query.
fn grep_docs_relaxed(files: &[(String, String)], needle: &str) -> HashSet<usize> {
    let stripped_query = strip_seps(&needle.to_lowercase());
    if stripped_query.is_empty() { return HashSet::new(); }

    files.iter().enumerate()
        .filter(|(_, (_, content))| {
            let lower = content.to_lowercase();
            // Extract words: runs of content chars
            let words: Vec<String> = lower
                .split(|c: char| !is_content_char(c))
                .filter(|w| !w.is_empty())
                .map(|w| w.to_string())
                .collect();

            // Sliding window: concatenate adjacent words and check if query is a substring
            // The query could span at most ceil(query.len() / 1) = query.len() words
            // but practically limited. Use a window large enough.
            // Sliding window: concatenate adjacent words. The query can span
            // across word boundaries, so we keep adding words as long as the
            // query could still straddle the junction.
            let qlen = stripped_query.len();
            for start in 0..words.len() {
                let mut concat = String::new();
                for end in start..words.len() {
                    concat.push_str(&words[end]);
                    if concat.len() >= qlen {
                        if concat.contains(&stripped_query) {
                            return true;
                        }
                        // Only stop if the last qlen-1 bytes of concat can't
                        // possibly start the query (no overlap with next word).
                        // Simple bound: stop when concat is qlen bytes longer
                        // than the query — the query can't straddle further.
                        if concat.len() >= qlen * 2 {
                            break;
                        }
                    }
                }
            }
            false
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
    let files = collect_files(5000);
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
    let mut fail_entries: Vec<serde_json::Value> = Vec::new();
    let diag_mode = std::env::var("V3_DIAG").is_ok();

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

        if v3_result.doc_indices == grep_set {
            pass += 1;
        } else {
            fail += 1;

            // Record failure for diag pass
            let only_grep: Vec<usize> = grep_set.difference(&v3_result.doc_indices).copied().collect();
            let only_v3: Vec<usize> = v3_result.doc_indices.difference(&grep_set).copied().collect();
            fail_entries.push(serde_json::json!({
                "query": q.text,
                "strict_sep": q.strict_sep,
                "fn_doc_indices": only_grep,
                "fp_doc_indices": only_v3,
                "fn_paths": only_grep.iter().map(|&i| &files[i].0).collect::<Vec<_>>(),
                "fp_paths": only_v3.iter().map(|&i| &files[i].0).collect::<Vec<_>>(),
            }));
        }
    }

    eprintln!("\n{pass} pass, {fail} fail");
    eprintln!("Report: {REPORT_PATH}");

    // Export failures to JSON for diag pass
    if !fail_entries.is_empty() {
        let json = serde_json::to_string_pretty(&fail_entries).unwrap();
        std::fs::write("/tmp/v3_ground_truth_fails.json", &json).ok();
        eprintln!("Failures exported: /tmp/v3_ground_truth_fails.json");
    }

    // V3_DIAG=1: re-test FN docs in isolation for each failing query
    if diag_mode && !fail_entries.is_empty() {
        eprintln!("\n=== DIAG MODE: re-testing FN docs in isolation ===\n");
        for entry in &fail_entries {
            let query = entry["query"].as_str().unwrap();
            let strict = entry["strict_sep"].as_bool().unwrap();
            let fn_indices: Vec<usize> = entry["fn_doc_indices"].as_array().unwrap()
                .iter().filter_map(|v| v.as_u64().map(|i| i as usize)).collect();
            let fp_indices: Vec<usize> = entry["fp_doc_indices"].as_array().unwrap()
                .iter().filter_map(|v| v.as_u64().map(|i| i as usize)).collect();
            let mode = if strict { "strict" } else { "relax" };

            if !fn_indices.is_empty() {
                // Re-index just the FN docs
                let fn_files: Vec<(String, String)> = fn_indices.iter()
                    .map(|&i| files[i].clone()).collect();
                let mini = create_v3_index(&fn_files);
                let mini_result = search_v3(&mini, &fn_files, query, strict);
                let found = mini_result.doc_indices.len();
                let total = fn_files.len();
                let verdict = if found == total { "SCALE-DEPENDENT" } else { "PER-DOC BUG" };
                let label = format!("{} {}", query, mode);
                eprintln!("  {label:30} FN: {found}/{total} in isolation → {verdict}");

                // If per-doc bug, show which docs still fail
                if found < total {
                    for (i, (path, _)) in fn_files.iter().enumerate() {
                        if !mini_result.doc_indices.contains(&(i as usize)) {
                            eprintln!("    still missing: {path}");
                        }
                    }
                }
            }

            if !fp_indices.is_empty() {
                let label = format!("{} {}", query, mode);
                let n = fp_indices.len();
                eprintln!("  {label:30} FP: {n} docs — grep logic issue?");
            }

            // Re-run with trace enabled — export to JSON
            std::env::set_var("V3_DEBUG_QUERY", query);
            let _ = search_v3(&handle, &files, query, strict);
            std::env::remove_var("V3_DEBUG_QUERY");
            let traces = ld_lucivy::suffix_fst::briques::trace::trace_drain_all();
            let trace_json: Vec<serde_json::Value> = traces.iter().map(|(tid, trace)| {
                serde_json::json!({
                    "trace_id": tid,
                    "query": query,
                    "mode": mode,
                    "num_events": trace.events.len(),
                    "events": trace.events.iter().map(|ev| {
                        serde_json::json!({
                            "label": ev.label,
                            "depth": ev.depth,
                            "data": ev.data.iter().map(|(k,v)| (k.clone(), serde_json::Value::String(v.clone()))).collect::<serde_json::Map<String, serde_json::Value>>(),
                        })
                    }).collect::<Vec<_>>(),
                })
            }).collect();
            let trace_path = format!("/tmp/v3_trace_{}_{}.json", query.replace("::", "_").replace(" ", "_"), mode);
            let json = serde_json::to_string_pretty(&trace_json).unwrap();
            std::fs::write(&trace_path, &json).ok();
            let total_events: usize = traces.iter().map(|(_, t)| t.events.len()).sum();
            eprintln!("  Trace: {trace_path} ({} segments, {total_events} events)", traces.len());

            // DAG explain: run find_literal_v3_dag_explained per segment
            {
                use ld_lucivy::suffix_fst::file_v3::SfxFileReaderV3;
                use ld_lucivy::suffix_fst::briques::context::BriquesContext;
                use ld_lucivy::suffix_fst::briques::dag_builder::find_literal_v3_dag_explained;
                use ld_lucivy::tokenizer::equal_chunk::is_content_char;

                let effective_query: String = if strict {
                    query.to_string()
                } else {
                    query.chars().filter(|c| is_content_char(*c)).collect()
                };

                let searcher = handle.reader.searcher();
                let content_f = handle.field("content").unwrap();
                let mut dag_explains = Vec::new();

                for (seg_ord, seg_reader) in searcher.segment_readers().iter().enumerate() {
                    let sfx_bytes = match seg_reader.sfx_file(content_f)
                        .and_then(|fs| fs.read_bytes().ok()) {
                        Some(b) => b.as_ref().to_vec(),
                        None => continue,
                    };
                    let reader = match SfxFileReaderV3::open(&sfx_bytes) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let pr = ld_lucivy::query::posting_resolver::build_resolver(seg_reader, content_f).unwrap();

                    let load = |ext: &str| -> Option<Vec<u8>> {
                        seg_reader.sfx_index_file(ext, content_f)
                            .and_then(|fs| fs.read_bytes().ok())
                            .map(|b| b.as_ref().to_vec())
                    };
                    let posmap_bytes = load("posmap");
                    let bytemap_bytes = load("bytemap");
                    let wsp_bytes = load("word_sfxpost");
                    let sib_bytes = load("sibling_v3");
                    let tt_bytes = load("termtexts");

                    let ctx = BriquesContext {
                        reader: &reader,
                        resolver: &*pr,
                        filter_docs: None,
                        debug: false,
                        trace_id: None,
                        posmap: posmap_bytes.as_ref().and_then(|b| ld_lucivy::suffix_fst::posmap::PosMapReader::open(b)),
                        bytemap: bytemap_bytes.as_ref().and_then(|b| ld_lucivy::suffix_fst::bytemap::ByteBitmapReader::open(b)),
                        word_sfxpost: wsp_bytes.as_ref().and_then(|b| ld_lucivy::suffix_fst::word_sfxpost::WordSfxPostReader::open(b)),
                        sibling_v3: sib_bytes.as_ref().and_then(|b| ld_lucivy::suffix_fst::sibling_table::SiblingTableReader::open(b)),
                        termtexts: tt_bytes.as_ref().and_then(|b| ld_lucivy::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b)),
                    };

                    let r = find_literal_v3_dag_explained(&ctx, &effective_query, false, strict);

                    dag_explains.push(serde_json::json!({
                        "segment": seg_ord,
                        "segment_id": format!("{:?}", seg_reader.segment_id()),
                        "query": query,
                        "effective_query": effective_query,
                        "mode": mode,
                        "matches_count": r.matches.len(),
                        "mermaid": r.dump_mermaid(),
                        "dag_summary": r.dag_info.display_summary(),
                        "edge_data": r.dump_edge_data(),
                    }));
                }

                let dag_path = format!("/tmp/v3_dag_{}_{}.json",
                    query.replace("::", "_").replace(" ", "_"), mode);
                let json = serde_json::to_string_pretty(&dag_explains).unwrap();
                std::fs::write(&dag_path, &json).ok();
                eprintln!("  DAG explain: {dag_path} ({} segments)", dag_explains.len());
            }

            // ── Doc Forensics: find the FN doc's segment, tokenize, check FST ──
            if !fn_indices.is_empty() {
                use ld_lucivy::suffix_fst::file_v3::SfxFileReaderV3;
                use ld_lucivy::suffix_fst::briques::fst_walk;
                use ld_lucivy::tokenizer::equal_chunk::{segment_and_chunk, is_content_char};

                let effective_query: String = if strict {
                    query.to_string()
                } else {
                    query.chars().filter(|c| is_content_char(*c)).collect()
                };

                let searcher = handle.reader.searcher();
                let content_f = handle.field("content").unwrap();
                let nid_f = handle.field(NODE_ID_FIELD).unwrap();

                let mut forensics = Vec::new();

                for &global_idx in &fn_indices {
                    let (path, content) = &files[global_idx];
                    let mut doc_forensic = serde_json::json!({
                        "global_doc_idx": global_idx,
                        "path": path,
                        "query": query,
                        "effective_query": effective_query,
                    });

                    // 1. Tokenize the doc and find chunks containing the query
                    let chunks = segment_and_chunk(content, 8);
                    let lower_q = effective_query.to_lowercase();
                    let mut relevant_chunks: Vec<serde_json::Value> = Vec::new();
                    let mut offset = 0usize;
                    for (ci, (chunk_text, meta)) in chunks.iter().enumerate() {
                        let chunk_lower = chunk_text.to_lowercase();
                        let ovl_text = if ci + 1 < chunks.len() {
                            let next = &chunks[ci + 1].0;
                            &next[..2.min(next.len())]
                        } else { "" };
                        let extended = format!("{}{}", chunk_lower, ovl_text.to_lowercase());
                        if extended.contains(&lower_q) || lower_q.contains(&chunk_lower) {
                            relevant_chunks.push(serde_json::json!({
                                "chunk_idx": ci,
                                "byte_offset": offset,
                                "text": chunk_text,
                                "extended": format!("{}{}", chunk_text, ovl_text),
                                "content_len": meta.content_len,
                                "sep_len": meta.sep_len,
                                "word_id": meta.word_id,
                            }));
                        }
                        offset += chunk_text.len();
                    }
                    doc_forensic["relevant_chunks"] = serde_json::json!(relevant_chunks);

                    // 2. Find which segment contains this doc
                    for seg_ord in 0..searcher.segment_readers().len() {
                        let seg_reader = searcher.segment_reader(seg_ord as u32);
                        let max_doc = seg_reader.max_doc();

                        let mut found_local_id = None;
                        for local_doc in 0..max_doc {
                            let doc = searcher.doc::<ld_lucivy::LucivyDocument>(
                                ld_lucivy::DocAddress::new(seg_ord as u32, local_doc)
                            );
                            if let Ok(doc) = doc {
                                use ld_lucivy::schema::document::Value;
                                let nid = doc.field_values()
                                    .find(|(f, _)| *f == nid_f)
                                    .and_then(|(_, v)| v.as_value().as_u64());
                                if nid == Some(global_idx as u64) {
                                    found_local_id = Some(local_doc);
                                    break;
                                }
                            }
                        }

                        let Some(local_doc_id) = found_local_id else { continue };

                        doc_forensic["segment"] = serde_json::json!(seg_ord);
                        doc_forensic["segment_id"] = serde_json::json!(format!("{:?}", seg_reader.segment_id()));
                        doc_forensic["local_doc_id"] = serde_json::json!(local_doc_id);
                        doc_forensic["segment_max_doc"] = serde_json::json!(max_doc);

                        // 3. Use fst_candidates_v3 + postings to check
                        let sfx_bytes = match seg_reader.sfx_file(content_f)
                            .and_then(|fs| fs.read_bytes().ok()) {
                            Some(b) => b.as_ref().to_vec(),
                            None => continue,
                        };
                        let sfx_reader = match SfxFileReaderV3::open(&sfx_bytes) {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        let pr = ld_lucivy::query::posting_resolver::build_resolver(seg_reader, content_f).unwrap();

                        // Get candidates for the query
                        let candidates = fst_walk::fst_candidates_v3(&sfx_reader, &effective_query, false, strict);

                        let mut cand_details: Vec<serde_json::Value> = Vec::new();
                        for c in &candidates {
                            let postings = pr.resolve(c.raw_ordinal);
                            let has_our_doc = postings.iter().any(|pe| pe.doc_id == local_doc_id);
                            let doc_ids: Vec<u32> = postings.iter().map(|pe| pe.doc_id).collect();
                            cand_details.push(serde_json::json!({
                                "ordinal": c.raw_ordinal,
                                "sti": c.sti,
                                "own_len": c.own_len,
                                "sep_len": c.sep_len,
                                "overlap_len": c.overlap_len,
                                "ws": c.is_word_start,
                                "total_postings": postings.len(),
                                "has_fn_doc": has_our_doc,
                                "doc_ids": &doc_ids[..doc_ids.len().min(30)],
                            }));
                        }
                        doc_forensic["candidates"] = serde_json::json!(cand_details);
                        let with_doc = cand_details.iter()
                            .filter(|e| e["has_fn_doc"].as_bool() == Some(true)).count();
                        doc_forensic["candidates_with_fn_doc"] = serde_json::json!(with_doc);

                        // 4. Also try falling walk splits to see if cross-token would help
                        let splits = fst_walk::falling_walk_chunks(&sfx_reader, &effective_query);
                        let mut split_details: Vec<serde_json::Value> = Vec::new();
                        for s in &splits {
                            let postings = pr.resolve(s.parent.raw_ordinal);
                            let has_our_doc = postings.iter().any(|pe| pe.doc_id == local_doc_id);
                            split_details.push(serde_json::json!({
                                "query_consumed": s.query_consumed,
                                "ordinal": s.parent.raw_ordinal,
                                "sti": s.parent.sti,
                                "own_len": s.parent.own_len,
                                "has_fn_doc": has_our_doc,
                            }));
                        }
                        doc_forensic["splits"] = serde_json::json!(split_details);

                        break;
                    }

                    forensics.push(doc_forensic);
                }

                let forensics_path = format!("/tmp/v3_forensics_{}_{}.json",
                    query.replace("::", "_").replace(" ", "_"), mode);
                let json = serde_json::to_string_pretty(&forensics).unwrap();
                std::fs::write(&forensics_path, &json).ok();
                eprintln!("  Forensics: {forensics_path}");
            }
        }
    }

    assert_eq!(fail, 0, "ground truth mismatch — see {REPORT_PATH}");
}

/// Debug targeted: for a specific query, dump chains + matches for FP docs.
/// Run: cargo test -p lucivy-core --test test_sfx_v3_ground_truth debug_struct -- --nocapture
#[test]
fn debug_struct_fp() {
    use ld_lucivy::tokenizer::equal_chunk::segment_and_chunk;

    let files = collect_files(5000);
    if files.is_empty() { return; }

    let handle = create_v3_index(&files);
    let query = "struct";
    let grep_set = grep_docs_strict(&files, query);
    let v3_result = search_v3(&handle, &files, query, true);

    let fp_docs: Vec<usize> = v3_result.doc_indices.difference(&grep_set).copied().collect();
    eprintln!("\n=== DEBUG struct strict ===");
    eprintln!("grep={} v3={} FP={}", grep_set.len(), v3_result.doc_indices.len(), fp_docs.len());

    let mut dbg = std::fs::File::create("/tmp/v3_debug_struct.txt").unwrap();
    writeln!(dbg, "FP docs for '{}' strict: {} docs\n", query, fp_docs.len()).ok();

    for &doc_idx in &fp_docs {
        let (path, content) = &files[doc_idx];
        writeln!(dbg, "--- doc={doc_idx} path={path} ---").ok();

        // Show highlights for this doc
        for &(fidx, bf, bt) in &v3_result.highlights {
            if fidx != doc_idx { continue; }
            let bf_c = bf.min(content.len());
            let bt_c = bt.min(content.len());
            let ctx_s = snap_back(content, bf_c.saturating_sub(40));
            let ctx_e = snap_fwd(content, (bt_c + 40).min(content.len()));
            let actual_bytes = &content[bf_c..bt_c];
            let actual_lower = actual_bytes.to_lowercase();
            writeln!(dbg, "  highlight [{bf}..{bt}] len={}: ...{}>>{}<<{}...",
                bt - bf,
                &content[ctx_s..bf_c], actual_bytes, &content[bt_c..ctx_e]).ok();
            writeln!(dbg, "    actual_bytes_lower={:?} query={:?} match={}",
                actual_lower, query, actual_lower == query).ok();

            // Tokenize the region around the highlight to show chunks + overlaps
            let region_start = snap_back(content, bf_c.saturating_sub(30));
            let region_end = snap_fwd(content, (bt_c + 30).min(content.len()));
            let region = &content[region_start..region_end];
            let chunks = segment_and_chunk(region, 8);
            writeln!(dbg, "    tokenization of [{region_start}..{region_end}] ({} chunks):", chunks.len()).ok();
            let mut offset = region_start;
            for (i, (chunk_text, meta)) in chunks.iter().enumerate() {
                let chunk_end = offset + chunk_text.len();
                // Compute overlap
                let ovl = if i + 1 < chunks.len() {
                    let next = &chunks[i + 1].0;
                    let ol = 2.min(next.len());
                    &next[..ol]
                } else { "" };
                let extended = format!("{}{}", chunk_text, ovl);
                let in_hl = offset < bt_c && chunk_end > bf_c;
                let marker = if in_hl { ">>>" } else { "   " };
                writeln!(dbg, "      {marker} chunk[{i}] byte=[{offset}..{chunk_end}] content_len={} sep_len={} word_id={} text={:?} extended={:?}",
                    meta.content_len, meta.sep_len, meta.word_id,
                    chunk_text, extended).ok();
                offset += chunk_text.len();
            }
        }
        writeln!(dbg).ok();
    }
    eprintln!("Debug output: /tmp/v3_debug_struct.txt");
}
