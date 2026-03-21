//! ACID tests with a real Postgres database.
//!
//! These tests are #[ignore] by default. To run them:
//!
//! 1. Start Postgres:
//!    docker compose -f docker-compose.test.yml up -d
//!
//! 2. Run tests:
//!    POSTGRES_URL="host=localhost port=5433 user=test password=test dbname=lucivy_test" \
//!      cargo test --package lucivy-core --test acid_postgres -- --ignored --nocapture
//!
//! 3. Cleanup:
//!    docker compose -f docker-compose.test.yml down -v

use std::io;
use std::sync::Mutex;

use lucivy_core::blob_store::BlobStore;
use lucivy_core::query::QueryConfig;

// ─── PostgresBlobStore ──────────────────────────────────────────────────────

struct PostgresBlobStore {
    client: Mutex<postgres::Client>,
}

impl PostgresBlobStore {
    fn connect(params: &str) -> Result<Self, postgres::Error> {
        let mut client = postgres::Client::connect(params, postgres::NoTls)?;

        client.batch_execute(
            "DO $$ BEGIN
                CREATE TABLE _index_blobs (
                    key TEXT PRIMARY KEY,
                    data BYTEA NOT NULL
                );
            EXCEPTION WHEN duplicate_table THEN NULL;
            END $$"
        )?;

        Ok(Self { client: Mutex::new(client) })
    }

    fn clear_index(&self, index_name: &str) {
        let mut c = self.client.lock().unwrap();
        let pattern1 = format!("{index_name}/%");
        let pattern2 = format!("Lucivy_{index_name}/%");
        let _ = c.execute("DELETE FROM _index_blobs WHERE key LIKE $1 OR key LIKE $2",
            &[&pattern1, &pattern2]);
    }
}

impl BlobStore for PostgresBlobStore {
    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> {
        let key = format!("{index_name}/{file_name}");
        let mut c = self.client.lock().unwrap();
        c.execute(
            "INSERT INTO _index_blobs (key, data) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET data = $2",
            &[&key, &data],
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(())
    }

    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>> {
        let key = format!("{index_name}/{file_name}");
        let mut c = self.client.lock().unwrap();
        let row = c.query_opt("SELECT data FROM _index_blobs WHERE key = $1", &[&key])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        match row {
            Some(r) => Ok(r.get::<_, Vec<u8>>(0)),
            None => Err(io::Error::new(io::ErrorKind::NotFound,
                format!("blob not found: {key}"))),
        }
    }

    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()> {
        let key = format!("{index_name}/{file_name}");
        let mut c = self.client.lock().unwrap();
        c.execute("DELETE FROM _index_blobs WHERE key = $1", &[&key])
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(())
    }

    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool> {
        let key = format!("{index_name}/{file_name}");
        let mut c = self.client.lock().unwrap();
        let row = c.query_one(
            "SELECT EXISTS(SELECT 1 FROM _index_blobs WHERE key = $1)",
            &[&key],
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(row.get::<_, bool>(0))
    }

    fn list(&self, index_name: &str) -> io::Result<Vec<String>> {
        let prefix = format!("{index_name}/");
        let mut c = self.client.lock().unwrap();
        let rows = c.query(
            "SELECT key FROM _index_blobs WHERE key LIKE $1",
            &[&format!("{prefix}%")],
        ).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(rows.iter().map(|r| {
            let key: String = r.get(0);
            key.strip_prefix(&prefix).unwrap_or(&key).to_string()
        }).collect())
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn pg_url() -> Option<String> {
    std::env::var("POSTGRES_URL").ok()
}

fn make_config(shards: usize) -> lucivy_core::query::SchemaConfig {
    lucivy_core::query::SchemaConfig {
        fields: vec![
            lucivy_core::query::FieldDef {
                name: "title".into(),
                field_type: "text".into(),
                stored: Some(true),
                indexed: Some(true),
                fast: None,
            },
            lucivy_core::query::FieldDef {
                name: "body".into(),
                field_type: "text".into(),
                stored: Some(true),
                indexed: Some(true),
                fast: None,
            },
        ],
        tokenizer: None,
        shards: Some(shards),
        ..Default::default()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Basic ACID: create index in Postgres, insert docs, commit, drop, reopen, search.
#[test]
#[ignore]
fn test_acid_sharded_postgres_create_reopen_search() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("acid_test_1");

    let config = make_config(2);
    let cache = std::env::temp_dir().join("lucivy_acid_test_1");
    let _ = std::fs::remove_dir_all(&cache);

    // Create sharded index backed by Postgres
    let storage = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "acid_test_1", &cache,
    );
    let handle = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage), &config,
    ).expect("create sharded handle");

    // Index documents
    let title = handle.field("title").unwrap();
    let body = handle.field("body").unwrap();
    let nid = handle.field("_node_id").unwrap();

    for i in 0..100u64 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(title, &format!("Document {i}"));
        doc.add_text(body, &format!("This is the body of document number {i} about mutex and scheduling"));
        handle.add_document(doc, i).unwrap();
    }
    handle.commit().unwrap();

    // Verify search works
    let results = handle.search(&QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("mutex".into()),
        ..Default::default()
    }, 10, None).unwrap();
    let count_before = results.len();
    eprintln!("Before reopen: {} results for 'mutex'", count_before);
    assert!(count_before > 0, "should find docs with 'mutex'");

    // Close and drop — simulates process exit
    handle.close().unwrap();
    drop(handle);

    // Clean local cache — force reload from Postgres
    let _ = std::fs::remove_dir_all(&cache);

    // Reopen from Postgres
    let storage2 = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "acid_test_1", &cache,
    );
    let handle2 = lucivy_core::sharded_handle::ShardedHandle::open_with_storage(
        Box::new(storage2),
    ).expect("reopen from postgres");

    // Search again — must find same results
    let results2 = handle2.search(&QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("mutex".into()),
        ..Default::default()
    }, 10, None).unwrap();
    eprintln!("After reopen: {} results for 'mutex'", results2.len());
    assert_eq!(results2.len(), count_before, "same results after reopen from Postgres");

    // Contains search with highlights
    let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
    let results3 = handle2.search(&QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("scheduling".into()),
        ..Default::default()
    }, 5, Some(sink.clone())).unwrap();
    eprintln!("Highlights test: {} results for 'scheduling'", results3.len());
    assert!(results3.len() > 0);

    // Verify highlights are present
    for r in &results3 {
        let shard = handle2.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg = searcher.segment_reader(r.doc_address.segment_ord);
        let hl = sink.get(seg.segment_id(), r.doc_address.doc_id);
        eprintln!("  doc {} shard {} → highlights: {:?}", r.doc_address.doc_id, r.shard_id, hl);
    }

    handle2.close().unwrap();
    eprintln!("ACID test passed!");
}

/// Verify blobs are actually in Postgres (not just local cache).
#[test]
#[ignore]
fn test_acid_blobs_in_postgres() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("acid_test_2");

    let config = make_config(1);
    let cache = std::env::temp_dir().join("lucivy_acid_test_2");
    let _ = std::fs::remove_dir_all(&cache);

    let storage = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "acid_test_2", &cache,
    );
    let handle = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage), &config,
    ).unwrap();

    let body = handle.field("body").unwrap();
    let nid = handle.field("_node_id").unwrap();
    for i in 0..10u64 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(body, &format!("hello world document {i}"));
        handle.add_document(doc, i).unwrap();
    }
    handle.commit().unwrap();
    handle.close().unwrap();

    // Check that blobs exist in Postgres (BlobDirectory uses "Lucivy_" prefix)
    let blobs = store.list("Lucivy_acid_test_2").unwrap();
    eprintln!("Blobs in Postgres: {} files", blobs.len());
    for b in &blobs {
        eprintln!("  {}", b);
    }
    assert!(blobs.len() > 0, "should have blobs in Postgres after commit");

    // Should have segment files (.store, .pos, .idx, etc.) + meta.json + config
    let has_meta = blobs.iter().any(|b| b.contains("meta.json"));
    eprintln!("Has meta.json: {}", has_meta);
    assert!(has_meta, "meta.json should be in Postgres");
}

/// Crash simulation: index docs, commit, kill without close, reopen.
#[test]
#[ignore]
fn test_acid_crash_recovery() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("acid_test_3");

    let config = make_config(2);
    let cache = std::env::temp_dir().join("lucivy_acid_test_3");
    let _ = std::fs::remove_dir_all(&cache);

    let storage = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "acid_test_3", &cache,
    );
    let handle = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage), &config,
    ).unwrap();

    let body = handle.field("body").unwrap();
    let nid = handle.field("_node_id").unwrap();
    for i in 0..50u64 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(body, &format!("crash test document {i} with function calls"));
        handle.add_document(doc, i).unwrap();
    }
    handle.commit().unwrap();

    // DON'T call close() — simulate crash
    drop(handle);

    // Nuke local cache — only Postgres survives
    let _ = std::fs::remove_dir_all(&cache);

    // Reopen — should recover from Postgres
    let storage2 = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "acid_test_3", &cache,
    );
    let handle2 = lucivy_core::sharded_handle::ShardedHandle::open_with_storage(
        Box::new(storage2),
    ).expect("recover from postgres after crash");

    let results = handle2.search(&QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("function".into()),
        ..Default::default()
    }, 100, None).unwrap();
    eprintln!("After crash recovery: {} results for 'function'", results.len());
    assert_eq!(results.len(), 50, "all 50 docs should survive crash");

    handle2.close().unwrap();
    eprintln!("Crash recovery test passed!");
}

/// Distributed search: two ShardedHandles in Postgres, unified BM25, merged results.
#[test]
#[ignore]
fn test_distributed_search_two_nodes_postgres() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("dist_node_a");
    store.clear_index("dist_node_b");

    let config = make_config(2);
    let cache_a = std::env::temp_dir().join("lucivy_dist_a");
    let cache_b = std::env::temp_dir().join("lucivy_dist_b");
    let _ = std::fs::remove_dir_all(&cache_a);
    let _ = std::fs::remove_dir_all(&cache_b);

    // ── Create two "machines" backed by same Postgres, different namespaces ──

    let storage_a = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "dist_node_a", &cache_a,
    );
    let node_a = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_a), &config,
    ).expect("create node A");

    let storage_b = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "dist_node_b", &cache_b,
    );
    let node_b = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_b), &config,
    ).expect("create node B");

    // ── Distributed indexation with client-side routing ──
    //
    // In a real deployment, the client (or a load balancer) decides which
    // node receives each document. The nodes don't coordinate during indexation.
    //
    // Common routing strategies:
    //   - hash(doc_id) % num_nodes  → deterministic, good for deletes
    //   - round-robin               → balanced, simple
    //   - by category/tenant        → data locality
    //
    // Here we use hash routing: doc_id % 2 → node A or B.

    let nodes = [&node_a, &node_b];
    let body = node_a.field("body").unwrap();
    let nid = node_a.field("_node_id").unwrap();

    let topics = [
        "mutex synchronization and lock contention in concurrent systems",
        "scheduler performance and lock free data structures",
        "memory allocation with malloc and free in kernel modules",
        "spinlock implementation and atomic compare exchange operations",
        "process scheduling with priority queues and lock ordering",
    ];

    for i in 0..100u64 {
        let target_node = (i % 2) as usize;  // hash routing
        let topic = &topics[(i % topics.len() as u64) as usize];

        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(body, &format!("Document {i} covers {topic}"));

        nodes[target_node].add_document(doc, i).unwrap();
    }

    // Each node commits independently (could be on different machines)
    node_a.commit().unwrap();
    node_b.commit().unwrap();

    eprintln!("Node A: {} docs, Node B: {} docs", node_a.num_docs(), node_b.num_docs());
    assert_eq!(node_a.num_docs() + node_b.num_docs(), 100);

    // ── Distributed search protocol ──

    let query = QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("lock".into()),
        ..Default::default()
    };

    // Phase 1: collect stats from both nodes
    let stats_a = node_a.export_stats(&query).unwrap();
    let stats_b = node_b.export_stats(&query).unwrap();
    eprintln!("Stats A: {} docs, Stats B: {} docs", stats_a.total_num_docs, stats_b.total_num_docs);

    // Simulate serialization over network
    let json_a = serde_json::to_string(&stats_a).unwrap();
    let json_b = serde_json::to_string(&stats_b).unwrap();
    eprintln!("Stats A JSON: {} bytes, Stats B JSON: {} bytes", json_a.len(), json_b.len());

    let stats_a: lucivy_core::bm25_global::ExportableStats = serde_json::from_str(&json_a).unwrap();
    let stats_b: lucivy_core::bm25_global::ExportableStats = serde_json::from_str(&json_b).unwrap();

    // Phase 2: coordinator merges stats
    let global_stats = lucivy_core::bm25_global::ExportableStats::merge(&[stats_a, stats_b]);
    eprintln!("Global stats: {} total docs", global_stats.total_num_docs);

    // Phase 3: search each node with global stats + highlights
    let sink_a = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
    let sink_b = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());

    let results_a = node_a.search_with_global_stats(&query, 10, &global_stats, Some(sink_a.clone())).unwrap();
    let results_b = node_b.search_with_global_stats(&query, 10, &global_stats, Some(sink_b.clone())).unwrap();

    eprintln!("Results A: {} hits, Results B: {} hits", results_a.len(), results_b.len());

    // Phase 4: merge results (coordinator side)
    let mut merged: Vec<_> = results_a.iter()
        .map(|r| (r.score, "A", r.shard_id, r.doc_address))
        .chain(results_b.iter().map(|r| (r.score, "B", r.shard_id, r.doc_address)))
        .collect();
    merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    merged.truncate(10);

    eprintln!("\n=== Top 10 merged results (global BM25) ===");
    for (score, node, shard, addr) in &merged {
        eprintln!("  score={:.4} node={} shard={} doc={}", score, node, shard, addr.doc_id);
    }

    // ── Verify ──

    // "lock" appears in both nodes (A: "lock contention", B: "lock free")
    assert!(!results_a.is_empty(), "node A should have results for 'lock'");
    assert!(!results_b.is_empty(), "node B should have results for 'lock'");
    assert!(merged.len() <= 10, "top-10 merge");

    // Verify highlights work on both nodes
    for r in &results_a {
        let shard = node_a.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg = searcher.segment_reader(r.doc_address.segment_ord);
        let hl = sink_a.get(seg.segment_id(), r.doc_address.doc_id);
        assert!(hl.is_some(), "highlights should exist for node A results");
    }
    for r in &results_b {
        let shard = node_b.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg = searcher.segment_reader(r.doc_address.segment_ord);
        let hl = sink_b.get(seg.segment_id(), r.doc_address.doc_id);
        assert!(hl.is_some(), "highlights should exist for node B results");
    }

    // Also test search_with_docs (convenience method)
    let hits_a = node_a.search_with_docs(&query, 3).unwrap();
    eprintln!("\n=== search_with_docs (node A, top 3) ===");
    for hit in &hits_a {
        eprintln!("  score={:.4} shard={} highlights={:?}",
            hit.score, hit.shard_id, hit.highlights);
    }
    assert!(!hits_a.is_empty());
    assert!(!hits_a[0].highlights.is_empty(), "search_with_docs should include highlights");

    node_a.close().unwrap();
    node_b.close().unwrap();
    eprintln!("\nDistributed search test passed!");
}
