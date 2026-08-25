//! The highlight sink is bounded (`LUCIVY_HIGHLIGHT_SPAN_CAP`): scorers record
//! the spans of every document they verify, and a one-letter query over a large
//! corpus produced tens of millions of them — enough to take a 4 GB WebAssembly
//! heap down. Past the cap the sink overflows and `ShardedHandle` repeats the
//! search restricted to the ids it returned, so the top-k still carries every
//! span it would have had with an unbounded sink.

use std::collections::HashMap;
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const DOCS: usize = 300;
const SPANS_PER_DOC: usize = 3;
// Far below DOCS * SPANS_PER_DOC (the first pass overflows), above
// TOP_K * SPANS_PER_DOC (the repair pass does not).
const CAP: usize = 40;
const TOP_K: usize = 10;

fn build(path: &str) -> ShardedHandle {
    let _ = std::fs::remove_dir_all(path);
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            { "name": "path", "type": "text" },
            { "name": "content", "type": "text" }
        ],
        "shards": 2
    })).unwrap();
    let index = ShardedHandle::create(path, &config).unwrap();
    for i in 0..DOCS {
        index.add_document_json(i as u64, &serde_json::json!({
            "path": format!("src/file_{i}.c"),
            "content": format!("lock the spin lock and the mutex lock number {i}"),
        })).unwrap();
    }
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();
    index
}

fn query() -> QueryConfig {
    QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("lock".into()),
        ..Default::default()
    }
}

fn highlights_of(index: &ShardedHandle, sink: &HighlightSink, results: &[lucivy_core::sharded_handle::ShardedSearchResult])
    -> Vec<HashMap<String, Vec<[usize; 2]>>>
{
    results.iter().map(|r| {
        let shard = index.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg = searcher.segment_reader(r.doc_address.segment_ord);
        sink.get(seg.segment_id(), r.doc_address.doc_id).unwrap_or_default()
    }).collect()
}

#[test]
fn a_capped_sink_still_yields_complete_highlights_for_the_top_k() {
    // Also drives the sink `search_with_docs` builds by itself (read once per process).
    std::env::set_var("LUCIVY_HIGHLIGHT_SPAN_CAP", CAP.to_string());
    let index = build("/tmp/lucivy_test_highlight_cap");

    // Reference: unbounded sink.
    let full = Arc::new(HighlightSink::with_cap(usize::MAX));
    let full_results = index.search(&query(), TOP_K, Some(full.clone())).unwrap();
    assert_eq!(full_results.len(), TOP_K);
    assert!(!full.overflowed());
    assert_eq!(full.span_count(), DOCS * SPANS_PER_DOC, "every verified document records its spans");
    let full_hl = highlights_of(&index, &full, &full_results);
    for hl in &full_hl {
        assert_eq!(hl["content"].len(), SPANS_PER_DOC);
    }

    // Capped sink: the first pass overflows, the repair pass fills the top-k.
    let capped = Arc::new(HighlightSink::with_cap(CAP));
    let capped_results = index.search(&query(), TOP_K, Some(capped.clone())).unwrap();
    assert!(!capped.overflowed(), "repaired for the top-k only, under the cap");
    assert_eq!(capped.span_count(), TOP_K * SPANS_PER_DOC);
    let ids = |rs: &[lucivy_core::sharded_handle::ShardedSearchResult]| rs.iter()
        .map(|r| (r.shard_id, r.doc_address.segment_ord, r.doc_address.doc_id, r.score.to_bits()))
        .collect::<Vec<_>>();
    assert_eq!(ids(&capped_results), ids(&full_results), "scores and order come from the first pass");
    assert_eq!(highlights_of(&index, &capped, &capped_results), full_hl);

    // The sink `search_with_docs` creates follows the environment cap.
    assert_eq!(ld_lucivy::query::highlight_span_cap(), CAP);
    let hits = index.search_with_docs(&query(), TOP_K).unwrap();
    assert_eq!(hits.len(), TOP_K);
    for (hit, hl) in hits.iter().zip(&full_hl) {
        assert_eq!(&hit.highlights, hl);
    }

    // A caller-supplied filter is not repeated: a cap under the filtered
    // set's own spans leaves the highlights incomplete rather than looping.
    let tiny = Arc::new(HighlightSink::with_cap(2));
    let allowed = (0..TOP_K as u64).collect();
    let _ = index.search_filtered(&query(), TOP_K, Some(tiny.clone()), allowed).unwrap();
    assert!(tiny.overflowed());

    index.close().unwrap();
}
