//! Profile harness for the fixed cost of `ShardedHandle::commit()` on tiny
//! batches (reported by rag3weaver: ~0.6-1.2 s of pure waiting per dirty
//! commit, saturating with batch size). Run with:
//!
//!   cargo test --release -p lucivy-core --test test_commit_floor -- --ignored --nocapture

use lucistore::blob_store::MemBlobStore;
use lucivy_core::query::SchemaConfig;
use lucivy_core::sharded_handle::{BlobShardStorage, RamShardStorage, ShardedHandle};
use std::sync::Arc;
use std::time::Instant;

fn schema(shards: u32) -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "sfx_version": 3,
        "shards": shards
    }))
    .unwrap()
}

fn make_handle(shards: u32) -> ShardedHandle {
    ShardedHandle::create_with_storage(Box::new(RamShardStorage::new()), &schema(shards)).unwrap()
}

fn make_blob_handle(shards: u32, cache: &str) -> ShardedHandle {
    let _ = std::fs::remove_dir_all(cache);
    let store = Arc::new(MemBlobStore::new());
    let storage = BlobShardStorage::new(store, "commit_floor", cache);
    ShardedHandle::create_with_storage(Box::new(storage), &schema(shards)).unwrap()
}

fn add_docs(h: &ShardedHandle, n: u64, offset: u64) {
    let content_f = h.field("content").unwrap();
    let nid_f = h.field("_node_id").unwrap();
    for i in 0..n {
        let id = offset + i;
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid_f, id);
        doc.add_text(content_f, &format!("tiny document number {id} with a few words"));
        h.add_document(doc, id).unwrap();
    }
}

#[test]
#[ignore]
fn commit_floor_profile() {
    for shards in [1u32, 2, 4] {
        for n in [1u64, 9, 90, 900] {
            let h = make_handle(shards);
            add_docs(&h, n, 0);
            let t = Instant::now();
            h.commit().unwrap();
            let dirty = t.elapsed();
            let t = Instant::now();
            h.commit().unwrap();
            let clean = t.elapsed();
            println!(
                "shards={shards} docs={n:4} commit#1 {:8.1}ms  commit#2 {:6.1}ms",
                dirty.as_secs_f64() * 1e3,
                clean.as_secs_f64() * 1e3
            );
            h.close().unwrap();
        }
    }
}

#[test]
#[ignore]
fn commit_floor_profile_blob() {
    let scratch = std::env::var("V3_SCRATCH")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let cache = format!("{scratch}/commit_floor_cache");
    for shards in [2u32, 4] {
        for n in [1u64, 9, 90, 900] {
            let h = make_blob_handle(shards, &cache);
            add_docs(&h, n, 0);
            let t = Instant::now();
            h.commit().unwrap();
            let dirty = t.elapsed();
            let t = Instant::now();
            h.commit().unwrap();
            let clean = t.elapsed();
            println!(
                "blob shards={shards} docs={n:4} commit#1 {:8.1}ms  commit#2 {:6.1}ms",
                dirty.as_secs_f64() * 1e3,
                clean.as_secs_f64() * 1e3
            );
            h.close().unwrap();
        }
    }
}
