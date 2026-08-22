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
use ld_lucivy::suffix_fst::briques::profile;

const DEFAULT_REPO_PATH: &str = "/tmp/rag3db-bench";

/// Corpus root. Override with `V3_CORPUS=/path/to/tree` to run the same checks at
/// a different scale — 5k documents is small enough that timings and error counts
/// say little about behaviour on a real index.
fn repo_path() -> String {
    std::env::var("V3_CORPUS").unwrap_or_else(|_| DEFAULT_REPO_PATH.to_string())
}

/// Cap on documents collected. `V3_MAX_DOCS` overrides the per-test default.
fn max_docs(default: usize) -> usize {
    std::env::var("V3_MAX_DOCS").ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
/// Parse `V3_QUERIES` into `(value, strict_separators)` pairs.
///
/// Accepts `value`, `value:strict` or `value:relax`, comma separated. Shared by
/// every bench in this file so one spec drives them all — the sharding bench used
/// to carry its own rag3db-flavoured list, which measures nothing on a kernel tree.
///
/// The values come from the environment and are handed to APIs wanting `&'static
/// str`, so they are leaked: a handful of strings in a test binary.
fn query_spec() -> Option<Vec<(&'static str, bool)>> {
    std::env::var("V3_QUERIES").ok().map(|spec| {
        spec.split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|item| {
                let (value, strict) = match item.rsplit_once(':') {
                    Some((v, "relax")) => (v.trim(), false),
                    Some((v, "strict")) => (v.trim(), true),
                    _ => (item, true),
                };
                let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
                (leaked, strict)
            })
            .collect()
    })
}

/// Documents per commit, i.e. the knob that sets the segment count.
fn commit_every(default: usize) -> usize {
    std::env::var("V3_COMMIT_EVERY").ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

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

fn collect_files(max_docs_arg: usize) -> Vec<(String, String)> {
    let root_owned = repo_path();
    let root = std::path::Path::new(&root_owned);
    let max_docs = max_docs(max_docs_arg);
    if !root.exists() {
        eprintln!("Skipping: corpus not found at {root_owned}");
        eprintln!("  git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git {DEFAULT_REPO_PATH}");
        eprintln!("  or set V3_CORPUS=/path/to/another/tree");
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

/// Merge parameters, read once from the environment.
///
/// `target` is a floor, not a goal to beat: segment count is the prescan's only
/// unit of parallelism, so merging past it is a pessimisation. Measured on 50k
/// kernel docs, target 32 overshot to 7 segments and `__init` went from 9.3s to
/// 18 minutes.
fn merge_params() -> Option<(usize, usize, bool)> {
    if std::env::var("V3_MERGE").is_err() { return None; }
    let target = std::env::var("V3_MERGE_TARGET").ok()
        .and_then(|v| v.parse().ok()).filter(|&n: &usize| n > 0).unwrap_or(1);
    let group = std::env::var("V3_MERGE_GROUP").ok()
        .and_then(|v| v.parse().ok()).filter(|&n: &usize| n > 1).unwrap_or(8);
    // Merge as we index instead of once at the end. Nothing in the engine
    // triggers merges on commit — segment_updater_actor::handle_commit defers
    // them to an explicit start_merge() — so if the harness does not ask
    // during the loop, every merge lands after the last document.
    let progressive = std::env::var("V3_MERGE_AT_END").is_err();
    Some((target, group, progressive))
}

/// Run one merge round over `ids`, reducing the count towards `target` without
/// ever going below it. Returns false when nothing is left to do.
///
/// A group of `k` segments removes `k - 1` of them, so the number of groups is
/// chosen from the excess rather than from the total. The previous version
/// chunked the whole list by a fixed size and divided the count by 8 every
/// round, which is how a target of 32 became 7.
fn merge_round(
    w: &mut ld_lucivy::IndexWriter,
    ids: &[ld_lucivy::index::SegmentId],
    target: usize,
    group: usize,
) -> bool {
    let len = ids.len();
    if len <= target { return false; }
    let excess = len - target;
    let k = group.min(excess + 1).max(2);
    if k > len { return false; }
    let groups = (excess / (k - 1)).min(len / k);
    if groups == 0 { return false; }
    for g in ids[..groups * k].chunks(k) {
        w.merge(g).unwrap();
    }
    true
}


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

    // Documents per commit. Each commit closes segments, so this is the direct knob
    // on segment count — and segment count is the unit of prescan parallelism, so it
    // drives query latency more than anything else measured today.
    let commit_every: usize = std::env::var("V3_COMMIT_EVERY").ok()
        .and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(500);

    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        // NoMergePolicy by default: it keeps the corpus split into many small
        // segments — the worst case for per-segment cost, and the only shape v3
        // could take while merges were refused. `V3_MERGE=1` keeps the handle's
        // default LogMergePolicy instead, which is what a real index looks like.
        if std::env::var("V3_MERGE").is_err() {
            w.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        }
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            w.add_document(doc).unwrap();
            if (i + 1) % commit_every == 0 {
                w.commit().unwrap();
                // Keep the segment count bounded as we go. Left to the end, this
                // same work merges a much larger pile in one burst — which is what
                // made every previous run finish with a multi-minute merge.
                if let Some((target, group, true)) = merge_params() {
                    let ids = handle.index.searchable_segment_ids().unwrap();
                    if ids.len() > target.saturating_mul(2) {
                        if merge_round(w, &ids, target, group) {
                            w.commit().unwrap();
                        }
                    }
                }
                if (i + 1) % 5000 == 0 || files.len() < 5000 {
                    eprintln!("  indexed {}/{}", i + 1, files.len());
                }
            }
        }
        w.commit().unwrap();
    }

    // Drive the merges ourselves, in bounded tiers.
    //
    // Nothing consults the merge policy automatically: segment_updater_actor
    // defers merges to an explicit drain_merges()/start_merge() "to avoid thread
    // starvation during commit", and drain_merges only WAITS for merges already
    // in flight. So an index built through LucivyHandle never fuses anything —
    // which is why every measurement so far ran on 800 segments.
    //
    // Merging all of them at once is the one thing no policy would do, and it
    // showed: 18 GB re-indexing, 30 GB remapping. max_docs_before_merge exists to
    // bound exactly that, so respect it and merge in groups.
    if let Some((target, group, _)) = merge_params() {
        let t = std::time::Instant::now();
        handle.reader.reload().unwrap();
        let before = handle.reader.searcher().segment_readers().len();

        let mut round = 0;
        loop {
            let ids = handle.index.searchable_segment_ids().unwrap();
            if ids.len() <= target || round > 24 { break; }
            let merged = {
                let mut guard = handle.writer.lock().unwrap();
                let w = guard.as_mut().unwrap();
                let did = merge_round(w, &ids, target, group);
                if did { w.commit().unwrap(); }
                did
            };
            if !merged { break; }
            handle.reader.reload().unwrap();
            let now = handle.reader.searcher().segment_readers().len();
            eprintln!("    merge round {round}: -> {now} segments ({:.1}s)",
                t.elapsed().as_secs_f64());
            round += 1;
        }

        {
            let mut guard = handle.writer.lock().unwrap();
            if let Some(w) = guard.take() { w.wait_merging_threads().unwrap(); }
        }
        handle.reader.reload().unwrap();
        let after = handle.reader.searcher().segment_readers().len();
        eprintln!("  merge (tiered, groups of {group}, target {target}): {before} -> {after} segments in {:.1}s",
            t.elapsed().as_secs_f64());
    }

    // Search executor. Note this changes very little for SFX queries: all the work
    // happens in Query::weight (prescan), which runs BEFORE executor.map — measured
    // 1.0x on 80 segments / 24 threads. Exposed so that can be re-checked, not
    // because it is expected to help.
    if let Some(n) = std::env::var("V3_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
        if n > 1 {
            let mut idx = handle.index.clone();
            idx.set_multithread_executor(n).unwrap();
            eprintln!("  search executor: {n} threads");
        }
    }

    handle.reader.reload().unwrap();
    // Shape of what we are timing: 1 shard, single-thread search executor
    // (Index::search_executor defaults to Executor::single_thread and nothing here
    // overrides it), NoMergePolicy + a commit every 500 docs. Query timings below
    // are therefore a SERIAL walk over every segment, with no parallelism at all.
    eprintln!("  index shape: 1 shard, {} segments, single-thread executor",
        handle.reader.searcher().segment_readers().len());
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

/// Result cap for the ground-truth searches.
///
/// A fixed 10_000 silently turned every high-recall query into a failure the
/// moment the corpus grew: at 50k kernel documents `include` has 36824 true hits
/// and the run reported "36824 vs 10000 FAIL" — a truncation, not a defect.
/// Defaults to the corpus size, i.e. no cap. `V3_LIMIT` overrides it.
fn result_limit(files: &[(String, String)]) -> usize {
    std::env::var("V3_LIMIT").ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| files.len().max(1))
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
    let collector = ld_lucivy::collector::TopDocs::with_limit(result_limit(files)).order_by_score();
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
    // Queries must actually occur in the corpus under test: half the rag3db set
    // (rag3db, std::unique_ptr, ku_dynamic_cast) returns 0 hits on the kernel, which
    // measures nothing. `V3_QUERIES` takes a comma-separated list of `value` or
    // `value:relax` entries.
    let custom_queries: Option<Vec<GroundTruthQuery>> = std::env::var("V3_QUERIES").ok()
        .map(|spec| spec.split(',').map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|item| {
                // GroundTruthQuery holds &'static str; these come from the
                // environment, so leak them — a handful of strings in a test binary.
                let item = item.trim();
                let (value, strict) = match item.rsplit_once(':') {
                    Some((v, "relax")) => (v.trim(), false),
                    Some((v, "strict")) => (v.trim(), true),
                    _ => (item, true),
                };
                let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
                if strict { GroundTruthQuery::strict(leaked) }
                else { GroundTruthQuery::relaxed(leaked) }
            })
            .collect());

    let default_queries: Vec<GroundTruthQuery> = vec![
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

    let queries = custom_queries.unwrap_or(default_queries);
    eprintln!("  {} queries, result cap {}", queries.len(), result_limit(&files));

    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut fail_entries: Vec<serde_json::Value> = Vec::new();
    let diag_mode = std::env::var("V3_DIAG").is_ok();

    eprintln!("{:<35} {:>5} {:>8} {:>8} {:>8}", "Query", "Mode", "Grep", "V3", "Status");
    eprintln!("{}", "-".repeat(70));

    for q in &queries {
        let mode_label = if q.strict_sep { "strict" } else { "relax" };
        // Time the two independently. They used to share one timer, so every
        // reported latency silently carried a full grep over the corpus — a
        // constant that dilutes any engine-side comparison.
        let t_grep = std::time::Instant::now();
        let grep_set = if q.strict_sep {
            grep_docs_strict(&files, q.text)
        } else {
            grep_docs_relaxed(&files, q.text)
        };
        let grep_ms = t_grep.elapsed().as_secs_f64() * 1000.0;

        profile::reset();
        let t = std::time::Instant::now();
        let v3_result = search_v3(&handle, &files, q.text, q.strict_sep);
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let status = if v3_result.doc_indices == grep_set { "OK" } else { "FAIL" };
        eprintln!("{:<35} {:>5} {:>8} {:>8} {:>6} ({:.1}ms v3, {:.1}ms grep)",
            q.text, mode_label, grep_set.len(), v3_result.doc_indices.len(), status, ms, grep_ms);
        if std::env::var("V3_PROFILE").is_ok() {
            eprint!("{}", profile::dump());
        }

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

                        // 5. Wider scan: search with shorter prefix to find variants
                        let short_prefix = if lower_q.len() > 4 { &lower_q[..lower_q.len()-2] } else { &lower_q };
                        let wide_cands = fst_walk::fst_candidates_v3(&sfx_reader, short_prefix, false, strict);
                        let mut wide_with_doc: Vec<serde_json::Value> = Vec::new();
                        for c in &wide_cands {
                            let postings = pr.resolve(c.raw_ordinal);
                            if postings.iter().any(|pe| pe.doc_id == local_doc_id) {
                                // This ordinal has our doc! What's its text?
                                let doc_postings: Vec<_> = postings.iter()
                                    .filter(|pe| pe.doc_id == local_doc_id)
                                    .map(|pe| serde_json::json!({
                                        "pos": pe.position, "bf": pe.byte_from, "bt": pe.byte_to,
                                    }))
                                    .collect();
                                wide_with_doc.push(serde_json::json!({
                                    "ordinal": c.raw_ordinal,
                                    "sti": c.sti,
                                    "own_len": c.own_len,
                                    "sep_len": c.sep_len,
                                    "overlap_len": c.overlap_len,
                                    "ws": c.is_word_start,
                                    "postings_for_doc": doc_postings,
                                }));
                            }
                        }
                        doc_forensic["wider_prefix"] = serde_json::json!(short_prefix);
                        doc_forensic["wider_candidates_with_fn_doc"] = serde_json::json!(wide_with_doc);

                        // 6. Reverse scan: find ALL ordinals that have doc 30
                        //    by scanning sfxpost sequentially (brute force but definitive)
                        let sfxpost_bytes = seg_reader.sfx_index_file("sfxpost", content_f)
                            .and_then(|fs| fs.read_bytes().ok())
                            .map(|b| b.as_ref().to_vec());
                        if let Some(ref _spb) = sfxpost_bytes {
                            // Scan ordinals 0..max_ord for doc_id == local_doc_id
                            // Use posting resolver — try a reasonable range
                            let max_ord = wide_cands.iter()
                                .map(|c| c.raw_ordinal).max().unwrap_or(0) + 1000;
                            let mut doc_ordinals: Vec<serde_json::Value> = Vec::new();
                            for ord in 0..max_ord.min(100_000) {
                                let postings = pr.resolve(ord);
                                if postings.iter().any(|pe| pe.doc_id == local_doc_id) {
                                    // Found! Get the byte range to see what text this ordinal covers
                                    let doc_entries: Vec<_> = postings.iter()
                                        .filter(|pe| pe.doc_id == local_doc_id)
                                        .map(|pe| {
                                            let bf = pe.byte_from as usize;
                                            let bt = pe.byte_to as usize;
                                            let text = if bt <= content.len() && bf < bt {
                                                content[bf..bt].to_string()
                                            } else {
                                                format!("[{bf}..{bt} out of range]")
                                            };
                                            serde_json::json!({
                                                "pos": pe.position,
                                                "bf": pe.byte_from,
                                                "bt": pe.byte_to,
                                                "text": text,
                                            })
                                        })
                                        .collect();
                                    doc_ordinals.push(serde_json::json!({
                                        "ordinal": ord,
                                        "entries": doc_entries,
                                    }));
                                }
                            }
                            doc_forensic["all_ordinals_with_fn_doc"] = serde_json::json!(doc_ordinals);
                            doc_forensic["all_ordinals_count"] = serde_json::json!(doc_ordinals.len());
                            // Filter to ordinals whose text contains the query
                            let matching: Vec<_> = doc_ordinals.iter()
                                .filter(|o| {
                                    o["entries"].as_array().unwrap().iter().any(|e| {
                                        e["text"].as_str().unwrap_or("").to_lowercase().contains(&lower_q)
                                    })
                                })
                                .cloned()
                                .collect();
                            doc_forensic["ordinals_with_query_in_text"] = serde_json::json!(matching);
                        }

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

// ─── Fuzzy / Regex baseline ─────────────────────────────────────────────

fn search_v3_fuzzy(
    handle: &LucivyHandle,
    files: &[(String, String)],
    value: &str,
    distance: u8,
) -> SearchResult {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        distance: Some(distance),
        strict_separators: Some(false),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(result_limit(files)).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();

    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut doc_indices = HashSet::new();
    let highlights = Vec::new();
    for (_, addr) in &results {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let file_idx = doc.field_values()
            .find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64())
            .unwrap_or(0) as usize;
        doc_indices.insert(file_idx);
    }
    SearchResult { doc_indices, highlights }
}

fn search_v3_regex(
    handle: &LucivyHandle,
    files: &[(String, String)],
    pattern: &str,
) -> SearchResult {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(pattern.into()),
        regex: Some(true),
        strict_separators: Some(false),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(result_limit(files)).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();

    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut doc_indices = HashSet::new();
    let highlights = Vec::new();
    for (_, addr) in &results {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        use ld_lucivy::schema::document::Value;
        let file_idx = doc.field_values()
            .find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64())
            .unwrap_or(0) as usize;
        doc_indices.insert(file_idx);
    }
    SearchResult { doc_indices, highlights }
}

/// Semi-global Levenshtein substring match: find if `pattern` appears as a
/// fuzzy substring of `text` within edit distance `max_d`.
/// O(n × m) where n = text length, m = pattern length.
/// Uses free-start DP (curr[0] = 0) so the pattern can start anywhere.
fn fuzzy_substring_exists(text: &[u8], pattern: &[u8], max_d: u32) -> bool {
    let m = pattern.len();
    if m == 0 { return true; }
    let n = text.len();
    if n == 0 { return false; }
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    for i in 1..=n {
        let mut curr = vec![0u32; m + 1];
        curr[0] = 0; // free prefix — match can start anywhere
        for j in 1..=m {
            let cost = if text[i - 1] == pattern[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1)
                .min(prev[j] + 1)
                .min(prev[j - 1] + cost);
        }
        if curr[m] <= max_d {
            return true; // early exit — found a match
        }
        prev = curr;
    }
    false
}

/// Fuzzy grep using semi-global Levenshtein on lowercased text.
/// Strips separators from both query and text (sep-agnostic matching).
fn grep_docs_fuzzy(files: &[(String, String)], needle: &str, max_distance: u8) -> HashSet<usize> {
    let pattern: Vec<u8> = needle.to_lowercase()
        .chars().filter(|c| is_content_char(*c))
        .collect::<String>().into_bytes();

    files.iter().enumerate()
        .filter(|(_, (_, content))| {
            // Strip non-content chars and lowercase — same as what the index does
            let text: Vec<u8> = content.to_lowercase()
                .chars().filter(|c| is_content_char(*c))
                .collect::<String>().into_bytes();
            fuzzy_substring_exists(&text, &pattern, max_distance as u32)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Naive regex grep: find docs matching the pattern (case-insensitive).
fn grep_docs_regex(files: &[(String, String)], pattern: &str) -> HashSet<usize> {
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("invalid regex pattern");
    files.iter().enumerate()
        .filter(|(_, (_, content))| re.is_match(content))
        .map(|(i, _)| i)
        .collect()
}

/// Ground truth cache for fuzzy/regex — avoids recomputing slow grep each run.
/// Ground-truth cache. Keyed by corpus and size: a cache computed on one tree is
/// meaningless for another, and silently reusing it would fake a green run.
fn gt_cache_path() -> String {
    let key: String = repo_path().chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!("/tmp/v3_fuzzy_regex_gt_{}_{}.json", key, max_docs(500))
}

/// Compute or load cached ground truth for fuzzy/regex queries.
fn load_or_compute_ground_truth(
    files: &[(String, String)],
    queries: &[(&str, &str, &str)], // (value, type "fz1"|"regex", label)
) -> HashMap<String, Vec<usize>> {
    // Try loading cache
    if let Ok(data) = std::fs::read_to_string(gt_cache_path()) {
        if let Ok(cache) = serde_json::from_str::<HashMap<String, Vec<usize>>>(&data) {
            // Validate: cache has all queries and was built with same file count
            let meta_key = "__meta_file_count__".to_string();
            if let Some(count) = cache.get(&meta_key) {
                if count.first().copied() == Some(files.len()) {
                    let all_present = queries.iter().all(|(v, t, _)| {
                        cache.contains_key(&format!("{t}:{v}"))
                    });
                    if all_present {
                        eprintln!("  Ground truth loaded from cache ({})", gt_cache_path());
                        return cache;
                    }
                }
            }
        }
    }

    eprintln!("  Computing ground truth (will cache to {})...", gt_cache_path());
    let mut cache: HashMap<String, Vec<usize>> = HashMap::new();
    cache.insert("__meta_file_count__".into(), vec![files.len()]);

    for &(value, qtype, label) in queries {
        let key = format!("{qtype}:{value}");
        let t = std::time::Instant::now();
        let docs: Vec<usize> = if qtype == "fz1" {
            grep_docs_fuzzy(files, value, 1).into_iter().collect()
        } else {
            grep_docs_regex(files, value).into_iter().collect()
        };
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!("    {label:<25} {qtype:>5} -> {} docs ({:.0}ms)", docs.len(), ms);
        cache.insert(key, docs);
    }

    // Save cache
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        std::fs::write(gt_cache_path(), json).ok();
    }
    cache
}

/// Baseline test for fuzzy and regex — report only, no assert.
///
/// Env vars:
///   QUERY=retrun        — run only this query
///   MODE=fuzzy|regex    — run only fuzzy or regex queries
///   RECOMPUTE_GT=1      — force recompute ground truth cache
///
/// Run: cargo test -p lucivy-core --test test_sfx_v3_ground_truth baseline_fuzzy_regex -- --nocapture
#[test]
fn baseline_fuzzy_regex() {
    let query_filter = std::env::var("QUERY").ok();
    let mode_filter = std::env::var("MODE").ok();

    // Delete cache if forced
    if std::env::var("RECOMPUTE_GT").is_ok() {
        std::fs::remove_file(gt_cache_path()).ok();
    }

    let files = collect_files(500);
    if files.is_empty() { return; }
    eprintln!("\n=== Fuzzy/Regex Baseline: {} files ===\n", files.len());

    // All queries: (value, type, label)
    let all_queries: Vec<(&str, &str, &str)> = vec![
        ("functin",     "fz1",   "functin"),
        ("strcuture",   "fz1",   "strcuture"),
        ("inclde",      "fz1",   "inclde"),
        ("retrun",      "fz1",   "retrun"),
        ("rag3db",      "fz1",   "rag3db"),
        ("uint64",      "fz1",   "uint64"),
        (r#"function\s*\("#,       "regex", "func call"),
        (r#"uint\d+_t"#,          "regex", "uint types"),
        (r#"std::\w+"#,           "regex", "std namespace"),
        (r#"#include\s*[<"]"#,    "regex", "include dir"),
        (r#"Table\w+Function"#,   "regex", "Table*Func"),
    ];

    // Filter queries by env vars
    let queries: Vec<(&str, &str, &str)> = all_queries.iter()
        .filter(|(v, t, label)| {
            if let Some(ref q) = query_filter {
                return *v == q.as_str() || *label == q.as_str();
            }
            if let Some(ref m) = mode_filter {
                return (m == "fuzzy" && *t == "fz1") || (m == "regex" && *t == "regex");
            }
            true
        })
        .copied()
        .collect();

    if queries.is_empty() {
        eprintln!("No queries match filter. Available: {:?}",
            all_queries.iter().map(|(v,_,_)| *v).collect::<Vec<_>>());
        return;
    }

    // Step 1: compute/load ground truth (slow part — cached)
    let t_gt = std::time::Instant::now();
    let gt_cache = load_or_compute_ground_truth(&files, &all_queries);
    eprintln!("  Ground truth: {:.1}s\n", t_gt.elapsed().as_secs_f64());

    // Step 2: index (independent of GT)
    let t0 = std::time::Instant::now();
    let handle = create_v3_index(&files);
    eprintln!("  Index time: {:.1}s\n", t0.elapsed().as_secs_f64());

    eprintln!("  Ready — running V3 queries:\n");

    eprintln!("{:<25} {:>5} {:>6} {:>6} {:>4} {:>4} {:>8} {:>8}",
        "Query", "Type", "Grep", "V3", "FN", "FP", "V3 ms", "Status");
    eprintln!("{}", "-".repeat(80));

    let mut total = 0u32;
    let mut pass = 0u32;

    for &(value, qtype, label) in &queries {
        let key = format!("{qtype}:{value}");
        let grep_set: HashSet<usize> = gt_cache.get(&key)
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();

        let t = std::time::Instant::now();
        let v3_result = if qtype == "fz1" {
            search_v3_fuzzy(&handle, &files, value, 1)
        } else {
            search_v3_regex(&handle, &files, value)
        };
        let v3_ms = t.elapsed().as_secs_f64() * 1000.0;

        let fn_count = grep_set.difference(&v3_result.doc_indices).count();
        let fp_count = v3_result.doc_indices.difference(&grep_set).count();
        let status = if fn_count == 0 && fp_count == 0 { "OK" } else { "DIFF" };
        if fn_count == 0 && fp_count == 0 { pass += 1; }
        total += 1;

        eprintln!("{:<25} {:>5} {:>6} {:>6} {:>4} {:>4} {:>7.0} {:>8}",
            label, qtype, grep_set.len(), v3_result.doc_indices.len(),
            fn_count, fp_count, v3_ms, status);

        // Show FN/FP details
        if fn_count > 0 && fn_count <= 10 {
            let fns: Vec<usize> = grep_set.difference(&v3_result.doc_indices).copied().collect();
            for idx in &fns {
                eprintln!("  FN doc {}: {}", idx, files[*idx].0);
            }
        }
        if fp_count > 0 && fp_count <= 10 {
            let fps: Vec<usize> = v3_result.doc_indices.difference(&grep_set).copied().collect();
            for idx in &fps {
                eprintln!("  FP doc {}: {}", idx, files[*idx].0);
            }
        }
        if fp_count > 10 || fn_count > 10 {
            eprintln!("  ({} FN + {} FP — too many to show inline)", fn_count, fp_count);
        }

        // Export JSON for investigation
        if fn_count > 0 || fp_count > 0 {
            let fns: Vec<usize> = grep_set.difference(&v3_result.doc_indices).copied().collect();
            let fps: Vec<usize> = v3_result.doc_indices.difference(&grep_set).copied().collect();
            let safe_label = label.replace(|c: char| !c.is_alphanumeric(), "_");
            let path = format!("/tmp/v3_baseline_{}_{}.json", safe_label, qtype);
            let json = serde_json::json!({
                "query": value,
                "type": qtype,
                "label": label,
                "grep_count": grep_set.len(),
                "v3_count": v3_result.doc_indices.len(),
                "fn_count": fn_count,
                "fp_count": fp_count,
                "fn_docs": fns.iter().map(|&i| serde_json::json!({"idx": i, "path": &files[i].0})).collect::<Vec<_>>(),
                "fp_docs": fps.iter().map(|&i| serde_json::json!({"idx": i, "path": &files[i].0})).collect::<Vec<_>>(),
            });
            std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).ok();
            eprintln!("  → {path}");
        }
    }

    eprintln!("\n{pass}/{total} pass (baseline, no assert)");
}

/// PERF SHAPE — same index, same queries, only the search executor changes.
///
/// The ground truth test above runs 1 shard / 80 segments / single-thread, which
/// makes its timings a SERIAL walk over every segment. This isolates how much of
/// that is just missing parallelism, on the exact same searcher, so the numbers
/// are comparable line by line. Report only, no assertions.
///
/// Run: cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth
///      perf_shape_executor -- --nocapture
#[test]
fn perf_shape_executor() {
    use ld_lucivy::Executor;
    use ld_lucivy::query::{Bm25StatisticsProvider, EnableScoring};

    let files = collect_files(5000);
    if files.is_empty() { return; }

    let handle = create_v3_index(&files);
    let searcher = handle.reader.searcher();
    let n_seg = searcher.segment_readers().len();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);

    let single = Executor::single_thread();
    let multi = Executor::multi_thread(threads, "gt-perf-").unwrap();

    eprintln!("\n=== Executor shape: {n_seg} segments, {threads} threads ===\n");
    eprintln!("{:<28} {:>7} {:>10} {:>10} {:>8}", "Query", "Mode", "1 thread", "N threads", "speedup");
    eprintln!("{}", "-".repeat(68));

    let queries: [(&str, bool); 6] = [
        ("function", true), ("function", false),
        ("include", true),
        ("uint64_t", true), ("uint64_t", false),
        ("std::unique_ptr", false),
    ];

    for (value, strict) in queries.iter().copied() {
        let run = |exec: &Executor| -> (u128, usize) {
            let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
            let config = QueryConfig {
                query_type: "contains".into(),
                field: Some("content".into()),
                value: Some(value.into()),
                strict_separators: Some(strict),
                ..Default::default()
            };
            let query = query::build_query(&config, &handle.schema, &handle.index, Some(sink)).unwrap();
            let collector = ld_lucivy::collector::TopDocs::with_limit(10_000).order_by_score();
            let stats: Arc<dyn Bm25StatisticsProvider + Send + Sync> = Arc::new(searcher.clone());
            let scoring = EnableScoring::enabled_from_statistics_provider(stats, &searcher);
            let t = std::time::Instant::now();
            let res = searcher.search_with_executor(&*query, &collector, exec, scoring).unwrap();
            (t.elapsed().as_millis(), res.len())
        };

        let (ms1, n1) = run(&single);
        let (msn, nn) = run(&multi);
        assert_eq!(n1, nn, "executor changed the result set for '{value}'");
        let speedup = if msn > 0 { ms1 as f64 / msn as f64 } else { f64::NAN };
        eprintln!("{:<28} {:>7} {:>9}ms {:>9}ms {:>7.1}x",
            value, if strict { "strict" } else { "relax" }, ms1, msn, speedup);
    }
    eprintln!();
}

/// PERF SHAPE — sharding, the only parallelism that actually applies here.
///
/// ContainsQueryV3::weight() calls prescan_segments, which walks every segment
/// SERIALLY and does all the SFX work there. In search_with_executor, weight() runs
/// BEFORE executor.map, so a multi-thread executor parallelises the phase that costs
/// nothing (measured: 1.0x, see perf_shape_executor). Sharding is different: each
/// shard is its own index with its own weight(), so the prescan itself splits.
///
/// Same corpus, same RAM storage, same queries. Report only, no assertions.
///
/// Run: cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth
///      perf_shape_sharded -- --nocapture
#[test]
fn perf_shape_sharded() {
    use lucivy_core::sharded_handle::{ShardedHandle, RamShardStorage};

    let files = collect_files(5000);
    if files.is_empty() { return; }

    let queries: Vec<(&str, bool)> = query_spec().unwrap_or_else(|| vec![
        ("function", true), ("function", false),
        ("include", true),
        ("uint64_t", false),
        ("std::unique_ptr", false),
    ]);

    eprintln!("\n=== Sharding shape: {} docs ===\n", files.len());
    // Shard counts to compare. `V3_SHARDS=1,4,8,16` overrides.
    let shard_counts: Vec<usize> = std::env::var("V3_SHARDS").ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![1, 4, 8]);

    eprintln!("  shard counts: {shard_counts:?}");
    eprintln!("{}", "-".repeat(78));

    let build = |n: usize| -> ShardedHandle {
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "path", "type": "text", "stored": true},
                {"name": "content", "type": "text", "stored": true}
            ],
            "sfx_version": 3,
            "shards": n
        })).unwrap();
        let h = ShardedHandle::create_with_storage(
            Box::new(RamShardStorage::new()), &config).unwrap();
        let path_f = h.field("path").unwrap();
        let content_f = h.field("content").unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            h.add_document(doc, i as u64).unwrap();
            if (i + 1) % commit_every(500) == 0 { h.commit().unwrap(); }
        }
        h.commit().unwrap();
        h
    };

    let t = std::time::Instant::now();
    let handles: Vec<(usize, ShardedHandle)> = shard_counts.iter()
        .map(|&n| (n, build(n)))
        .collect();
    eprintln!("  (index build: {:.1}s total)\n", t.elapsed().as_secs_f64());

    for (value, strict) in queries.iter().copied() {
        let run = |h: &ShardedHandle| -> (u128, usize) {
            let config = QueryConfig {
                query_type: "contains".into(),
                field: Some("content".into()),
                value: Some(value.into()),
                strict_separators: Some(strict),
                ..Default::default()
            };
            let t = std::time::Instant::now();
            let res = h.search(&config, 10_000, None).unwrap();
            (t.elapsed().as_millis(), res.len())
        };
        let measured: Vec<(usize, u128, usize)> = handles.iter()
            .map(|(n, h)| { let (ms, hits) = run(h); (*n, ms, hits) })
            .collect();
        let base = measured[0].1;
        let cells: Vec<String> = measured.iter()
            .map(|(n, ms, _)| format!("{n}sh {ms}ms"))
            .collect();
        let hits: Vec<String> = measured.iter().map(|(_, _, h)| h.to_string()).collect();
        let last = measured.last().unwrap().1;
        let gain = if last > 0 { base as f64 / last as f64 } else { f64::NAN };
        eprintln!("{:<22} {:>7}  {}  gain {:.1}x  hits {}",
            value, if strict { "strict" } else { "relax" },
            cells.join("  "), gain, hits.join("/"));
    }
    eprintln!();
}

/// Distributed v3: two independent nodes, stats exported / merged / injected.
///
/// The multi-machine path (export_stats -> ExportableStats::merge ->
/// search_with_global_stats) is only exercised in acid_postgres.rs, which is
/// #[ignore] by default, needs a Postgres, and does not set sfx_version — so it
/// runs v2. v3 over that path had never been executed, exactly like v3 over
/// sharding. This runs it in RAM, no external service.
///
/// What it must prove: the union of what the two nodes return equals what a single
/// node holding all the documents returns. Scores may differ (that is the point of
/// global stats), the document SET must not.
#[test]
fn v3_distributed_two_nodes() {
    use lucivy_core::sharded_handle::{ShardedHandle, RamShardStorage};
    use lucivy_core::bm25_global::ExportableStats;

    // Enough files that both halves actually contain source code: the first few
    // hundred entries of the corpus are datasets and licences, and a green run on a
    // corpus with 5 hits proves nothing.
    let files = collect_files(3000);
    if files.is_empty() { return; }
    // Interleave rather than split in half, so neither node ends up with only the
    // non-code prefix of the walk order.
    let left: Vec<_> = files.iter().step_by(2).cloned().collect();
    let right: Vec<_> = files.iter().skip(1).step_by(2).cloned().collect();
    let (left, right) = (&left[..], &right[..]);

    let build = |docs: &[(String, String)], shards: usize| -> ShardedHandle {
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "path", "type": "text", "stored": true},
                {"name": "content", "type": "text", "stored": true}
            ],
            "sfx_version": 3,
            "shards": shards
        })).unwrap();
        let h = ShardedHandle::create_with_storage(
            Box::new(RamShardStorage::new()), &config).unwrap();
        let path_f = h.field("path").unwrap();
        let content_f = h.field("content").unwrap();
        for (i, (path, content)) in docs.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            h.add_document(doc, i as u64).unwrap();
        }
        h.commit().unwrap();
        h
    };

    let node_a = build(left, 2);
    let node_b = build(right, 2);
    let node_all = build(&files, 4);

    for (value, strict) in [("function", true), ("uint64_t", false), ("std::unique_ptr", false)] {
        let query = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(value.into()),
            strict_separators: Some(strict),
            ..Default::default()
        };

        // Coordinator: gather stats, round-trip through JSON like a real network hop.
        let sa = node_a.export_stats(&query).unwrap();
        let sb = node_b.export_stats(&query).unwrap();
        let sa: ExportableStats = serde_json::from_str(&serde_json::to_string(&sa).unwrap()).unwrap();
        let sb: ExportableStats = serde_json::from_str(&serde_json::to_string(&sb).unwrap()).unwrap();
        let global = ExportableStats::merge(&[sa, sb]);

        let ra = node_a.search_with_global_stats(&query, 10_000, &global, None).unwrap();
        let rb = node_b.search_with_global_stats(&query, 10_000, &global, None).unwrap();
        let distributed = ra.len() + rb.len();

        let single = node_all.search(&query, 10_000, None).unwrap().len();

        eprintln!("  {:<18} {:>6} — distributed {} (A {} + B {}) vs single {}",
            value, if strict { "strict" } else { "relax" },
            distributed, ra.len(), rb.len(), single);

        assert_eq!(distributed, single,
            "distributed v3 lost or invented documents for '{value}' \
             (A={} B={} vs single={single})", ra.len(), rb.len());
        assert!(global.total_num_docs >= distributed as u64,
            "merged stats must cover at least the returned docs");
    }
}

/// REGEX v2 vs v3, same corpus, same patterns, same brute-force reference.
///
/// The project's only real regex test (test_regex_ground_truth.rs) builds its
/// index with `SchemaConfig { ..Default::default() }`, i.e. sfx_version = 2. So
/// regex has never been measured on v3 outside the report-only fuzzy baseline.
/// Running both side by side is what localises the regression.
///
/// Report only, no assertions — this is a diagnostic, and asserting a target we
/// have not chosen yet would just freeze today's behaviour.
///
/// Run: cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth
///      regex_v2_vs_v3 -- --nocapture
#[test]
fn regex_v2_vs_v3() {
    let files = collect_files(2000);
    if files.is_empty() { return; }

    let build = |version: u8| -> LucivyHandle {
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "path", "type": "text", "stored": true},
                {"name": "content", "type": "text", "stored": true}
            ],
            "sfx_version": version
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
                if (i + 1) % 500 == 0 { w.commit().unwrap(); }
            }
            w.commit().unwrap();
        }
        handle.reader.reload().unwrap();
        handle
    };

    let patterns: [&str; 10] = [
        r#"rag3[a-z]+ver"#,      // character class + quantifier
        r#"uint\d+_t"#,          // \d class, one of the v3 FN
        r#"std::\w+"#,           // \w class after separators
        r#"function\s*\("#,      // \s class + literal paren, one of the v3 FN
        r#"#include\s*[<"]"#,    // \s + explicit class, one of the v3 FN
        r#"rag3.*ver"#,          // .* gap
        r#"impl.*fn.*self"#,     // multiple .* gaps
        r#"pub.*struct"#,        // .* common
        r#"Table\w+Function"#,   // \w between literals
        r#"[A-Z][a-z]+Error"#,   // leading class — no anchor literal at all
    ];

    let h2 = build(2);
    let h3 = build(3);

    eprintln!("\n=== Regex v2 vs v3: {} docs ===\n", files.len());
    eprintln!("{:<24} {:>6} {:>14} {:>14}", "Pattern", "Grep", "v2 (FN/FP)", "v3 (FN/FP)");
    eprintln!("{}", "-".repeat(62));

    for pat in patterns {
        // Exact pattern semantics: FST retrieval is case-insensitive (a superset),
        // the verification pass narrows it. Grepping with (?i) would measure a
        // different contract than the one the engine now implements.
        let re = regex::Regex::new(pat).unwrap();
        let truth: HashSet<usize> = files.iter().enumerate()
            .filter(|(_, (_, c))| re.is_match(c))
            .map(|(i, _)| i)
            .collect();

        let run = |h: &LucivyHandle| -> (usize, usize) {
            let got = search_v3_regex(h, &files, pat).doc_indices;
            (truth.difference(&got).count(), got.difference(&truth).count())
        };
        let (fn2, fp2) = run(&h2);
        let (fn3, fp3) = run(&h3);

        let mark = |f: usize, p: usize| if f == 0 && p == 0 { "OK".to_string() }
                   else { format!("{f}/{p}") };
        eprintln!("{:<24} {:>6} {:>14} {:>14}",
            pat, truth.len(), mark(fn2, fp2), mark(fn3, fp3));
    }
    eprintln!();
}
