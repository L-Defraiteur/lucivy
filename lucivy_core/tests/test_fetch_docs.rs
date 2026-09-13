//! `ShardedHandle::fetch_docs` — the documents of a hit list, in the list's
//! order, whether the list is short (sequential loop) or long (one scheduler
//! task per shard and segment). Both must give exactly what one `searcher.doc`
//! per hit gives.
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::{RamShardStorage, ShardedHandle, PARALLEL_FETCH_MIN_HITS};

fn build(n: u64, shards: usize) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            { "name": "path", "type": "text", "stored": true },
            { "name": "content", "type": "text", "stored": true }
        ],
        "shards": shards,
    })).unwrap();
    let h = ShardedHandle::create_with_storage(Box::new(RamShardStorage::new()), &config).unwrap();
    for i in 0..n {
        // Several segments per shard: a commit every 40 documents.
        let filler = "x".repeat((i % 7) as usize * 300);
        h.add_document_json(i, &serde_json::json!({
            "path": format!("dir/file_{i}.c"),
            "content": format!("/* doc {i} */ mutex_lock(&dev->lock); {filler} /* end {i} */"),
        })).unwrap();
        if i % 40 == 39 {
            h.commit().unwrap();
        }
    }
    h.commit().unwrap();
    h
}

fn contains(value: &str) -> QueryConfig {
    serde_json::from_value(serde_json::json!({
        "type": "contains", "field": "content", "value": value
    })).unwrap()
}

/// `fetch_docs` against one `searcher.doc` per hit: same documents, same order.
fn check(h: &ShardedHandle, value: &str, top_k: usize) -> usize {
    let results = h.search(&contains(value), top_k, None).unwrap();
    let docs = h.fetch_docs(&results).unwrap();
    assert_eq!(docs.len(), results.len());
    let content = h.schema.get_field("content").unwrap();
    let path = h.schema.get_field("path").unwrap();
    let ids = h.node_ids_of(&results).unwrap();
    for ((r, doc), id) in results.iter().zip(&docs).zip(ids) {
        let searcher = h.shard(r.shard_id).unwrap().reader.searcher();
        let expected: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address).unwrap();
        let text = |d: &ld_lucivy::LucivyDocument, f| {
            use ld_lucivy::schema::document::Value;
            d.get_first(f).and_then(|v| v.as_value().as_str()).unwrap().to_string()
        };
        assert_eq!(text(doc, content), text(&expected, content));
        assert_eq!(text(doc, path), text(&expected, path));
        assert_eq!(text(doc, path), format!("dir/file_{id}.c"), "document and fast field agree");
    }
    results.len()
}

#[test]
fn fetch_docs_short_and_long_lists_match_one_by_one_reads() {
    let h = build(300, 3);
    let short = check(&h, "mutex_lock", 10);
    assert_eq!(short, 10);
    let long = check(&h, "mutex_lock", 1000);
    assert_eq!(long, 300);
    assert!(long >= PARALLEL_FETCH_MIN_HITS, "the long list must take the parallel path");
    assert!(check(&h, "doc 7 ", 10) >= 1);
    assert_eq!(check(&h, "nothing-here", 10), 0);
}

#[test]
fn fetch_docs_after_deletes_and_merges() {
    let h = build(200, 2);
    for i in (0..200).step_by(3) {
        h.delete_by_node_id(i).unwrap();
    }
    h.commit().unwrap();
    h.wait_merges_quiet().unwrap();
    let n = check(&h, "mutex_lock", 1000);
    assert_eq!(n, 200 - 67);
}
