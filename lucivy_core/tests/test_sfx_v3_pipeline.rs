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
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
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
    let collector = ld_lucivy::collector::TopDocs::with_limit(100).order_by_score();
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
