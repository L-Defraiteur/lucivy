//! Minimal E2E test: SFX v3 pipeline through Index + IndexWriter directly.
//! Creates index with sfx_version=3, indexes docs, verifies search works.

use std::sync::Arc;
use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig, SchemaConfig};

fn make_handle(docs: &[&str]) -> LucivyHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "content", "type": "text", "stored": true}
        ],
        "sfx_version": 3
    })).unwrap();

    let dir = ld_lucivy::directory::RamDirectory::default();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        w.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        for (i, text) in docs.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(content_f, text);
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

/// Smoke test: bypass LucivyHandle, use Index+IndexWriter directly to get a clearer error.
#[test]
fn v3_smoke_direct_index() {
    use ld_lucivy::schema::{Schema, TEXT, STORED};
    use ld_lucivy::{Index, IndexSettings, LucivyDocument, ReloadPolicy};

    let mut schema_builder = Schema::builder();
    let content = schema_builder.add_text_field("content", TEXT | STORED);
    let schema = schema_builder.build();

    let mut settings = IndexSettings::default();
    settings.sfx_version = 3;

    let index = Index::builder().schema(schema).settings(settings).create_in_ram().unwrap();
    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
    writer.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));

    let mut doc = LucivyDocument::new();
    doc.add_text(content, "mutex_lock is a function");
    writer.add_document(doc).unwrap();
    let result = writer.commit();
    eprintln!("commit result: {:?}", result);
    assert!(result.is_ok(), "commit failed: {:?}", result.err());

    let reader = index.reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .unwrap();
    reader.reload().unwrap();
    let searcher = reader.searcher();
    eprintln!("num_docs: {}", searcher.num_docs());
    assert!(searcher.num_docs() > 0, "no docs indexed");
}

fn search(handle: &LucivyHandle, query_type: &str, value: &str) -> Vec<u32> {
    let config = QueryConfig {
        query_type: query_type.into(),
        field: Some("content".into()),
        value: Some(value.into()),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(10_000).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    results.iter().map(|(_, addr)| addr.doc_id).collect()
}

fn search_with_highlights(handle: &LucivyHandle, value: &str) -> Vec<(u32, Vec<[usize; 2]>)> {
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        ..Default::default()
    };
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(10_000).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();

    let mut out = Vec::new();
    for (_, addr) in &results {
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        let hl = sink.get(seg_id, addr.doc_id);
        let offsets = hl
            .and_then(|m| m.get("content").cloned())
            .unwrap_or_default();
        out.push((addr.doc_id, offsets));
    }
    out
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[test]
fn v3_contains_basic() {
    let handle = make_handle(&[
        "mutex_lock is a function",
        "hello world",
        "another mutex_lock usage",
    ]);
    let docs = search(&handle, "contains", "mutex_lock");
    assert!(docs.len() >= 2, "expected 2+ docs with mutex_lock, got {}", docs.len());

    let docs2 = search(&handle, "contains", "hello");
    assert_eq!(docs2.len(), 1);
}

#[test]
fn v3_contains_substring() {
    let handle = make_handle(&[
        "ku_dynamic_cast<StructColumn&>(column)",
        "no match here",
    ]);
    // Substring within a long identifier
    let docs = search(&handle, "contains", "dynamic_cast");
    assert!(!docs.is_empty(), "should find dynamic_cast as substring");

    let docs2 = search(&handle, "contains", "ku_dynamic");
    assert!(!docs2.is_empty(), "should find ku_dynamic as substring");
}

#[test]
fn v3_contains_cross_token() {
    let handle = make_handle(&[
        "std::unique_ptr<TableFuncBindData>",
    ]);
    // "unique_ptr" crosses token boundary (8 bytes max per token)
    let docs = search(&handle, "contains", "unique_ptr");
    assert!(!docs.is_empty(), "unique_ptr should be found cross-token");
}

#[test]
fn v3_starts_with() {
    let handle = make_handle(&[
        "rag3db_prepared_statement_bind_bool",
        "rag3db_connection_set_max",
        "something_else",
    ]);
    let docs = search(&handle, "startsWith", "rag3db_");
    assert_eq!(docs.len(), 2, "2 docs start with rag3db_");
}

#[test]
fn v3_fuzzy_d1() {
    let handle = make_handle(&[
        "ku_dynamic_cast is used everywhere",
        "no match",
    ]);
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("ku_dinamic_cast".into()), // typo y→i
        distance: Some(1),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let results = searcher.search(&*query, &ld_lucivy::collector::Count).unwrap();
    assert!(results >= 1, "fuzzy d=1 should find ku_dynamic_cast with typo, got {}", results);
}

#[test]
fn v3_highlights_byte_ranges() {
    let handle = make_handle(&[
        "mutex_lock is important",
    ]);
    let hl = search_with_highlights(&handle, "mutex");
    assert!(!hl.is_empty(), "should have highlights");
    let (_, offsets) = &hl[0];
    assert!(!offsets.is_empty(), "should have highlight offsets");
    // "mutex" starts at byte 0 in "mutex_lock is important"
    let [start, end] = offsets[0];
    assert_eq!(start, 0, "highlight should start at 0");
    assert!(end <= 10, "highlight end should be reasonable, got {}", end);
}

#[test]
fn v3_strict_sep_false() {
    let handle = make_handle(&[
        "mutex_lock function",
        "no match here",
    ]);
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("mutexlock".into()),
        strict_separators: Some(false),
        ..Default::default()
    };
    let query = query::build_query(&config, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let results = searcher.search(&*query, &ld_lucivy::collector::Count).unwrap();
    assert!(results >= 1, "strict_sep=false should find mutexlock in mutex_lock, got {}", results);
}

#[test]
fn v3_multi_doc_correct_ids() {
    let handle = make_handle(&[
        "alpha beta",      // doc 0
        "gamma delta",     // doc 1
        "alpha gamma",     // doc 2
    ]);
    // With NoMergePolicy, each doc is in its own segment (doc_id=0 in each).
    // Just verify the count is correct.
    let docs_alpha = search(&handle, "contains", "alpha");
    assert_eq!(docs_alpha.len(), 2, "alpha should be in 2 docs, got {:?}", docs_alpha);

    let docs_gamma = search(&handle, "contains", "gamma");
    assert_eq!(docs_gamma.len(), 2, "gamma should be in 2 docs, got {:?}", docs_gamma);

    let docs_delta = search(&handle, "contains", "delta");
    assert_eq!(docs_delta.len(), 1, "delta should be in 1 doc");
}

/// `term` must be a whole-token match, not a prefix.
///
/// CURRENTLY RED, and deliberately ignored rather than deleted or "fixed" by
/// weakening the assertion. `git bisect` puts the break at 8aeb093 ("unify span
/// unit, drop dead post-filter, wire word singles into contains"): from that
/// commit on, `term "utex"` matches "mutex". The exact_match predicate is
/// `token_end - byte_from == query_content_len`, which measures from the MATCH
/// start, so a suffix match inside a token reads as a whole-token match once
/// anything lets a candidate with sti > 0 through.
///
/// Left ignored because `term` is not on the critical path for code RAG — the
/// substring, fuzzy and regex paths are — and a red test drowns the signal from
/// the ones that matter. Unignore it before touching exact_match or anchor_start.
///
/// `term` is routed to `contains + anchor_start + exact_match`
/// (`lucivy_core/src/query.rs`), and `exact_match` is the ONLY thing separating it
/// from `contains`. Nothing else in the suite covers the negative direction, so any
/// change that makes the exact_match filter always-true would silently turn `term`
/// into `contains` and stay green. This test is that guard.
#[test]
fn v3_term_is_whole_token_not_prefix() {
    let handle = make_handle(&[
        "mutex lock implementation",
        "alpha beta gamma",
    ]);

    // Positive: the full token matches.
    let full = search(&handle, "term", "mutex");
    assert_eq!(full.len(), 1, "term 'mutex' should match the whole token, got {full:?}");

    // Negative: a strict prefix of the token must NOT match.
    let prefix = search(&handle, "term", "mut");
    assert!(prefix.is_empty(), "term 'mut' must not match 'mutex' — exact_match is broken, got {prefix:?}");

    // Negative: a strict suffix must not match either.
    let suffix = search(&handle, "term", "utex");
    assert!(suffix.is_empty(), "term 'utex' must not match 'mutex', got {suffix:?}");

    // Sanity: the same strings DO match as `contains`, proving the corpus is right
    // and that only the exact_match filter separates the two behaviours.
    assert_eq!(search(&handle, "contains", "mut").len(), 1, "contains 'mut' should match");
    assert_eq!(search(&handle, "contains", "utex").len(), 1, "contains 'utex' should match");
}

/// `startsWith` is "the match begins a word": `unlock` and `clock` contain
/// `lock` but do not start with it. Mirrors bench_sharding `t00`, which found
/// v3 answering 8 instead of 6.
#[test]
fn v3_starts_with_is_word_start() {
    let handle = make_handle(&[
        "The lock mechanism is simple",
        "Multiple locks are held",
        "Locking primitives in kernel",
        "lockdep is a debugging tool",
        "unlock the resource",
        "This has clock hardware",
        "spin_lock_init(&x)",
    ]);
    // `make_handle` gives every document its own segment, so the returned
    // ids are all 0: only the counts carry information here.
    let sw = search(&handle, "startsWith", "lock");
    assert_eq!(sw.len(), 5, "startsWith lock: docs 0-3 and spin_lock_init (lock after `_`), not unlock/clock; got {sw:?}");
    assert_eq!(search(&handle, "contains", "lock").len(), 7);
    // term: a whole word. `_` is a separator for v3, so `lock` inside
    // `spin_lock_init` is a whole word too — `locks`, `locking`, `lockdep`,
    // `unlock`, `clock` are not.
    let t = search(&handle, "term", "lock");
    assert_eq!(t.len(), 2, "term lock: `The lock` and `spin_lock_init`, got {t:?}");
}

/// PROBE — prints current behaviour of exact_match edge cases. No assertions.
#[test]
fn probe_exact_match_edge_cases() {
    let handle = make_handle(&[
        "mutex_lock implementation",
        "le café est chaud",
        "alpha beta gamma",
    ]);
    for (label, q) in [
        ("term mutex_lock (cross-chunk + sep)", "mutex_lock"),
        ("term mutex (prefix of mutex_lock)", "mutex"),
        ("term café (unicode)", "café"),
        ("term alpha (plain ascii word)", "alpha"),
        ("term implementation (>8 bytes, multi-chunk)", "implementation"),
        ("term gamma (last word, no trailing sep)", "gamma"),
    ] {
        eprintln!("  {label:45} -> {:?}", search(&handle, "term", q));
    }
}

/// A merged v3 index must answer exactly like an unmerged one.
///
/// The merge path used to route v3 through the v2 sub-DAG, whose CollectTokensNode
/// reads the inverted-index term dictionary — a different alphabet than the SFX
/// ordinal space. That produced a well-formed v2 segment whose postings pointed at
/// the wrong terms, silently. It is now a re-index from the stored text, so this
/// test is the guard: same documents, same answers, whether merged or not.
#[test]
fn v3_merge_preserves_results() {
    let docs: Vec<String> = (0..400)
        .map(|i| format!(
            "doc{i} mutex_lock implementation TableFunction rag3weaver \
             uint64_t value std::unique_ptr ptr{i} spin_lock_init"
        ))
        .collect();
    let refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();

    // Reference: no merge, many segments.
    let unmerged = make_handle(&refs);

    // Same corpus, but merging enabled.
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3
    })).unwrap();
    let dir = ld_lucivy::directory::RamDirectory::default();
    let merged = LucivyHandle::create(dir, &config).unwrap();
    let content_f = merged.field("content").unwrap();
    let nid_f = merged.field(NODE_ID_FIELD).unwrap();
    {
        let mut guard = merged.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        // The default policy now merges on commit; this test drives the merge
        // itself, and an explicit merge over segments a policy merge is
        // already reading is refused.
        w.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        for (i, text) in refs.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(content_f, *text);
            w.add_document(doc).unwrap();
            if (i + 1) % 50 == 0 { w.commit().unwrap(); }
        }
        w.commit().unwrap();
    }
    merged.reader.reload().unwrap();
    let n_before = merged.reader.searcher().segment_readers().len();

    // Force the merge rather than hoping a policy fires: a green run on an index
    // that never merged would prove nothing at all.
    {
        let mut guard = merged.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        let ids = merged.index.searchable_segment_ids().unwrap();
        assert!(ids.len() > 1, "need several segments to exercise the merge");
        w.merge(&ids).unwrap();
        w.commit().unwrap();
    }
    {
        let mut guard = merged.writer.lock().unwrap();
        if let Some(w) = guard.take() { w.wait_merging_threads().unwrap(); }
    }
    merged.reader.reload().unwrap();

    let n_unmerged = unmerged.reader.searcher().segment_readers().len();
    let n_merged = merged.reader.searcher().segment_readers().len();
    eprintln!("  segments: unmerged={n_unmerged}, merged {n_before} -> {n_merged}");
    assert!(n_merged < n_before,
        "the merge did not happen ({n_before} -> {n_merged}); the rest of this test would be vacuous");

    for (label, q) in [
        ("substring", "mutex"),
        ("cross-token", "mutex_lock"),
        ("camel", "TableFunction"),
        ("with separators", "std::unique_ptr"),
        ("suffix", "weaver"),
        ("digits", "uint64_t"),
    ] {
        let a = search(&unmerged, "contains", q).len();
        let b = search(&merged, "contains", q).len();
        eprintln!("  {label:<18} {q:<18} unmerged={a:<5} merged={b}");
        assert_eq!(a, b, "merged index disagrees on {label} query {q:?}");
        assert!(a > 0, "query {q:?} should match something");
    }
}

/// Every span the engine reports must be the exact byte range of an occurrence.
///
/// The ground-truth bench compares highlights against every occurrence on disk
/// and found three shapes of wrong span: matches straddling a chunk boundary
/// cut short by one or two bytes (`Functio`), and relaxed matches stopping
/// before a trailing separator-led token (`uint64` for `uint64_t`).
fn spans_for(handle: &LucivyHandle, value: &str, strict: bool) -> Vec<[usize; 2]> {
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict),
        ..Default::default()
    };
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let mut out = Vec::new();
    for (_, addr) in &results {
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        if let Some(m) = sink.get(seg_id, addr.doc_id) {
            out.extend(m.get("content").cloned().unwrap_or_default());
        }
    }
    out.sort();
    out
}

#[test]
fn v3_span_exact_across_chunk_boundary() {
    let doc = "       false\n  AfterFunction:   false\n  AfterNam";
    let handle = make_handle(&[doc]);
    let spans = spans_for(&handle, "function", true);
    let expect = doc.to_lowercase().find("function").unwrap();
    eprintln!("spans={spans:?} expect=[{}..{}]", expect, expect + 8);
    assert_eq!(spans, vec![[expect, expect + 8]]);
}

#[test]
fn v3_span_exact_three_chunks() {
    let doc = "clause.constCast<BoundTableFunctionCall>();\n    auto bi";
    let handle = make_handle(&[doc]);
    let spans = spans_for(&handle, "TableFunction", true);
    let expect = doc.to_lowercase().find("tablefunction").unwrap();
    eprintln!("spans={spans:?} expect=[{}..{}]", expect, expect + 13);
    assert_eq!(spans, vec![[expect, expect + 13]]);
}

#[test]
fn v3_span_exact_relaxed_trailing_token() {
    let doc = "2\"\n#endif\nconstexpr uint64_t DEFAULT_VECTOR_CAPA";
    let handle = make_handle(&[doc]);
    let spans = spans_for(&handle, "uint64_t", false);
    let expect = doc.find("uint64_t").unwrap();
    eprintln!("spans={spans:?} expect=[{}..{}]", expect, expect + 8);
    assert_eq!(spans, vec![[expect, expect + 8]]);
}

#[test]
fn v3_span_starts_at_match_not_at_head_token() {
    let doc = "sh_machine_vector mv_vapor __init mv = {\n\t\t.mv_name = ";
    let handle = make_handle(&[doc]);
    let spans = spans_for(&handle, "__init", true);
    let expect = doc.find("__init").unwrap();
    eprintln!("spans={spans:?} expect=[{}..{}]", expect, expect + 6);
    assert_eq!(spans, vec![[expect, expect + 6]]);
}

#[test]
fn v3_span_relaxed_starts_at_content() {
    let doc = "4_t numBytesRead = 0;\n    uint64_t offsetToWrite = 0";
    let handle = make_handle(&[doc]);
    let spans = spans_for(&handle, "uint64_t", false);
    let expect = doc.find("uint64_t").unwrap();
    eprintln!("spans={spans:?} expect=[{}..{}]", expect, expect + 8);
    assert_eq!(spans, vec![[expect, expect + 8]]);
}

#[test]
fn v3_span_relaxed_real_file_head() {
    let doc = std::fs::read_to_string("/tmp/s3fs_head.txt").unwrap_or_default();
    if doc.is_empty() { return; }
    let handle = make_handle(&[&doc]);
    let spans = spans_for(&handle, "uint64_t", false);
    let mut expect = Vec::new();
    let mut i = 0;
    while let Some(p) = doc[i..].find("uint64_t") { expect.push([i + p, i + p + 8]); i += p + 1; }
    eprintln!("spans={spans:?}\nexpect={expect:?}");
    assert_eq!(spans, expect);
}

#[test]
fn v3_span_relaxed_two_real_files() {
    let a = std::fs::read_to_string("/tmp/s3fs_head.txt").unwrap_or_default();
    let b = std::fs::read_to_string("/tmp/cfm_head.txt").unwrap_or_default();
    if a.is_empty() || b.is_empty() { return; }
    let handle = make_handle(&[&a, &b]);
    let spans = spans_for(&handle, "uint64_t", false);
    eprintln!("spans={spans:?}");
}

// ─── A merged segment must be indistinguishable from a fresh one ─────────────

/// Per-document spans, keyed by node id so two indexes of the same corpus can
/// be compared document by document regardless of segment layout.
fn doc_spans(handle: &LucivyHandle, value: &str, strict: bool, n_docs: usize)
    -> std::collections::BTreeMap<u64, Vec<[usize; 2]>>
{
    use ld_lucivy::schema::document::Value;
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        strict_separators: Some(strict),
        ..Default::default()
    };
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(n_docs.max(1)).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for (_, addr) in &results {
        let doc = searcher.doc::<ld_lucivy::LucivyDocument>(*addr).unwrap();
        let nid = doc.field_values()
            .find(|(f, _)| *f == nid_f)
            .and_then(|(_, v)| v.as_value().as_u64())
            .unwrap();
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        let mut spans = sink.get(seg_id, addr.doc_id)
            .and_then(|m| m.get("content").cloned())
            .unwrap_or_default();
        spans.sort();
        spans.dedup();
        out.insert(nid, spans);
    }
    out
}

/// All strict occurrences of `needle` in `text` (ASCII case-fold, overlapping).
fn grep_strict(text: &str, needle: &str) -> Vec<[usize; 2]> {
    let hay: Vec<u8> = text.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let nd: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    if nd.is_empty() || hay.len() < nd.len() { return out; }
    for s in 0..=hay.len() - nd.len() {
        if hay[s..s + nd.len()] == nd[..] { out.push([s, s + nd.len()]); }
    }
    out
}

/// Corpus for the merge-equivalence test: real kernel sources when the bench
/// tree is present (`V3_CORPUS`, default `/tmp/linux-bench`), otherwise a
/// synthetic corpus with enough shared vocabulary to make interning matter.
fn merge_corpus() -> Vec<String> {
    let n: usize = std::env::var("V3_MERGE_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
    let root = std::env::var("V3_CORPUS").unwrap_or_else(|_| "/tmp/linux-bench".into());
    let root = std::path::Path::new(&root);
    let mut docs = Vec::new();
    if root.exists() {
        let mut paths: Vec<std::path::PathBuf> = walkdir(root)
            .into_iter()
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("c") | Some("h") | Some("rst")))
            .collect();
        paths.sort();
        // Spread over the tree rather than taking one directory.
        let step = (paths.len() / n).max(1);
        for p in paths.iter().step_by(step).take(n) {
            if let Ok(s) = std::fs::read_to_string(p) {
                if s.len() < 64 * 1024 { docs.push(s); }
            }
        }
    }
    if docs.is_empty() {
        for i in 0..n {
            docs.push(format!(
                "#include <linux/init.h>\nstatic int __init mod{i}_init(void)\n{{\n\tspin_lock(&lock{i});\n\t\
                 uint64_t v = kmalloc(sizeof(struct net_device), GFP_KERNEL);\n\treturn {};\n}}\n",
                i % 7
            ));
        }
    }
    docs
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p); } else { out.push(p); }
        }
    }
    out
}

fn handle_with_commits(docs: &[String], commit_every: usize) -> LucivyHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3
    })).unwrap();
    let dir = ld_lucivy::directory::RamDirectory::default();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        w.set_merge_policy(Box::new(ld_lucivy::indexer::NoMergePolicy));
        for (i, text) in docs.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(content_f, text);
            w.add_document(doc).unwrap();
            if (i + 1) % commit_every == 0 { w.commit().unwrap(); }
        }
        w.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

/// Two-level merge: groups of `group` segments first, then everything — so a
/// merged segment is itself an input of a merge, as in a real progressive policy.
fn merge_all(handle: &LucivyHandle, group: usize) -> (usize, usize) {
    let before = handle.reader.searcher().segment_readers().len();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        let mut ids = handle.index.searchable_segment_ids().unwrap();
        ids.sort();
        let groups: Vec<Vec<_>> = ids.chunks(group).filter(|c| c.len() > 1).map(|c| c.to_vec()).collect();
        w.merge_many(&groups).unwrap();
        w.commit().unwrap();
        let ids = handle.index.searchable_segment_ids().unwrap();
        eprintln!("  level 1: {before} -> {} segments", ids.len());
        w.merge(&ids).unwrap();
        w.commit().unwrap();
    }
    {
        let mut guard = handle.writer.lock().unwrap();
        if let Some(w) = guard.take() { w.wait_merging_threads().unwrap(); }
    }
    handle.reader.reload().unwrap();
    (before, handle.reader.searcher().segment_readers().len())
}

/// `merge(A, B)` must answer exactly like `index(A ∪ B)` — same documents,
/// same spans, strict and relaxed.
///
/// The 50k kernel bench showed a 32-segment merged index missing 11 spans on
/// `include` that the unmerged index of the same corpus found. The merge tests
/// in `sfx_dag_v3.rs` only check that the output is self-consistent; this one
/// checks it against the reference the merge is supposed to reproduce.
/// `V3_MERGE_DOCS` sizes the corpus, `V3_CORPUS` points at a source tree.
#[test]
fn v3_merge_equals_fresh_by_spans() {
    let docs = merge_corpus();
    let n = docs.len();
    let fresh = handle_with_commits(&docs, usize::MAX);
    let merged = handle_with_commits(&docs, (n / 8).max(1));
    let (before, after) = merge_all(&merged, 8);
    let n_fresh = fresh.reader.searcher().segment_readers().len();
    // One commit still yields one segment per indexing thread; the reference
    // is "never merged", not "one segment".
    eprintln!("  docs={n} fresh segments={n_fresh} merged {before} -> {after}");
    assert!(after < before, "merge did not happen");

    let queries = [
        "include", "__init", "uint64_t", "spin_lock", "function", "struct",
        "static int", "net_device", "kmalloc", "->", "zzqqxxyyww",
    ];
    let mut failures = Vec::new();
    for strict in [true, false] {
        for q in queries {
            let a = doc_spans(&fresh, q, strict, n);
            let b = doc_spans(&merged, q, strict, n);
            let mode = if strict { "strict" } else { "relax" };
            let na: usize = a.values().map(|v| v.len()).sum();
            let nb: usize = b.values().map(|v| v.len()).sum();
            let mut line = format!("  {q:<12} {mode:<6} fresh docs={:<4} spans={na:<6} merged docs={:<4} spans={nb:<6}", a.len(), b.len());
            if strict {
                let g: usize = docs.iter().map(|d| grep_strict(d, q).len()).sum();
                line.push_str(&format!(" grep spans={g}"));
            }
            eprintln!("{line}");
            if a != b {
                for nid in a.keys().chain(b.keys()).collect::<std::collections::BTreeSet<_>>() {
                    let sa = a.get(nid).cloned().unwrap_or_default();
                    let sb = b.get(nid).cloned().unwrap_or_default();
                    if sa != sb {
                        let miss: Vec<_> = sa.iter().filter(|s| !sb.contains(s)).collect();
                        let extra: Vec<_> = sb.iter().filter(|s| !sa.contains(s)).collect();
                        let ctx = |s: &[usize; 2]| {
                            let d = &docs[*nid as usize];
                            let lo = s[0].saturating_sub(12);
                            let hi = (s[1] + 12).min(d.len());
                            let lo = (lo..=s[0]).find(|&i| d.is_char_boundary(i)).unwrap_or(s[0]);
                            let hi = (s[1]..=hi).rev().find(|&i| d.is_char_boundary(i)).unwrap_or(s[1]);
                            d[lo..hi].replace('\n', "\\n")
                        };
                        let show = |v: &[&[usize; 2]]| v.iter().take(4)
                            .map(|s| format!("{:?} {:?}", s, ctx(s))).collect::<Vec<_>>().join(", ");
                        eprintln!("    doc {nid}: merged misses {} [{}] / extra {} [{}]",
                                  miss.len(), show(&miss), extra.len(), show(&extra));
                    }
                }
                failures.push(format!("{q} {mode}"));
            }
        }
    }
    assert!(failures.is_empty(), "merged index differs from fresh on: {failures:?}");
}

/// One 0x02 key, several word shapes: "init" is the word `init`, or `in` +
/// overlap `it`, or `in` + overlap `i` + … — each with its own content length.
/// Interned under one ordinal they shared the first occurrence's metadata, and
/// which occurrence came first depended on segment order, so a merged index
/// and a fresh one disagreed (and both were sometimes wrong). Each shape now
/// gets its own ordinal; the posting carries the content end (WSP2).
#[test]
fn v3_word_shapes_share_key_not_ordinal() {
    let docs: Vec<String> = [
        "init the module",
        "bugs in it even",
        "events in i-th last",
        "in_i, todo",
        "security bugs in it even\n * harmless",
    ].iter().map(|s| s.to_string()).collect();
    let expect: std::collections::BTreeMap<u64, Vec<[usize; 2]>> = [
        (0u64, vec![[0usize, 4usize]]),
        (1, vec![[5, 10]]),
        (2, vec![[7, 13]]),
        (3, vec![[0, 7]]),
        (4, vec![[14, 19]]),
    ].into_iter().collect();

    let fresh = handle_with_commits(&docs, usize::MAX);
    assert_eq!(doc_spans(&fresh, "__init", false, docs.len()), expect, "fresh index");

    // One document per segment, in every order the merge could see them.
    let merged = handle_with_commits(&docs, 1);
    let (before, after) = merge_all(&merged, 2);
    assert!(after < before);
    assert_eq!(doc_spans(&merged, "__init", false, docs.len()), expect, "merged index");

    let reversed: Vec<String> = docs.iter().rev().cloned().collect();
    let merged_rev = handle_with_commits(&reversed, 1);
    merge_all(&merged_rev, 2);
    let got = doc_spans(&merged_rev, "__init", false, docs.len());
    let expect_rev: std::collections::BTreeMap<u64, Vec<[usize; 2]>> = expect.iter()
        .map(|(k, v)| ((docs.len() - 1) as u64 - k, v.clone())).collect();
    assert_eq!(got, expect_rev, "merged index, reversed insertion order");
}

#[test]
fn v3_merge_non_ascii_neighbours() {
    let docs: Vec<String> = [
        "结构体（struct）、共用",
        "序通常在spinlock保护的临",
        "plain struct here",
    ].iter().map(|s| s.to_string()).collect();
    let fresh = handle_with_commits(&docs, usize::MAX);
    let merged = handle_with_commits(&docs, 1);
    merge_all(&merged, 2);
    for (q, strict) in [("struct", true), ("spin_lock", false)] {
        let a = doc_spans(&fresh, q, strict, docs.len());
        let b = doc_spans(&merged, q, strict, docs.len());
        eprintln!("{q} strict={strict}: fresh={a:?} merged={b:?}");
        assert_eq!(a, b, "{q}");
    }
}

/// Delta-debugging helper: shrink the set of "other" documents for which a
/// merged index disagrees with a fresh one on `target`. Opt-in, prints the
/// minimal corpus.
#[test]
#[ignore]
fn v3_merge_bisect() {
    let target = std::env::var("V3_BISECT_TARGET").unwrap();
    let query = std::env::var("V3_BISECT_QUERY").unwrap_or("spin_lock".into());
    let strict = std::env::var("V3_BISECT_STRICT").is_ok();
    let corpus = merge_corpus();
    let target_text = std::fs::read_to_string(&target).unwrap();
    let diverges = |others: &[String]| -> bool {
        let mut docs = others.to_vec();
        docs.push(target_text.clone());
        let fresh = handle_with_commits(&docs, usize::MAX);
        let grep_mode = std::env::var("V3_BISECT_GREP").is_ok();
        let merged = handle_with_commits(&docs, if grep_mode { usize::MAX } else { (docs.len() / 8).max(1) });
        if !grep_mode { merge_all(&merged, 8); }
        let tid = (docs.len() - 1) as u64;
        let a = doc_spans(&fresh, &query, strict, docs.len()).remove(&tid).unwrap_or_default();
        // V3_BISECT_GREP: shrink for "fresh differs from grep" (strict only)
        // instead of "fresh differs from merged".
        let b = if std::env::var("V3_BISECT_GREP").is_ok() {
            grep_strict(&target_text, &query)
        } else {
            doc_spans(&merged, &query, strict, docs.len()).remove(&tid).unwrap_or_default()
        };
        a != b
    };
    let mut others: Vec<String> = corpus.into_iter().filter(|d| *d != target_text).collect();
    assert!(diverges(&others), "no divergence on the full corpus");
    let mut chunk = others.len() / 2;
    while chunk >= 1 {
        let mut i = 0;
        let mut progressed = false;
        while i < others.len() {
            let end = (i + chunk).min(others.len());
            let mut trial = others.clone();
            trial.drain(i..end);
            if !trial.is_empty() && diverges(&trial) {
                others = trial;
                progressed = true;
            } else {
                i = end;
            }
        }
        eprintln!("  chunk {chunk}: {} others remain", others.len());
        if !progressed { chunk /= 2; }
    }
    eprintln!("MINIMAL: {} other docs", others.len());
    for (i, d) in others.iter().enumerate() {
        let p = format!("/tmp/bisect_other_{i}.txt");
        std::fs::write(&p, d).unwrap();
        eprintln!("  {p} ({} bytes)", d.len());
    }
    std::fs::write("/tmp/bisect_target.txt", &target_text).unwrap();
}

#[test]
#[ignore]
fn v3_merge_repro_files() {
    let files: Vec<String> = std::env::var("V3_REPRO_FILES").unwrap().split(',').map(String::from).collect();
    let docs: Vec<String> = files.iter().map(|f| std::fs::read_to_string(f).unwrap()).collect();
    let query = std::env::var("V3_BISECT_QUERY").unwrap_or("spin_lock".into());
    let strict = std::env::var("V3_BISECT_STRICT").is_ok();
    let fresh = handle_with_commits(&docs, usize::MAX);
    eprintln!("===== FRESH");
    let a = doc_spans(&fresh, &query, strict, docs.len());
    let merged = handle_with_commits(&docs, 1);
    merge_all(&merged, 8);
    eprintln!("===== MERGED");
    let b = doc_spans(&merged, &query, strict, docs.len());
    eprintln!("fresh={a:?}\nmerged={b:?}");
    assert_eq!(a, b);
}

#[test]
fn v3_span_non_ascii_neighbours() {
    let cases: &[(&str, &str, bool)] = &[
        ("殊的部分；用 ``__init`` 标记的函数和", "__init", true),
        ("锁可以通过使用spin_lock_irqsave()\n或spin_l", "spin_lock", true),
        ("Drivers using affinity‑managed interrupts", "__init", false),
        ("namespace rag3db\n", "rag3db", true),
        ("see rag3db → here", "rag3db", true),
        ("MTHCA_TRANS_INIT2INIT,\n", "init", true),
    ];
    let mut bad = Vec::new();
    for (doc, q, strict) in cases {
        let handle = make_handle(&[doc]);
        let spans = spans_for(&handle, q, *strict);
        let needle: String = if *strict { q.to_lowercase() } else { q.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase() };
        let expect = if *strict { doc.to_lowercase().find(&needle).map(|s| [s, s + needle.len()]) } else { None };
        eprintln!("{q:?} strict={strict} in {doc:?}: spans={spans:?} expect={expect:?}");
        let all: Vec<[usize; 2]> = if *strict { grep_strict(doc, q) } else { Vec::new() };
        if spans.is_empty() { bad.push(format!("{q} in {doc:?}")); }
        else if *strict && spans != all { bad.push(format!("{q} in {doc:?}: {spans:?} != {all:?}")); }
        let _ = expect;
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
#[ignore]
fn v3_a2_probe() {
    let full = std::fs::read_to_string("/tmp/a2_line.txt").unwrap();
    let chars: Vec<char> = full.chars().collect();
    let q = "__init";
    for cut in 0..chars.len() {
        let doc: String = chars[cut..].iter().collect();
        let Some(first) = doc.find(q) else { break };
        let handle = make_handle(&[&doc]);
        let spans = spans_for(&handle, q, true);
        let ok = spans.iter().any(|s| s[0] == first);
        eprintln!("cut={cut:>3} first_at={first:>3} found={ok} spans={spans:?} doc={:?}", doc.chars().take(20).collect::<String>());
    }
}

#[test]
#[ignore]
fn v3_a2_chunks() {
    use ld_lucivy::tokenizer::equal_chunk::{segment_and_chunk, DEFAULT_MAX_TOKEN};
    let full = std::fs::read_to_string("/tmp/a2_line.txt").unwrap();
    let chars: Vec<char> = full.chars().collect();
    for cut in [0usize, 1] {
        let doc: String = chars[cut..].iter().collect();
        eprintln!("--- cut={cut}");
        for (i, (t, m)) in segment_and_chunk(&doc, DEFAULT_MAX_TOKEN).iter().enumerate() {
            eprintln!("  {i:>2} {t:?} content={} sep={} ws={} word={}", m.content_len, m.sep_len, m.is_word_start, m.word_id);
        }
    }
}

/// Merges driven by the writer's own policy at commit time (wired on
/// 23 August) must lose nothing: every document, every span.
#[test]
fn v3_policy_merges_preserve_everything() {
    let docs = merge_corpus();
    let n = docs.len();
    let fresh = handle_with_commits(&docs, usize::MAX);

    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3
    })).unwrap();
    let dir = ld_lucivy::directory::RamDirectory::default();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid_f = handle.field(NODE_ID_FIELD).unwrap();
    {
        let mut guard = handle.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        let mut policy = ld_lucivy::indexer::LogMergePolicy::default();
        policy.set_min_num_segments(3);
        policy.set_max_docs_before_merge(n / 3);
        policy.set_max_merged_docs(Some(n / 3));
        w.set_merge_policy(Box::new(policy));
        for (i, text) in docs.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, i as u64);
            doc.add_text(content_f, text);
            w.add_document(doc).unwrap();
            if (i + 1) % 20 == 0 {
                w.commit().unwrap();
                if std::env::var("V3_DRAIN_EACH").is_ok() { w.drain_merges().unwrap(); }
            }
        }
        w.commit().unwrap();
        w.drain_merges().unwrap();
    }
    handle.reader.reload().unwrap();
    let searcher = handle.reader.searcher();
    let mut sizes: Vec<u32> = searcher.segment_readers().iter().map(|r| r.num_docs()).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let total: u32 = sizes.iter().sum();
    eprintln!("  docs={n} policy-merged segments={} sizes={sizes:?}", sizes.len());
    assert_eq!(total as usize, n, "documents lost by policy merges");

    for (q, strict) in [("include", true), ("__init", false), ("struct", true), ("->", true)] {
        let a = doc_spans(&fresh, q, strict, n);
        let b = doc_spans(&handle, q, strict, n);
        eprintln!("  {q:<8} strict={strict} fresh docs={} merged docs={}", a.len(), b.len());
        if a != b {
            // Where are the holes: per segment, expected hits (by text) vs found.
            use ld_lucivy::schema::document::Value;
            for (si, sr) in searcher.segment_readers().iter().enumerate() {
                let store = sr.get_store_reader(0).unwrap();
                let mut expect = 0; let mut ids = Vec::new();
                for d in 0..sr.max_doc() {
                    let doc: ld_lucivy::LucivyDocument = store.get(d).unwrap();
                    let nid = doc.field_values().find(|(f, _)| *f == nid_f)
                        .and_then(|(_, v)| v.as_value().as_u64()).unwrap();
                    if a.contains_key(&nid) { expect += 1; if !b.contains_key(&nid) { ids.push(nid); } }
                }
                eprintln!("    seg {si}: docs={} expected hits={expect} missing={} {:?}", sr.max_doc(), ids.len(), &ids[..ids.len().min(10)]);
            }
        }
        assert_eq!(a, b, "{q} strict={strict}");
    }
}

fn fuzzy_spans_for(handle: &LucivyHandle, value: &str, distance: u8) -> Vec<[usize; 2]> {
    let config = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        distance: Some(distance),
        strict_separators: Some(false),
        ..Default::default()
    };
    let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
    let query = query::build_query(&config, &handle.schema, &handle.index, Some(Arc::clone(&sink))).unwrap();
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let mut out = Vec::new();
    for (_, addr) in &results {
        let seg_id = searcher.segment_reader(addr.segment_ord).segment_id();
        if let Some(m) = sink.get(seg_id, addr.doc_id) {
            out.extend(m.get("content").cloned().unwrap_or_default());
        }
    }
    out.sort();
    out
}

/// Fuzzy occurrences inside long tokens, at the token's middle or end.
#[test]
fn v3_fuzzy_span_inside_long_token() {
    let cases: &[(&str, &str, u8)] = &[
        ("on(AllSPDestinationsFunction::getAlgorithm());", "functin", 1),
        ("Function→CALLS→Function, Class→INHERITS", "functin", 1),
        ("cast(12, \"UINT64\"), cast(4324.123, ", "uint64", 1),
        ("ions(expressionsBeforePruning);\n    if (expres", "retrun", 1),
        ("plain functin here", "functin", 1),
    ];
    let mut bad = Vec::new();
    for (doc, q, d) in cases {
        let handle = make_handle(&[doc]);
        let got = fuzzy_spans_for(&handle, q, *d);
        // Expected: the shared definition on the stripped, lowercased text.
        let mut stripped = Vec::new(); let mut back = Vec::new();
        for (off, ch) in doc.char_indices() {
            if !ld_lucivy::tokenizer::equal_chunk::is_content_char(ch) { continue; }
            for lc in ch.to_lowercase() { let mut b = [0u8; 4]; for x in lc.encode_utf8(&mut b).bytes() { stripped.push(x); back.push(off); } }
        }
        let expect: Vec<[usize; 2]> = ld_lucivy::suffix_fst::briques::fuzzy_spans::fuzzy_spans(q.as_bytes(), &stripped, *d as usize)
            .into_iter().map(|(s, e, _)| { let last = back[e - 1]; [back[s], last + doc[last..].chars().next().unwrap().len_utf8()] }).collect();
        eprintln!("{q:?} d={d} in {doc:?}: got={got:?} expect={expect:?}");
        if got != expect { bad.push(format!("{q} in {doc:?}: {got:?} != {expect:?}")); }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// Relaxed ground truth on a synthetic text: lowercase, separators stripped,
/// every occurrence mapped back to source bytes (same rules as the harness).
fn grep_relaxed(text: &str, needle: &str) -> Vec<[usize; 2]> {
    let nd: String = needle.to_lowercase().chars()
        .filter(|c| ld_lucivy::tokenizer::equal_chunk::is_content_char(*c)).collect();
    let mut stripped = Vec::new(); let mut back = Vec::new();
    for (off, ch) in text.char_indices() {
        if !ld_lucivy::tokenizer::equal_chunk::is_content_char(ch) { continue; }
        for lc in ch.to_lowercase() { let mut b = [0u8; 4]; for x in lc.encode_utf8(&mut b).bytes() { stripped.push(x); back.push(off); } }
    }
    let mut out = Vec::new();
    let n = nd.as_bytes();
    if n.is_empty() || stripped.len() < n.len() { return out; }
    for s in 0..=stripped.len() - n.len() {
        if &stripped[s..s + n.len()] == n {
            let last = back[s + n.len() - 1];
            out.push([back[s], last + text[last..].chars().next().unwrap().len_utf8()]);
        }
    }
    out
}

/// Deterministic SKU / identifier corpus: the shapes a product catalogue or a
/// log file throws at a substring engine — long digit runs, separators inside
/// tokens, identifiers far longer than the 264-byte word-entry limit, matches
/// at every position of those, and occurrences straddling separator runs.
fn sku_corpus() -> Vec<String> {
    let mut docs = Vec::new();
    let mut seed = 12345u64;
    let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    for i in 0..200 {
        let mut line = String::new();
        for _ in 0..20 {
            let r = rnd();
            let kind = r % 6;
            match kind {
                0 => line.push_str(&format!("SKU-{:05}-{}{}", r % 100_000, (b'A' + (r % 26) as u8) as char, (b'A' + ((r / 26) % 26) as u8) as char)),
                1 => line.push_str(&format!("ABC{:08}", r % 100_000_000)),
                2 => line.push_str(&format!("ref_{}_{}", r % 1000, r % 77)),
                3 => line.push_str(&format!("{}", r % 10_000_000_000)),
                4 => line.push_str(&format!("part/{}/{}", r % 500, (r / 500) % 500)),
                _ => line.push_str("widget"),
            }
            line.push_str(match r % 4 { 0 => " ", 1 => ", ", 2 => "\n\t", _ => " | " });
        }
        // One very long identifier per 10 docs: a 400-byte hex token with a
        // marker planted deep inside it.
        if i % 10 == 0 {
            let mut long = String::new();
            for k in 0..50 { long.push_str(&format!("{:08x}", rnd() ^ k)); }
            let at = 300 + (i * 7) % 80;
            long.replace_range(at..at + 8, "deepmark");
            line.push_str(&long);
            line.push('\n');
        }
        docs.push(line);
    }
    docs
}

#[test]
fn v3_relaxed_sku_corpus_matches_grep() {
    let docs = sku_corpus();
    let handle = handle_with_commits(&docs, 25);
    let queries = ["SKU-0", "ABC00", "ref_1", "widget", "part/1", "deepmark", "00000", "AB", "0-1", "mark", "SKU", "et, A"];
    let mut bad = Vec::new();
    for q in queries {
        let got = doc_spans(&handle, q, false, docs.len());
        let mut expect = std::collections::BTreeMap::new();
        for (i, d) in docs.iter().enumerate() {
            let sp = grep_relaxed(d, q);
            if !sp.is_empty() { expect.insert(i as u64, sp); }
        }
        let ng: usize = got.values().map(|v| v.len()).sum();
        let ne: usize = expect.values().map(|v| v.len()).sum();
        eprintln!("  {q:<10} docs got={} expect={} spans got={ng} expect={ne}", got.len(), expect.len());
        if got != expect {
            for (k, v) in &expect {
                let g = got.get(k).cloned().unwrap_or_default();
                if g != *v {
                    let miss: Vec<_> = v.iter().filter(|s| !g.contains(s)).take(3).collect();
                    let extra: Vec<_> = g.iter().filter(|s| !v.contains(s)).take(3).collect();
                    eprintln!("    doc {k}: miss {miss:?} extra {extra:?}");
                    break;
                }
            }
            bad.push(q);
        }
    }
    assert!(bad.is_empty(), "relaxed differs from grep on {bad:?}");
}

/// Strict literals that enter their first token through its separator zone
/// and then run over three chunks: `<const TARGET*>` over
/// `cast<const TARGET*>(this)`. Found by the rag3db coherence panel (0 of
/// 17 files), along with `<binder::Expression>`.
#[test]
fn v3_strict_sep_head_three_chunks() {
    let docs = [
        "return common::ku_dynamic_cast<const TARGET*>(this);",
        "std::shared_ptr<binder::Expression> query;",
        "auto x = f<T>(y);",
        // Noise: other continuations of the same words, so the walk has
        // several alternatives per position like on a real corpus.
        "const TARGET target) targeti TARGET; TARGETCB targets targetTy",
        "binder::ExpressionType binder::Expressions Expression; ExpressionUtil",
        "const Target* const TARGET& <const TARGET>",
        // The real layout from rag3db function.h, where the panel missed it.
        "    template<class TARGET>\n    const TARGET* constPtrCast() const {\n        return common::ku_dynamic_cast<const TARGET*>(this);\n    }\n    template<class TARGET>\n    TARGET* ptrCast() {\n        return common::ku_dynamic_cast<TARGET*>(this);\n    }\n",
        // Real layout from rag3db gds.h (61 files missed).
        "    static std::shared_ptr<binder::Expression> bindRelOutput(const TableFuncBindInput& bindInput,\n        std::shared_ptr<binder::NodeExpression> dstNode,\n        const std::optional<std::string>& name = std::nullopt);\n    static std::shared_ptr<binder::Expression> bindNodeOutput(const TableFuncBindInput& bindInput,\n",
    ];
    let handle = make_handle(&docs);
    for q in [
        "<const TARGET*>", "const TARGET*>", "<const TARGET", "<const TARGET*",
        "<binder::Expression>", "<binder::Expression", "binder::Expression>", "<T>(y",
    ] {
        let spans = doc_spans(&handle, q, true, docs.len());
        let got: Vec<(u64, Vec<[usize; 2]>)> = spans.into_iter().collect();
        let expect: Vec<(u64, Vec<[usize; 2]>)> = docs.iter().enumerate()
            .map(|(i, d)| (i as u64, grep_strict(d, q)))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        assert_eq!(got, expect, "strict {q:?}");
    }
}

/// Migration: what happens to yesterday's v2 indexes now that new indexes
/// default to v3.
///
/// Three promises, each asserted here:
/// 1. a fresh index with no `sfx_version` in its config builds v3;
/// 2. an existing v2 index (meta.json without the field) reopened by the new
///    code keeps building v2 segments — nothing changes behind the user;
/// 3. an index whose meta.json was switched to 3 (the migration gesture)
///    keeps its old v2 segments readable next to new v3 ones: a search
///    returns the union, and `query_warnings` names the v2 segments.
#[test]
fn v3_migration_from_v2_index() {
    let scratch = std::env::var("V3_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let dir_path = format!("{scratch}/v3_migration");
    let _ = std::fs::remove_dir_all(&dir_path);
    std::fs::create_dir_all(&dir_path).unwrap();

    // 1. Fresh index, no sfx_version in the config → v3.
    let plain_config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}]
    })).unwrap();
    {
        let h = LucivyHandle::create(ld_lucivy::directory::RamDirectory::default(), &plain_config).unwrap();
        assert_eq!(h.index.settings().sfx_version, 3, "new indexes must default to v3");
    }

    // 2. A v2 index on disk...
    let v2_config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 2
    })).unwrap();
    let add_commit = |h: &LucivyHandle, from: u64, texts: &[&str]| {
        let content_f = h.field("content").unwrap();
        let nid_f = h.field(NODE_ID_FIELD).unwrap();
        let mut guard = h.writer.lock().unwrap();
        let w = guard.as_mut().unwrap();
        for (i, t) in texts.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_f, from + i as u64);
            doc.add_text(content_f, t);
            w.add_document(doc).unwrap();
        }
        w.commit().unwrap();
        w.drain_merges().unwrap();
        drop(guard);
        h.reader.reload().unwrap();
    };
    {
        let dir = lucivy_core::directory::StdFsDirectory::open(&dir_path).unwrap();
        let h = LucivyHandle::create(dir, &v2_config).unwrap();
        add_commit(&h, 0, &["mutex_lock in the old segment", "spin_lock everywhere"]);
        h.close().unwrap();
    }
    // meta.json of a v2 index: the field is written explicitly since v3
    // became the default; older files omitted it. Erase it to simulate a
    // genuinely old index, and check it reads back as 2.
    let meta_path = format!("{dir_path}/meta.json");
    let set_meta_version = |v: Option<u64>| {
        let mut m: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let settings = m.get_mut("index_settings").unwrap().as_object_mut().unwrap();
        match v {
            Some(v) => { settings.insert("sfx_version".into(), v.into()); }
            None => { assert!(settings.remove("sfx_version").is_some(),
                "expected the field in a fresh v2 meta.json"); }
        }
        std::fs::write(&meta_path, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    };
    set_meta_version(None);

    // ...reopened by the new code: still v2, still searchable, new segments v2.
    {
        let dir = lucivy_core::directory::StdFsDirectory::open(&dir_path).unwrap();
        let h = LucivyHandle::open(dir).unwrap();
        assert_eq!(h.index.settings().sfx_version, 2,
            "an old meta.json without the field is a v2 index");
        add_commit(&h, 10, &["mutex_lock in a second v2 segment"]);
        let docs = search(&h, "contains", "mutex_lock");
        assert_eq!(docs.len(), 2, "v2 index reopened: {docs:?}");
        let versions = h.sfx_versions();
        // The v2 writer produces the original file layout, which
        // detect_sfx_version labels 1; only "not 3" matters here.
        assert!(versions.iter().all(|v| *v != Some(3)), "{versions:?}");
        h.close().unwrap();
    }

    // 3. The migration gesture: switch meta.json to 3, then keep working.
    set_meta_version(Some(3));
    {
        let dir = lucivy_core::directory::StdFsDirectory::open(&dir_path).unwrap();
        let h = LucivyHandle::open(dir).unwrap();
        assert_eq!(h.index.settings().sfx_version, 3);
        add_commit(&h, 20, &["mutex_lock arrives in a v3 segment"]);

        let versions = h.sfx_versions();
        assert!(versions.contains(&Some(3)) && versions.iter().any(|v| *v != Some(3)),
            "expected a mixed index, got {versions:?}");

        // Union across formats, for the type the compat layer routes.
        let docs = search(&h, "contains", "mutex_lock");
        assert_eq!(docs.len(), 3, "mixed index must return both formats: {docs:?}");
        let docs = search(&h, "startsWith", "mutex");
        assert_eq!(docs.len(), 3, "{docs:?}");

        // The user is told, not left to wonder.
        let config = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("mutex_lock".into()),
            ..Default::default()
        };
        let w = h.query_warnings(&config);
        assert!(w.iter().any(|m| m.contains("v2 indexer")), "{w:?}");
        h.close().unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir_path);
}

/// Unicode grep with the engine's folding rules, for the tests below: char
/// by char lowercase (a fold can change byte LENGTH: Kelvin K → k, İ → i̇),
/// separators stripped in relaxed mode, spans on the source bytes.
fn grep_fold(text: &str, needle: &str, strict: bool) -> Vec<[usize; 2]> {
    use ld_lucivy::tokenizer::equal_chunk::is_content_char;
    let fold = |t: &str| -> (String, Vec<(usize, usize)>) {
        let mut out = String::new();
        let mut back = Vec::new();
        for (off, ch) in t.char_indices() {
            if !strict && !is_content_char(ch) { continue; }
            for lc in ch.to_lowercase() {
                let start = out.len();
                out.push(lc);
                for _ in start..out.len() { back.push((off, ch.len_utf8())); }
            }
        }
        (out, back)
    };
    let (hay, back) = fold(text);
    let (nd, _) = fold(needle);
    let mut spans = Vec::new();
    if nd.is_empty() { return spans; }
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&nd) {
        let at = from + rel;
        let (a, _) = back[at];
        let (b, n) = back[at + nd.len() - 1];
        spans.push([a, b + n]);
        from = at + 1;
        while from < hay.len() && !hay.is_char_boundary(from) { from += 1; }
    }
    spans
}

/// Case folds that change byte length used to shift spans: suffix indexes
/// are counted on the LOWERCASED key and were applied to source bytes, so
/// `->` in `'K' -> 'K'` (Kelvin sign, 3 bytes → `k`, 1) came out two bytes
/// early (re2's unicode_casefold.h, coherence panel, 23 August). Same for
/// `İ` (2 bytes → `i̇`, 3) and for plain uppercase accents (`DÉJÀ`, same
/// length — must simply match). Empirically fixed on the corpus; pinned
/// here without a corpus.
#[test]
fn v3_case_fold_length_changes() {
    let docs = [
        "//     'k' -> '\u{212A}'  (Kelvin symbol)\n//     '\u{212A}' -> 'K'\n",
        "\u{130}stanbul kelvin i\u{307}stanbul plain",
        "DÉJÀ vu and déjà encore",
        "no funny chars -> here",
    ];
    let handle = make_handle(&docs);
    for (q, strict) in [
        ("->", true),
        ("déjà", true), ("déjà", false),
        ("kelvin", true),
        ("i\u{307}stanbul", false),
    ] {
        let spans = doc_spans(&handle, q, strict, docs.len());
        let expect: std::collections::BTreeMap<u64, Vec<[usize; 2]>> = docs.iter().enumerate()
            .map(|(i, d)| (i as u64, grep_fold(d, q, strict)))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        assert_eq!(spans, expect, "{q:?} strict={strict}");
    }
}

/// Fuzzy and regex through ShardedHandle must reach EVERY shard: the search
/// DAG used to leave their prescan to `weight()`, which saw shard 0 only —
/// a 4-shard index returned a quarter of the fuzzy results, and regex
/// tripped `bm25::idf` (global doc_freq vs one shard's doc count). Found on
/// the rag3db distributed panel, 23 August; pinned here without a corpus.
#[test]
fn v3_sharded_fuzzy_regex_reach_all_shards() {
    use lucivy_core::sharded_handle::{ShardedHandle, RamShardStorage};
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3,
        "shards": 4
    })).unwrap();
    let h = ShardedHandle::create_with_storage(Box::new(RamShardStorage::new()), &config).unwrap();
    let content_f = h.field("content").unwrap();
    let nid_f = h.field(NODE_ID_FIELD).unwrap();
    let n_docs = 16u64;
    for i in 0..n_docs {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, i);
        doc.add_text(content_f, &format!("kmalloc buffer_{i:04} spin_lock"));
        h.add_document(doc, i).unwrap();
    }
    h.commit().unwrap();

    let q = |value: &str, distance: Option<u8>, regex: bool| QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        distance,
        regex: if regex { Some(true) } else { None },
        ..Default::default()
    };
    // Every document matches each query; anything less means a shard was
    // skipped. And none of these may panic in idf.
    for (label, config) in [
        ("contains", q("kmalloc", None, false)),
        ("fuzzy d=1", q("kmallc", Some(1), false)),
        ("regex", q("buffer_[0-9]{4}", None, true)),
        ("regex full scan", q("[0-9]{4}", None, true)),
    ] {
        let r = h.search(&config, 1000, None).unwrap();
        assert_eq!(r.len(), n_docs as usize, "{label}: {} of {n_docs}", r.len());
        let shards: std::collections::HashSet<usize> = r.iter().map(|x| x.shard_id).collect();
        assert!(shards.len() > 1, "{label}: all results from shard(s) {shards:?}");
    }
}
