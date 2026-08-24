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

/// Every store call is a round trip for a database-backed store: count them
/// per phase so the cost model is explicit, whatever the store charges.
struct CountingStore {
    inner: MemBlobStore,
    calls: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
}
impl CountingStore {
    fn new() -> Self {
        Self { inner: MemBlobStore::new(), calls: std::sync::Mutex::new(Default::default()) }
    }
    fn note(&self, what: &str, file: &str) {
        let key = if file.ends_with(".managed.json") || file == "meta.json" {
            format!("{what} {file}")
        } else {
            format!("{what} <segment files>")
        };
        *self.calls.lock().unwrap().entry(key).or_insert(0) += 1;
    }
    fn drain(&self) -> String {
        let mut calls = self.calls.lock().unwrap();
        let s: Vec<String> = calls.iter().map(|(k, v)| format!("{v}× {k}")).collect();
        calls.clear();
        s.join(", ")
    }
}
impl lucistore::blob_store::BlobStore for CountingStore {
    fn load(&self, i: &str, f: &str) -> std::io::Result<Vec<u8>> {
        self.note("load", f); self.inner.load(i, f)
    }
    fn save(&self, i: &str, f: &str, d: &[u8]) -> std::io::Result<()> {
        self.note("save", f); self.inner.save(i, f, d)
    }
    fn delete(&self, i: &str, f: &str) -> std::io::Result<()> {
        self.note("delete", f); self.inner.delete(i, f)
    }
    fn exists(&self, i: &str, f: &str) -> std::io::Result<bool> {
        self.note("exists", f); self.inner.exists(i, f)
    }
    fn list(&self, i: &str) -> std::io::Result<Vec<String>> {
        self.note("list", ""); self.inner.list(i)
    }
}

#[test]
#[ignore]
fn commit_floor_store_calls() {
    let scratch = std::env::var("V3_SCRATCH")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let cache = format!("{scratch}/commit_floor_calls_cache");
    let _ = std::fs::remove_dir_all(&cache);
    let store = Arc::new(CountingStore::new());
    {
        let storage = BlobShardStorage::new(store.clone(), "commit_floor_calls", &cache);
        let h = ShardedHandle::create_with_storage(Box::new(storage), &schema(2)).unwrap();
        println!("create        : {}", store.drain());
        add_docs(&h, 9, 0);
        h.commit().unwrap();
        println!("commit 9 docs : {}", store.drain());
        h.commit().unwrap();
        println!("commit clean  : {}", store.drain());
        add_docs(&h, 9, 100);
        h.commit().unwrap();
        println!("commit 9 more : {}", store.drain());
        h.close().unwrap();
        println!("close         : {}", store.drain());
    }
    let storage = BlobShardStorage::new(store.clone(), "commit_floor_calls", &cache);
    let h = ShardedHandle::open_with_storage(Box::new(storage)).unwrap();
    println!("reopen        : {}", store.drain());
    add_docs(&h, 9, 200);
    h.commit().unwrap();
    println!("commit reopen : {}", store.drain());
    h.close().unwrap();
    println!("close         : {}", store.drain());
}

/// Reopen scenario (rag3weaver doc 19): create → add → commit → close,
/// then open the same store again and commit a small batch. Reported as
/// 25-40 s of waiting per commit after reopen.
#[test]
#[ignore]
fn commit_floor_profile_reopen() {
    let scratch = std::env::var("V3_SCRATCH")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let cache = format!("{scratch}/commit_floor_reopen_cache");
    let _ = std::fs::remove_dir_all(&cache);
    let store = Arc::new(MemBlobStore::new());
    let shards = 2u32;
    let t = Instant::now();
    {
        let storage = BlobShardStorage::new(store.clone(), "commit_floor_reopen", &cache);
        let h = ShardedHandle::create_with_storage(Box::new(storage), &schema(shards)).unwrap();
        add_docs(&h, 9, 0);
        h.commit().unwrap();
        h.close().unwrap();
    }
    println!("create+commit+close {:8.1}ms", t.elapsed().as_secs_f64() * 1e3);
    for cycle in 0..3u64 {
        let t = Instant::now();
        let storage = BlobShardStorage::new(store.clone(), "commit_floor_reopen", &cache);
        let h = ShardedHandle::open_with_storage(Box::new(storage)).unwrap();
        let opened = t.elapsed();
        add_docs(&h, 9, 100 * (cycle + 1));
        let t = Instant::now();
        h.commit().unwrap();
        let committed = t.elapsed();
        let t = Instant::now();
        h.close().unwrap();
        let closed = t.elapsed();
        println!(
            "reopen#{cycle} open {:8.1}ms  commit {:8.1}ms  close {:8.1}ms",
            opened.as_secs_f64() * 1e3,
            committed.as_secs_f64() * 1e3,
            closed.as_secs_f64() * 1e3
        );
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
