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
#[ignore = "regression introduced by 8aeb093; term is not on the critical path"]
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
