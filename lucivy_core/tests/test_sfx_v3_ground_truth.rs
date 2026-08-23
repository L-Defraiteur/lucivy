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
/// Marker in the `distance` slot for a regex query.
const RX: u8 = 255;

/// `V3_QUERIES=a:strict,b:relax,c:fz1,d:fz2,e:rx` → (value, strict, distance).
/// Fuzzy is always relaxed (the query is separator-stripped before matching).
fn query_spec() -> Option<Vec<GroundTruthQuery>> {
    std::env::var("V3_QUERIES").ok().map(|spec| {
        spec.split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|item| {
                let (value, mode) = item.rsplit_once(':').unwrap_or((item, "strict"));
                // Whitespace cannot survive the trim: `\s`, `\t`, `\n` stand
                // for a space, a tab, a newline.
                let value = value.trim().replace("\\s", " ").replace("\\t", "\t").replace("\\n", "\n");
                let leaked: &'static str = Box::leak(value.into_boxed_str());
                GroundTruthQuery::from_mode(leaked, mode)
                    .unwrap_or_else(|| panic!("unknown mode {mode:?} in V3_QUERIES item {item:?}"))
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
    if std::env::var("V3_MERGE").is_err() || std::env::var("V3_POLICY").is_ok() { return None; }
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
    // One call for all groups: they are disjoint, and merge_many runs them as
    // concurrent tasks. Calling merge() per group serialised them — 20 groups
    // of ~700ms made a 14s round on a 24-core machine.
    let batch: Vec<Vec<ld_lucivy::index::SegmentId>> =
        ids[..groups * k].chunks(k).map(|g| g.to_vec()).collect();
    w.merge_many(&batch).unwrap();
    true
}


/// Identity of the index a given set of knobs produces.
///
/// Two runs with the same key build byte-identical indexes, so the second can
/// open the first instead of rebuilding it.
fn index_shape_key(num_files: usize) -> String {
    let (target, group, progressive) = match merge_params() {
        Some((t, g, p)) => (t as i64, g as i64, p),
        None => (-1, -1, false),
    };
    format!(
        "corpus={} files={} commit_every={} merge_target={} merge_group={} progressive={} policy={} v=9",
        repo_path(), num_files, commit_every(500), target, group, progressive,
        std::env::var("V3_POLICY").is_ok(),
    )
}

/// Reuse a persisted index when one matching the current knobs is on disk.
///
/// Indexing 50k documents costs 82s and merging them another 190s to 1150s, to
/// measure queries that run in 300ms. `V3_INDEX_DIR=/path` persists the built
/// index there and reopens it on later runs, which is what makes iterating on
/// query performance practical.
///
/// Note this also swaps RamDirectory for MmapDirectory: closer to production,
/// but timings taken with and without the cache are not directly comparable.
fn print_shape(handle: &LucivyHandle) {
    let searcher = handle.reader.searcher();
    let mut sizes: Vec<u32> = searcher.segment_readers().iter().map(|r| r.max_doc()).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let shown: Vec<String> = sizes.iter().take(12).map(|n| n.to_string()).collect();
    eprintln!("  index shape: 1 shard, {} segments (prescan on the luciole pool), docs per segment, largest first: {}{}",
        sizes.len(), shown.join(" "), if sizes.len() > 12 { " …" } else { "" });
}

fn try_reuse_index(files: &[(String, String)]) -> Option<LucivyHandle> {
    let dir_path = std::env::var("V3_INDEX_DIR").ok()?;
    let key_path = std::path::Path::new(&dir_path).join(".v3_shape");
    let want = index_shape_key(files.len());
    if std::fs::read_to_string(&key_path).ok()? != want {
        return None;
    }
    let dir = ld_lucivy::directory::MmapDirectory::open(&dir_path).ok()?;
    let handle = LucivyHandle::open(dir).ok()?;
    let segs = handle.reader.searcher().segment_readers().len();
    eprintln!("  index reused from {dir_path} ({segs} segments) — delete it to force a rebuild");
    print_shape(&handle);
    Some(handle)
}

fn create_v3_index(files: &[(String, String)]) -> LucivyHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": 3
    })).unwrap();

    if let Some(h) = try_reuse_index(files) {
        return h;
    }

    // Always build in RAM. Building straight into an MmapDirectory was tried:
    // every sidecar is fsynced on close, eight finalizes run concurrently, and
    // on btrfs+zstd one fdatasync costs ~65ms — 10k documents took 464s against
    // 5s in RAM. The index is copied out once at the end instead, unsynced; the
    // shape marker written after it is what makes the copy trustworthy.
    let ram_dir = ld_lucivy::directory::RamDirectory::default();
    let handle = LucivyHandle::create(ram_dir.clone(), &config).unwrap();
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
        // The harness drives every merge itself (`merge_round`, `merge_many`),
        // so the writer's policy must stay out of the way: since 23 August
        // the policy fires on commit, and an explicit merge overlapping a
        // running one is refused. `V3_POLICY=1` hands the index to the
        // handle's LogMergePolicy instead and skips the harness merges.
        if std::env::var("V3_POLICY").is_err() {
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
        // Policy merges started by the last commit are still running as
        // scheduler tasks; persisting or querying now reads half-written
        // segment files (measured: SIGSEGV through mmap on the first run).
        w.drain_merges().unwrap();
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

    // Persist once, then stamp the shape last: a key on disk means the index
    // next to it is complete. The handle keeps serving from RAM for this run;
    // the next run reopens the copy through MmapDirectory.
    if let Some(path) = std::env::var("V3_INDEX_DIR").ok() {
        let t = std::time::Instant::now();
        let root = std::path::Path::new(&path);
        let _ = std::fs::remove_dir_all(root);
        ram_dir.persist_unsynced(root).unwrap();
        std::fs::write(root.join(".v3_shape"), index_shape_key(files.len())).unwrap();
        eprintln!("  index persisted to {path} in {:.1}s", t.elapsed().as_secs_f64());
    }

    // Shape of what we are timing: 1 shard, single-thread search executor
    // (Index::search_executor defaults to Executor::single_thread and nothing here
    // overrides it), NoMergePolicy + a commit every 500 docs. Query timings below
    // are therefore a SERIAL walk over every segment, with no parallelism at all.
    print_shape(&handle);
    handle
}

// ─── Ground truth (naive grep) ────────────────────────────────────────────

/// Literal case-insensitive grep (for strict_sep=true).
/// Ground truth doing the engine's exact job: for every file, read from disk,
/// every occurrence of the query as a byte span in the original text.
///
/// Strict: case-insensitive (ASCII folding, which keeps byte offsets exact —
/// the kernel corpus is ASCII; Unicode case changes are out of this scope).
/// Overlapping occurrences are all reported, as the engine's suffix walk does.
///
/// Relaxed: separators do not exist. The file is stripped to its content bytes
/// with a map back to original offsets, the query is searched there, and each
/// hit is mapped back to the span from its first content byte to its last.
///
/// Reading from disk is deliberate: the engine serves from an mmap'd index, so
/// the reference must pay the same page-cache reality, not a pre-loaded Vec.
struct GrepSpans {
    docs: HashSet<usize>,
    spans: HashSet<(usize, usize, usize)>,
}

fn grep_spans(files: &[(String, String)], needle: &str, strict: bool) -> GrepSpans {
    let root = repo_path();
    let root = std::path::Path::new(&root);
    let mut docs = HashSet::new();
    let mut spans = HashSet::new();

    // Unicode lowercase, like the engine (`DÉJÀ` matches `déjà`); separators
    // stripped in relaxed mode. Each byte of the folded text remembers the
    // source offset and length of the char it came from, so spans are
    // reported on the original bytes even where folding changes the length.
    let fold = |text: &str, strip: bool| -> (Vec<u8>, Vec<(usize, usize)>) {
        let mut out = Vec::with_capacity(text.len());
        let mut back = Vec::with_capacity(text.len());
        for (off, ch) in text.char_indices() {
            if strip && !is_content_char(ch) { continue; }
            let n = ch.len_utf8();
            for lc in ch.to_lowercase() {
                let mut buf = [0u8; 4];
                for b in lc.encode_utf8(&mut buf).bytes() {
                    out.push(b);
                    back.push((off, n));
                }
            }
        }
        (out, back)
    };
    let (needle_l, _) = fold(needle, !strict);
    if needle_l.is_empty() { return GrepSpans { docs, spans }; }

    for (i, (rel, _)) in files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(root.join(rel)) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let (hay, back) = fold(&text, !strict);
        let mut hit = false;
        for start in find_all(&hay, &needle_l) {
            let end_idx = start + needle_l.len() - 1;
            let from = back[start].0;
            let (last, n) = back[end_idx];
            spans.insert((i, from, last + n));
            hit = true;
        }
        if hit { docs.insert(i); }
    }
    GrepSpans { docs, spans }
}

/// Fuzzy ground truth: the engine's own occurrence definition
/// (`fuzzy_spans`, one per run of acceptable end offsets) applied to each
/// file read from disk — lowercase, separators stripped, mapped back to
/// source bytes exactly like the relaxed grep.
fn grep_spans_fuzzy(files: &[(String, String)], needle: &str, distance: u8) -> GrepSpans {
    let root = repo_path();
    let root = std::path::Path::new(&root);
    let mut docs = HashSet::new();
    let mut spans = HashSet::new();
    let needle_l: Vec<u8> = strip_seps(&needle.to_lowercase()).into_bytes();
    if needle_l.is_empty() { return GrepSpans { docs, spans }; }

    for (i, (rel, _)) in files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(root.join(rel)) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let mut stripped: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut back: Vec<usize> = Vec::with_capacity(bytes.len());
        for (off, ch) in text.char_indices() {
            if !is_content_char(ch) { continue; }
            for lc in ch.to_lowercase() {
                let mut buf = [0u8; 4];
                for b in lc.encode_utf8(&mut buf).bytes() {
                    stripped.push(b);
                    back.push(off);
                }
            }
        }
        let mut hit = false;
        for (s, e, _) in ld_lucivy::suffix_fst::briques::fuzzy_spans::fuzzy_spans(&needle_l, &stripped, distance as usize) {
            let from = back[s];
            let last = back[e - 1];
            let to = last + text[last..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            spans.insert((i, from, to));
            hit = true;
        }
        if hit { docs.insert(i); }
    }
    GrepSpans { docs, spans }
}

/// Regex ground truth from disk: every non-overlapping, leftmost-first match
/// of the pattern (case-insensitive, as the engine), in source bytes.
fn grep_spans_regex(files: &[(String, String)], pattern: &str) -> GrepSpans {
    let root = repo_path();
    let root = std::path::Path::new(&root);
    let mut docs = HashSet::new();
    let mut spans = HashSet::new();
    let re = regex::RegexBuilder::new(pattern).case_insensitive(true).build()
        .expect("invalid regex pattern");
    for (i, (rel, _)) in files.iter().enumerate() {
        let Ok(bytes) = std::fs::read(root.join(rel)) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let mut hit = false;
        for m in re.find_iter(&text) {
            if m.start() == m.end() { continue; }
            spans.insert((i, m.start(), m.end()));
            hit = true;
        }
        if hit { docs.insert(i); }
    }
    GrepSpans { docs, spans }
}

/// All start offsets of `needle` in `hay`, overlapping included.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() { return out; }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match hay[i..].iter().position(|&b| b == first) {
            None => break,
            Some(p) => {
                let s = i + p;
                if s + needle.len() <= hay.len() && &hay[s..s + needle.len()] == needle {
                    out.push(s);
                }
                i = s + 1;
            }
        }
    }
    out
}

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
    // Relaxed means separators do not exist: the query is a substring of the
    // document with every non-content char removed. That is the whole
    // definition, so that is the whole check.
    //
    // This used to walk a sliding window over words and stop once the window
    // reached twice the query length — which a single long word reaches on its
    // own, before its junction with the next word is ever tested. `maintain its`
    // was never concatenated into `maintainits`, so the `init` straddling the
    // space went unseen, and v3 was charged with a false positive for finding
    // it. Three such "false positives" on the kernel corpus, all the harness.
    let stripped_query = strip_seps(&needle.to_lowercase());
    if stripped_query.is_empty() { return HashSet::new(); }

    files.iter().enumerate()
        .filter(|(_, (_, content))| {
            strip_seps(&content.to_lowercase()).contains(&stripped_query)
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

thread_local! {
    /// Engine-only time of the last search_v3 call (search, no doc fetch).
    static LAST_SEARCH_MS: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

fn search_v3(
    handle: &LucivyHandle,
    files: &[(String, String)],
    value: &str,
    strict_separators: bool,
) -> SearchResult {
    search_v3_d(handle, files, value, strict_separators, 0)
}

fn search_v3_d(
    handle: &LucivyHandle,
    files: &[(String, String)],
    value: &str,
    strict_separators: bool,
    distance: u8,
) -> SearchResult {
    search_v3_q(handle, files, value, strict_separators, distance, false, false)
}

fn search_v3_q(
    handle: &LucivyHandle,
    files: &[(String, String)],
    value: &str,
    strict_separators: bool,
    distance: u8,
    anchor: bool,
    exact: bool,
) -> SearchResult {
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict_separators),
        distance: if distance > 0 && distance != RX { Some(distance) } else { None },
        regex: if distance == RX { Some(true) } else { None },
        anchor_start: if anchor { Some(true) } else { None },
        exact_match: if exact { Some(true) } else { None },
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(result_limit(files)).order_by_score();
    let t_search = std::time::Instant::now();
    let results = searcher.search(&*query, &collector).unwrap();
    let search_ms = t_search.elapsed().as_secs_f64() * 1000.0;
    // What follows — one docstore fetch per hit, to recover the file index for
    // the ground-truth comparison — is harness work, not the engine. It was
    // silently inside the reported latency: 36 824 fetches on `include`.
    LAST_SEARCH_MS.with(|c| c.set(search_ms));

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

/// `...before>>match<<after...` around a byte span, for span diagnostics.
fn span_context(content: &str, a: usize, b: usize) -> String {
    let a = a.min(content.len());
    let b = b.min(content.len()).max(a);
    let cs = snap_back(content, a.saturating_sub(20));
    let ce = snap_fwd(content, (b + 20).min(content.len()));
    let a2 = snap_back(content, a);
    let b2 = snap_fwd(content, b);
    format!("{:?}", format!("{}>>{}<<{}", &content[cs..a2], &content[a2..b2], &content[b2..ce]))
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
    distance: u8,
    /// `startsWith`: the occurrence begins a word (separator or file start before it).
    anchor: bool,
    /// `term`: the occurrence is whole words (anchor + separator or file end after it).
    exact: bool,
}

impl GroundTruthQuery {
    fn strict(text: &'static str) -> Self { Self { text, strict_sep: true, distance: 0, anchor: false, exact: false } }
    fn relaxed(text: &'static str) -> Self { Self { text, strict_sep: false, distance: 0, anchor: false, exact: false } }
    fn fuzzy(text: &'static str, distance: u8) -> Self { Self { text, strict_sep: false, distance, anchor: false, exact: false } }
    fn is_regex(&self) -> bool { self.distance == RX }
    /// Mode suffix of `V3_QUERIES`: `strict`, `relax`, `fz1`-`fz3`, `rx`,
    /// `sw` (startsWith, relaxed), `sws` (startsWith, strict), `term`
    /// (whole words, relaxed), `terms` (whole words, strict).
    fn from_mode(text: &'static str, mode: &str) -> Option<Self> {
        Some(match mode {
            "strict" => Self::strict(text),
            "relax" => Self::relaxed(text),
            "fz1" => Self::fuzzy(text, 1),
            "fz2" => Self::fuzzy(text, 2),
            "fz3" => Self::fuzzy(text, 3),
            "rx" => Self::fuzzy(text, RX),
            "sw" => Self { anchor: true, ..Self::relaxed(text) },
            "sws" => Self { anchor: true, ..Self::strict(text) },
            "term" => Self { anchor: true, exact: true, ..Self::relaxed(text) },
            "terms" => Self { anchor: true, exact: true, ..Self::strict(text) },
            _ => return None,
        })
    }
    fn mode_label(&self) -> String {
        if self.is_regex() { "rx".into() }
        else if self.distance > 0 { format!("fz{}", self.distance) }
        else if self.exact { if self.strict_sep { "terms".into() } else { "term".into() } }
        else if self.anchor { if self.strict_sep { "sws".into() } else { "sw".into() } }
        else if self.strict_sep { "strict".into() } else { "relax".into() }
    }
}

/// Word-boundary filter for the anchored modes, on the source bytes: a span
/// starts a word when the character before it is a separator (or the file
/// starts there), and is a whole word when the character after it is one too
/// (or the file ends there). Separators are ASCII non-alphanumerics, exactly
/// the engine's `is_content_char`.
fn filter_boundaries(gt: GrepSpans, files: &[(String, String)], anchor: bool, exact: bool) -> GrepSpans {
    if !anchor && !exact { return gt; }
    let root = repo_path();
    let root = std::path::Path::new(&root);
    let mut cache: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut spans = HashSet::new();
    let mut docs = HashSet::new();
    for (fi, from, to) in gt.spans {
        let bytes = cache.entry(fi).or_insert_with(|| std::fs::read(root.join(&files[fi].0)).unwrap_or_default());
        let text = String::from_utf8_lossy(bytes);
        let before_ok = from == 0 || text.get(..from).and_then(|t| t.chars().last()).map_or(true, |c| !is_content_char(c));
        let after_ok = to >= bytes.len() || text.get(to..).and_then(|t| t.chars().next()).map_or(true, |c| !is_content_char(c));
        if before_ok && (!exact || after_ok) {
            spans.insert((fi, from, to));
            docs.insert(fi);
        }
    }
    GrepSpans { docs, spans }
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
    let custom_queries: Option<Vec<GroundTruthQuery>> = query_spec();

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
    run_panel(&handle, &files, &queries, &mut report);
}

/// The coherence panel: the shape of queries a code RAG actually sends —
/// long literals full of separators, anchored and whole-word forms, typos
/// in them, and the non-ASCII the corpus really contains (accents, CJK,
/// emoji and ZWJ sequences). Every line is exact against the disk or the
/// test fails. Not a benchmark: rag3db, ~5 s.
#[test]
fn v3_ground_truth_coherence() {
    let files = collect_files(5000);
    if files.is_empty() { return; }
    let handle = create_v3_index(&files);
    let mut report = std::fs::File::create("/tmp/v3_coherence_report.txt").unwrap();
    let q = |t: &'static str, m: &str| GroundTruthQuery::from_mode(t, m).unwrap();
    let queries = query_spec().unwrap_or_else(|| vec![
        // Long literals with separators, strict and relaxed
        q("std::shared_ptr<binder::Expression>", "strict"),
        q("std::shared_ptr<binder::Expression>", "relax"),
        q("#include \"common/types/types.h\"", "strict"),
        q("#include \"common/types/types.h\"", "relax"),
        q("ku_dynamic_cast<const TARGET*>", "strict"),
        q("if (result == nullptr)", "strict"),
        q("if (result == nullptr)", "relax"),
        q("->", "strict"),
        q("::", "strict"),
        // Anchored and whole-word
        q("lock", "sw"),
        q("Expression", "sw"),
        q("shared_ptr", "sws"),
        q("std::shared", "sws"),
        q("ptr", "term"),
        q("Expression", "term"),
        q("unique_ptr", "terms"),
        // Typos inside long separated literals
        q("std::shared_ptr<bindr::Expression>", "fz1"),
        q("ku_dynamc_cast", "fz1"),
        q("client_contxt.h", "fz1"),
        q("unique_ptr", "fz2"),
        // Non-ASCII: accents, CJK, emoji, ZWJ sequence
        q("déjà", "strict"),
        q("déjà", "relax"),
        q("entité", "sw"),
        q("成績評価", "strict"),
        q("成績評価", "fz1"),
        q("🦆🦆🦆", "strict"),
        q("🦆🦆🦆", "relax"),
        q("😂😃", "relax"),
        q("🧘🏻‍♂️🌍", "strict"),
        q("🌍🌦️🍞🚗 movies", "strict"),
        q("🍞🚗", "sw"),
    ]);
    run_panel(&handle, &files, &queries, &mut report);
}

fn run_panel(
    handle: &LucivyHandle,
    files: &[(String, String)],
    queries: &[GroundTruthQuery],
    report: &mut std::fs::File,
) {
    let files = files;
    let handle = handle;
    let mut report = report;
    eprintln!("  {} queries, result cap {}", queries.len(), result_limit(files));

    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut fail_entries: Vec<serde_json::Value> = Vec::new();
    let diag_mode = std::env::var("V3_DIAG").is_ok();

    eprintln!("{:<35} {:>5} {:>8} {:>8} {:>8}", "Query", "Mode", "Grep", "V3", "Status");
    eprintln!("{}", "-".repeat(70));

    for q in queries {
        let mode_label = q.mode_label();
        // Time the two independently. They used to share one timer, so every
        // reported latency silently carried a full grep over the corpus — a
        // constant that dilutes any engine-side comparison.
        let t_grep = std::time::Instant::now();
        let gt = if q.is_regex() { grep_spans_regex(files, q.text) }
                 else if q.distance > 0 { grep_spans_fuzzy(files, q.text, q.distance) }
                 else { filter_boundaries(grep_spans(files, q.text, q.strict_sep), files, q.anchor, q.exact) };
        let grep_ms = t_grep.elapsed().as_secs_f64() * 1000.0;
        let grep_set = gt.docs;

        profile::reset();
        let t = std::time::Instant::now();
        let v3_result = search_v3_q(handle, files, q.text, q.strict_sep, q.distance, q.anchor, q.exact);
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        let search_ms = LAST_SEARCH_MS.with(|c| c.get());

        // Spans: the engine's highlights against every occurrence on disk.
        // Asserted since 23 August, when the 50k kernel panel went exact on
        // both the natural and the merged index: a missing or extra span is
        // a failure, not a remark. `V3_SPANS_REPORT_ONLY=1` restores the old
        // doc-set-only criterion for diagnosis.
        let v3_spans: HashSet<(usize, usize, usize)> =
            v3_result.highlights.iter().copied().collect();
        let missing = gt.spans.difference(&v3_spans).count();
        let extra = v3_spans.difference(&gt.spans).count();
        let spans_ok = (missing == 0 && extra == 0)
            || std::env::var("V3_SPANS_REPORT_ONLY").is_ok();
        let docs_ok = v3_result.doc_indices == grep_set;
        let status = if docs_ok && spans_ok { "OK" } else { "FAIL" };
        let hl = if missing == 0 && extra == 0 {
            format!("spans {} exact", gt.spans.len())
        } else {
            format!("spans gt={} v3={} miss={} extra={}", gt.spans.len(), v3_spans.len(), missing, extra)
        };
        eprintln!("{:<35} {:>5} {:>8} {:>8} {:>6} ({:.1}ms search, {:.1}ms +fetch, {:.1}ms grep) {hl}",
            q.text, mode_label, grep_set.len(), v3_result.doc_indices.len(), status,
            search_ms, ms - search_ms, grep_ms);
        if missing > 0 || extra > 0 {
            let mut miss: Vec<_> = gt.spans.difference(&v3_spans).copied().collect();
            miss.sort();
            let mut ext: Vec<_> = v3_spans.difference(&gt.spans).copied().collect();
            ext.sort();
            for (fi, a, b) in miss.iter().take(3) {
                eprintln!("    missing  doc={fi} [{a}..{b}] {}", span_context(&files[*fi].1, *a, *b));
            }
            for (fi, a, b) in ext.iter().take(3) {
                eprintln!("    extra    doc={fi} [{a}..{b}] {} ({})", span_context(&files[*fi].1, *a, *b), files[*fi].0);
            }
        }
        if std::env::var("V3_PROFILE").is_ok() {
            eprint!("{}", profile::dump());
        }

        write_report(&mut report, q.text, &mode_label, &files, &grep_set, &v3_result);

        if docs_ok && spans_ok {
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
                        word_posmap: None,
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

    let queries: Vec<(&str, bool)> = query_spec().map(|v| v.into_iter().map(|q| (q.text, q.strict_sep)).collect()).unwrap_or_else(|| vec![
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

/// The coherence panel through sharding and distribution, spans included.
///
/// `v3_distributed_two_nodes` only compared document COUNTS for three contains
/// queries. This runs the whole RAG-shaped panel (strict/relaxed long literals,
/// startsWith/term, fuzzy, regex, non-ASCII) on three shapes of the same
/// corpus — one shard, four shards, and two nodes with stats exported,
/// merged and injected like over a network — and requires the highlights of
/// each shape to be exactly the occurrences on disk, hence identical to each
/// other. Scores may differ between shapes; spans may not.
#[test]
fn v3_distributed_coherence() {
    use lucivy_core::sharded_handle::{ShardedHandle, ShardedSearchResult, RamShardStorage};
    use lucivy_core::bm25_global::ExportableStats;

    let files = collect_files(3000);
    if files.is_empty() { return; }
    let left: Vec<_> = files.iter().step_by(2).cloned().collect();
    let right: Vec<_> = files.iter().skip(1).step_by(2).cloned().collect();

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
        let nid_f = h.field(NODE_ID_FIELD).unwrap();
        for (path, content) in docs {
            // The node id is the file's index in `files`, on every node. The
            // id given to `add_document` only feeds the router; the stored
            // field is the caller's (the bindings add it the same way).
            let idx = files.iter().position(|(p, _)| p == path).unwrap();
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, idx as u64);
            doc.add_text(path_f, path);
            doc.add_text(content_f, content);
            h.add_document(doc, idx as u64).unwrap();
        }
        h.commit().unwrap();
        h
    };
    let node_1 = build(&files, 1);
    let node_4 = build(&files, 4);
    let node_a = build(&left, 2);
    let node_b = build(&right, 2);

    // Highlights of a sharded result set, keyed by file index.
    let collect = |h: &ShardedHandle, results: &[ShardedSearchResult],
                   sink: &ld_lucivy::query::HighlightSink| -> SearchResult {
        let mut doc_indices = HashSet::new();
        let mut highlights = Vec::new();
        for r in results {
            let shard = h.shard(r.shard_id).unwrap();
            let searcher = shard.reader.searcher();
            let doc: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address).unwrap();
            let nid_f = shard.field(NODE_ID_FIELD).unwrap();
            use ld_lucivy::schema::document::Value;
            let idx = doc.field_values().find(|(f, _)| *f == nid_f)
                .and_then(|(_, v)| v.as_value().as_u64()).unwrap() as usize;
            doc_indices.insert(idx);
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            if let Some(hl) = sink.get(seg_id, r.doc_address.doc_id) {
                if let Some(offs) = hl.get("content") {
                    for [a, b] in offs { highlights.push((idx, *a, *b)); }
                }
            }
        }
        SearchResult { doc_indices, highlights }
    };

    let q = |t: &'static str, m: &str| GroundTruthQuery::from_mode(t, m).unwrap();
    let queries = query_spec().unwrap_or_else(|| vec![
        q("std::shared_ptr<binder::Expression>", "strict"),
        q("std::shared_ptr<binder::Expression>", "relax"),
        q("#include \"common/types/types.h\"", "strict"),
        q("ku_dynamic_cast<const TARGET*>", "strict"),
        q("if (result == nullptr)", "relax"),
        q("::", "strict"),
        q("lock", "sw"),
        q("std::shared", "sws"),
        q("ptr", "term"),
        q("unique_ptr", "terms"),
        q("std::shared_ptr<bindr::Expression>", "fz1"),
        q("ku_dynamc_cast", "fz1"),
        q("unique_ptr", "fz2"),
        q("std::[a-z_]+_ptr<", "rx"),
        q("[0-9]{8}", "rx"),
        q("déjà", "relax"),
        q("entité", "sw"),
        q("🦆🦆🦆", "strict"),
        q("🧘🏻‍♂️🌍", "strict"),
    ]);

    let mut fails = 0;
    for gq in &queries {
        let config = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(gq.text.into()),
            strict_separators: Some(gq.strict_sep),
            distance: if gq.distance > 0 && gq.distance != RX { Some(gq.distance) } else { None },
            regex: if gq.distance == RX { Some(true) } else { None },
            anchor_start: if gq.anchor { Some(true) } else { None },
            exact_match: if gq.exact { Some(true) } else { None },
            ..Default::default()
        };
        let gt = if gq.is_regex() { grep_spans_regex(&files, gq.text) }
                 else if gq.distance > 0 { grep_spans_fuzzy(&files, gq.text, gq.distance) }
                 else { filter_boundaries(grep_spans(&files, gq.text, gq.strict_sep), &files, gq.anchor, gq.exact) };

        let run = |h: &ShardedHandle| -> SearchResult {
            let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
            let r = h.search(&config, 100_000, Some(sink.clone())).unwrap();
            collect(h, &r, &sink)
        };
        let r1 = run(&node_1);
        let r4 = run(&node_4);

        let sa = node_a.export_stats(&config).unwrap();
        let sb = node_b.export_stats(&config).unwrap();
        let sa: ExportableStats = serde_json::from_str(&serde_json::to_string(&sa).unwrap()).unwrap();
        let sb: ExportableStats = serde_json::from_str(&serde_json::to_string(&sb).unwrap()).unwrap();
        let global = ExportableStats::merge(&[sa, sb]);
        let sink_a = Arc::new(ld_lucivy::query::HighlightSink::new());
        let sink_b = Arc::new(ld_lucivy::query::HighlightSink::new());
        let ra = node_a.search_with_global_stats(&config, 100_000, &global, Some(sink_a.clone())).unwrap();
        let rb = node_b.search_with_global_stats(&config, 100_000, &global, Some(sink_b.clone())).unwrap();
        let mut rd = collect(&node_a, &ra, &sink_a);
        let rbb = collect(&node_b, &rb, &sink_b);
        rd.doc_indices.extend(rbb.doc_indices);
        rd.highlights.extend(rbb.highlights);

        let mut line = format!("  {:<36} {:>5} gt docs={} spans={}", gq.text, gq.mode_label(), gt.docs.len(), gt.spans.len());
        let mut ok = true;
        for (label, r) in [("1 shard", &r1), ("4 shards", &r4), ("2 nodes", &rd)] {
            let spans: HashSet<(usize, usize, usize)> = r.highlights.iter().copied().collect();
            let miss = gt.spans.difference(&spans).count();
            let extra = spans.difference(&gt.spans).count();
            let docs_ok = r.doc_indices == gt.docs;
            if miss > 0 || extra > 0 || !docs_ok { ok = false; }
            line.push_str(&format!(" | {label}: docs={} spans={} miss={miss} extra={extra}", r.doc_indices.len(), spans.len()));
        }
        eprintln!("{line} {}", if ok { "OK" } else { "FAIL" });
        if !ok { fails += 1; }
    }
    assert_eq!(fails, 0, "{fails} queries differ between shapes or from the disk");
}

/// Node-id filtering, deletion by node id, and the sharded delta (LUCIDS)
/// on a v3 index, spans included.
///
/// A database that already knows which documents qualify hands the engine a
/// set of node ids: the answer must be exactly the ground truth restricted
/// to that set, on every shard. A deletion by node id must remove every
/// occurrence of that document and nothing else. And a client holding a
/// snapshot, given the delta of those deletions and of new documents, must
/// answer exactly like the source. All of it on disk, 4 shards, v3.
#[test]
fn v3_sharded_filter_delete_delta() {
    use lucivy_core::sharded_handle::{ShardedHandle, ShardedSearchResult};
    use lucistore::delta_sharded::compute_shard_versions;

    let files = collect_files(2000);
    if files.is_empty() { return; }
    let scratch = std::env::var("V3_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let src_dir = format!("{scratch}/v3_fdd_src");
    let dst_dir = format!("{scratch}/v3_fdd_dst");
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
    std::fs::create_dir_all(&src_dir).unwrap();

    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": 3,
        "shards": 4
    })).unwrap();
    let src = ShardedHandle::create(&src_dir, &config).unwrap();
    let path_f = src.field("path").unwrap();
    let content_f = src.field("content").unwrap();
    let nid_f = src.field(NODE_ID_FIELD).unwrap();
    let add = |h: &ShardedHandle, idx: usize, path: &str, content: &str| {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, idx as u64);
        doc.add_text(path_f, path);
        doc.add_text(content_f, content);
        h.add_document(doc, idx as u64).unwrap();
    };
    for (i, (p, c)) in files.iter().enumerate() { add(&src, i, p, c); }
    src.commit().unwrap();
    // Raw copy below: let the policy merges and their GC finish first.
    let drain = |h: &ShardedHandle| {
        for i in 0.. {
            let Some(s) = h.shard(i) else { break };
            s.writer.lock().unwrap().as_ref().unwrap().drain_merges().unwrap();
            s.reader.reload().unwrap();
        }
    };
    drain(&src);

    let collect = |h: &ShardedHandle, results: &[ShardedSearchResult],
                   sink: &ld_lucivy::query::HighlightSink| -> SearchResult {
        let mut doc_indices = HashSet::new();
        let mut highlights = Vec::new();
        for r in results {
            let shard = h.shard(r.shard_id).unwrap();
            let searcher = shard.reader.searcher();
            let doc: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address).unwrap();
            use ld_lucivy::schema::document::Value;
            let idx = doc.field_values().find(|(f, _)| *f == nid_f)
                .and_then(|(_, v)| v.as_value().as_u64()).unwrap() as usize;
            doc_indices.insert(idx);
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            if let Some(hl) = sink.get(seg_id, r.doc_address.doc_id) {
                if let Some(offs) = hl.get("content") {
                    for [a, b] in offs { highlights.push((idx, *a, *b)); }
                }
            }
        }
        SearchResult { doc_indices, highlights }
    };
    let q = |t: &'static str, m: &str| GroundTruthQuery::from_mode(t, m).unwrap();
    let queries = vec![
        q("std::shared_ptr<binder::Expression>", "strict"),
        q("if (result == nullptr)", "relax"),
        q("::", "strict"),
        q("Expression", "sw"),
        q("ptr", "term"),
        q("ku_dynamc_cast", "fz1"),
        q("unique_ptr", "fz2"),
        q("std::[a-z_]+_ptr<", "rx"),
        q("déjà", "relax"),
    ];
    let config_of = |gq: &GroundTruthQuery| QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(gq.text.into()),
        strict_separators: Some(gq.strict_sep),
        distance: if gq.distance > 0 && gq.distance != RX { Some(gq.distance) } else { None },
        regex: if gq.distance == RX { Some(true) } else { None },
        anchor_start: if gq.anchor { Some(true) } else { None },
        exact_match: if gq.exact { Some(true) } else { None },
        ..Default::default()
    };
    let truth = |gq: &GroundTruthQuery| -> GrepSpans {
        if gq.is_regex() { grep_spans_regex(&files, gq.text) }
        else if gq.distance > 0 { grep_spans_fuzzy(&files, gq.text, gq.distance) }
        else { filter_boundaries(grep_spans(&files, gq.text, gq.strict_sep), &files, gq.anchor, gq.exact) }
    };
    let restrict = |gt: &GrepSpans, keep: &dyn Fn(usize) -> bool| -> GrepSpans {
        GrepSpans {
            docs: gt.docs.iter().copied().filter(|d| keep(*d)).collect(),
            spans: gt.spans.iter().copied().filter(|(d, _, _)| keep(*d)).collect(),
        }
    };
    let check = |label: &str, gq: &GroundTruthQuery, gt: &GrepSpans, r: &SearchResult, fails: &mut u32| {
        let spans: HashSet<(usize, usize, usize)> = r.highlights.iter().copied().collect();
        let miss = gt.spans.difference(&spans).count();
        let extra = spans.difference(&gt.spans).count();
        let ok = miss == 0 && extra == 0 && r.doc_indices == gt.docs;
        eprintln!("  {label:<10} {:<36} {:>5} gt docs={} spans={} | docs={} spans={} miss={miss} extra={extra} {}",
            gq.text, gq.mode_label(), gt.docs.len(), gt.spans.len(), r.doc_indices.len(), spans.len(),
            if ok { "OK" } else { "FAIL" });
        if !ok { *fails += 1; }
    };
    let mut fails = 0u32;

    // ── Filter by node id: the database pre-filtered, the engine must not scan past it.
    let allowed: HashSet<u64> = (0..files.len() as u64).filter(|i| i % 3 == 0).collect();
    for gq in &queries {
        let gt = restrict(&truth(gq), &|d| d % 3 == 0);
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let r = src.search_filtered(&config_of(gq), 100_000, Some(sink.clone()), allowed.clone()).unwrap();
        check("filter", gq, &gt, &collect(&src, &r, &sink), &mut fails);
    }

    // ── Snapshot for the delta client, taken before the changes below.
    std::fs::create_dir_all(&dst_dir).unwrap();
    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let p = e.path();
            let t = to.join(e.file_name());
            if p.is_dir() { std::fs::create_dir_all(&t).unwrap(); copy_tree(&p, &t); }
            else { std::fs::copy(&p, &t).unwrap(); }
        }
    }
    copy_tree(std::path::Path::new(&src_dir), std::path::Path::new(&dst_dir));
    let client_versions = compute_shard_versions(std::path::Path::new(&dst_dir), 4).unwrap();

    // ── Delete by node id, add new documents.
    let deleted: HashSet<usize> = (0..files.len()).filter(|i| i % 7 == 0).collect();
    for &d in &deleted { src.delete_by_node_id(d as u64).unwrap(); }
    let new_docs: Vec<(usize, String)> = (0..20).map(|k| (files.len() + k,
        format!("// added after snapshot {k}\nstd::shared_ptr<binder::Expression> added_{k} = nullptr;\nif (result == nullptr) {{ return déjà_{k}; }}\n"))).collect();
    for (idx, c) in &new_docs { add(&src, *idx, &format!("added_{idx}.cpp"), c); }
    src.commit().unwrap();
    drain(&src);
    // Ground truth for the new state: disk files minus the deleted ones, with
    // exact spans; the added documents are judged on membership only (they
    // are not on disk for the grep), so their spans are left out of the
    // comparison on both sides.
    let n_files = files.len();
    let contains_new = |gq: &GroundTruthQuery, c: &str| -> bool {
        if gq.is_regex() {
            regex::RegexBuilder::new(gq.text).case_insensitive(true).build().unwrap().is_match(c)
        } else if gq.distance > 0 {
            !ld_lucivy::suffix_fst::briques::fuzzy_spans::fuzzy_spans(
                strip_seps(&c.to_lowercase()).as_bytes(), strip_seps(&gq.text.to_lowercase()).as_bytes(),
                gq.distance as usize).is_empty()
        } else if gq.strict_sep {
            c.to_lowercase().contains(&gq.text.to_lowercase())
        } else {
            strip_seps(&c.to_lowercase()).contains(&strip_seps(&gq.text.to_lowercase()))
        }
    };
    let truth_after = |gq: &GroundTruthQuery| -> GrepSpans {
        let mut gt = restrict(&truth(gq), &|d| !deleted.contains(&d));
        if !(gq.anchor || gq.exact) {
            for (idx, c) in &new_docs {
                if contains_new(gq, c) { gt.docs.insert(*idx); }
            }
        }
        gt
    };
    let without_new = |r: SearchResult| SearchResult {
        doc_indices: r.doc_indices,
        highlights: r.highlights.into_iter().filter(|(d, _, _)| *d < n_files).collect(),
    };
    // Anchored modes: the added docs would need a boundary truth; the
    // membership check above is plain contains, so skip them for new docs.
    let after_queries: Vec<&GroundTruthQuery> = queries.iter().filter(|g| !(g.anchor || g.exact)).collect();

    for gq in &after_queries {
        let gt = truth_after(gq);
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let r = src.search(&config_of(gq), 100_000, Some(sink.clone())).unwrap();
        let r = without_new(collect(&src, &r, &sink));
        check("deleted", gq, &gt, &r, &mut fails);
    }

    // ── Delta to the client, then the client must answer like the source.
    let blob = src.export_sharded_delta(&src_dir, &client_versions).unwrap();
    eprintln!("  delta blob: {} bytes", blob.len());
    {
        let d = lucistore::delta_sharded::deserialize_sharded_delta(&blob).unwrap();
        for (sid, sd) in &d.shard_deltas {
            let added: Vec<String> = sd.added_segments.iter()
                .map(|b| format!("{}:{}B", &b.segment_id[..6], b.files.iter().map(|(_, f)| f.len()).sum::<usize>())).collect();
            eprintln!("    shard_{sid}: removed={:?} added={added:?}",
                sd.removed_segment_ids.iter().map(|x| &x[..6]).collect::<Vec<_>>());
        }
    }
    src.close().unwrap();
    for i in 0..4 { remove_lock_files_dir(&format!("{dst_dir}/shard_{i}")); }
    let dst = ShardedHandle::open(&dst_dir).unwrap();
    dst.apply_sharded_delta(&dst_dir, &blob).unwrap();
    for gq in &after_queries {
        let gt = truth_after(gq);
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let r = dst.search(&config_of(gq), 100_000, Some(sink.clone())).unwrap();
        let r = without_new(collect(&dst, &r, &sink));
        check("delta", gq, &gt, &r, &mut fails);
    }
    dst.close().unwrap();
    assert_eq!(fails, 0, "{fails} checks failed");
}

fn remove_lock_files_dir(dir: &str) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(".lock") { let _ = std::fs::remove_file(e.path()); }
        }
    }
}
