//! ACID/Blob storage with SFX v3, no external service: the blob store is
//! the source of truth, the mmap cache is disposable. What acid_postgres.rs
//! proves against a real Postgres (but only for v2, #[ignore]), this proves
//! for v3 on every CI run with MemBlobStore.

use std::collections::HashSet;
use std::sync::Arc;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::{ShardedHandle, BlobShardStorage};
use lucistore::blob_store::MemBlobStore;

fn config(shards: usize) -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3,
        "shards": shards
    })).unwrap()
}

fn docs() -> Vec<String> {
    (0..40).map(|i| format!(
        "doc {i}: std::shared_ptr<binder::Expression> expr_{i}; kmalloc(sizeof(x)); spin_lock_init(&l{i});"
    )).collect()
}

fn add_all(h: &ShardedHandle, docs: &[String]) {
    let content_f = h.field("content").unwrap();
    let nid_f = h.field(NODE_ID_FIELD).unwrap();
    for (i, d) in docs.iter().enumerate() {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, i as u64);
        doc.add_text(content_f, d);
        h.add_document(doc, i as u64).unwrap();
    }
    h.commit().unwrap();
}

fn search_ids(h: &ShardedHandle, q: &QueryConfig) -> HashSet<u64> {
    use ld_lucivy::schema::document::Value;
    let mut out = HashSet::new();
    for r in h.search(q, 1000, None).unwrap() {
        let shard = h.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let doc: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address).unwrap();
        let nid = shard.field(NODE_ID_FIELD).unwrap();
        let id = doc.field_values().find(|(f, _)| *f == nid)
            .and_then(|(_, v)| v.as_value().as_u64()).unwrap();
        out.insert(id);
    }
    out
}

fn q(value: &str, distance: Option<u8>, regex: bool) -> QueryConfig {
    QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(value.into()),
        distance,
        regex: if regex { Some(true) } else { None },
        ..Default::default()
    }
}

#[test]
fn v3_blob_storage_create_reopen_search() {
    let store = Arc::new(MemBlobStore::new());
    let scratch = std::env::var("V3_SCRATCH")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let cache_a = format!("{scratch}/blob_v3_cache_a");
    let cache_b = format!("{scratch}/blob_v3_cache_b");
    let _ = std::fs::remove_dir_all(&cache_a);
    let _ = std::fs::remove_dir_all(&cache_b);
    let docs = docs();
    let all: HashSet<u64> = (0..docs.len() as u64).collect();

    // Build over the blob store, then close: the durable state is ONLY the blobs.
    {
        let storage = BlobShardStorage::new(store.clone(), "acid_v3", &cache_a);
        let h = ShardedHandle::create_with_storage(Box::new(storage), &config(2)).unwrap();
        add_all(&h, &docs);
        assert_eq!(search_ids(&h, &q("shared_ptr<binder::Expression>", None, false)), all);
        h.close().unwrap();
    }
    // A different machine: fresh cache dir, same store.
    let _ = std::fs::remove_dir_all(&cache_a);
    {
        let storage = BlobShardStorage::new(store.clone(), "acid_v3", &cache_b);
        let h = ShardedHandle::open_with_storage(Box::new(storage)).unwrap();
        for (label, query, expect) in [
            ("strict long", q("shared_ptr<binder::Expression>", None, false), all.clone()),
            ("relaxed", q("spinlockinit", None, false), all.clone()),
            ("fuzzy d=1", q("kmalloc", Some(1), false), all.clone()),
            ("regex", q(r"expr_[0-9]+", None, true), all.clone()),
            ("one doc", q("expr_7;", None, false), HashSet::from([7])),
        ] {
            let got = search_ids(&h, &query);
            assert_eq!(got, expect, "{label}: {} docs instead of {}", got.len(), expect.len());
        }
        // And it can keep writing.
        let content_f = h.field("content").unwrap();
        let nid_f = h.field(NODE_ID_FIELD).unwrap();
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, 100);
        doc.add_text(content_f, "fresh after reopen kmalloc");
        h.add_document(doc, 100).unwrap();
        h.commit().unwrap();
        let got = search_ids(&h, &q("fresh after reopen", None, false));
        assert_eq!(got, HashSet::from([100]));
        h.close().unwrap();
    }
    let _ = std::fs::remove_dir_all(&cache_b);
}
