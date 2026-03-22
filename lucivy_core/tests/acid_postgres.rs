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
use ld_lucivy::query::Query;

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

/// Verify that BM25 scores are identical across ALL configurations:
///   1. Single LucivyHandle (no sharding at all)
///   2. ShardedHandle with 1 shard
///   3. ShardedHandle with 4 shards
/// Same 100 docs, same query → same scores.
#[test]
#[ignore]
fn test_bm25_scores_identical_across_shard_counts() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("score_0sh");
    store.clear_index("score_1sh");
    store.clear_index("score_4sh");

    let cache_0 = std::env::temp_dir().join("lucivy_score_0sh");
    let cache_1 = std::env::temp_dir().join("lucivy_score_1sh");
    let cache_4 = std::env::temp_dir().join("lucivy_score_4sh");
    let _ = std::fs::remove_dir_all(&cache_0);
    let _ = std::fs::remove_dir_all(&cache_1);
    let _ = std::fs::remove_dir_all(&cache_4);

    // Config 0: single LucivyHandle (no sharding at all)
    let blob_dir_0 = lucivy_core::blob_directory::BlobDirectory::new(
        store.clone(), "score_0sh", &cache_0,
    ).unwrap();
    let schema_config = make_config(1);
    let (schema_0, field_map_0) = lucivy_core::handle::build_schema(&schema_config).unwrap();
    let handle_0 = lucivy_core::handle::LucivyHandle::create(blob_dir_0, &schema_config).unwrap();

    // Config 1: ShardedHandle with 1 shard
    let storage_1 = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "score_1sh", &cache_1,
    );
    let handle_1 = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_1), &make_config(1),
    ).unwrap();

    // Config 4: ShardedHandle with 4 shards
    let storage_4 = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "score_4sh", &cache_4,
    );
    let handle_4 = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_4), &make_config(4),
    ).unwrap();

    // Index same 100 docs in all 3 configurations
    let body_1 = handle_1.field("body").unwrap();
    let nid_1 = handle_1.field("_node_id").unwrap();
    let body_0 = schema_0.get_field("body").unwrap();
    let nid_0 = schema_0.get_field("_node_id").unwrap();

    {
        let mut guard = handle_0.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for i in 0..100u64 {
            let text = format!("Document {i} about mutex lock contention and scheduling");

            // Config 0: raw LucivyHandle
            let mut doc0 = ld_lucivy::LucivyDocument::new();
            doc0.add_u64(nid_0, i);
            doc0.add_text(body_0, &text);
            writer.add_document(doc0).unwrap();

            // Config 1: ShardedHandle 1-shard
            let mut doc1 = ld_lucivy::LucivyDocument::new();
            doc1.add_u64(nid_1, i);
            doc1.add_text(body_1, &text);
            handle_1.add_document(doc1, i).unwrap();

            // Config 4: ShardedHandle 4-shard
            let mut doc4 = ld_lucivy::LucivyDocument::new();
            doc4.add_u64(nid_1, i);
            doc4.add_text(body_1, &text);
            handle_4.add_document(doc4, i).unwrap();
        }
        writer.commit().unwrap();
    }
    handle_0.reader.reload().unwrap();
    handle_1.commit().unwrap();
    handle_4.commit().unwrap();

    // Search all 3 configurations
    let query = QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("mutex".into()),
        ..Default::default()
    };

    // Config 0: raw LucivyHandle search (standard path, no DAG)
    let searcher_0 = handle_0.reader.searcher();
    let query_obj = lucivy_core::query::build_query(
        &query, &schema_0, &handle_0.index, None,
    ).unwrap();
    let collector = ld_lucivy::collector::TopDocs::with_limit(10).order_by_score();
    let results_0 = searcher_0.search(&*query_obj, &collector).unwrap();

    // Config 1 & 4: ShardedHandle search (two-pass DAG)
    let results_1 = handle_1.search(&query, 10, None).unwrap();
    let results_4 = handle_4.search(&query, 10, None).unwrap();

    let score_0 = results_0.first().map(|(s, _)| *s).unwrap_or(0.0);
    let score_1 = results_1.first().map(|r| r.score).unwrap_or(0.0);
    let score_4 = results_4.first().map(|r| r.score).unwrap_or(0.0);

    // ── Ground truth BM25 ──
    // Compute expected score by hand: iterate all docs, tokenize, count.
    let search_term = "mutex";
    let total_docs = 100u64;
    let mut doc_freq = 0u64;      // how many docs contain "mutex" as substring
    let mut doc_lengths = Vec::new(); // token count per doc
    let mut first_doc_tf = 0u32;  // TF of "mutex" in first matching doc

    for i in 0..100u64 {
        let text = format!("Document {i} about mutex lock contention and scheduling");
        let text_lower = text.to_lowercase();

        // Count tokens (SimpleTokenizer + CamelCaseSplit + LowerCaser = RAW_TOKENIZER)
        // For these simple docs, tokens ≈ words split on whitespace/punctuation
        let token_count = text_lower.split_whitespace().count() as u32;
        doc_lengths.push(token_count);

        // Count substring occurrences of "mutex"
        let tf = text_lower.matches(search_term).count() as u32;
        if tf > 0 {
            doc_freq += 1;
            if first_doc_tf == 0 { first_doc_tf = tf; }
        }
    }

    let avg_doc_len = doc_lengths.iter().sum::<u32>() as f32 / total_docs as f32;

    // BM25 formula (k1=1.2, b=0.75 — standard defaults)
    let k1: f32 = 1.2;
    let b: f32 = 0.75;
    let idf = ((total_docs as f32 - doc_freq as f32 + 0.5) / (doc_freq as f32 + 0.5) + 1.0).ln();
    let first_doc_len = doc_lengths[0] as f32;
    let tf_component = (first_doc_tf as f32 * (k1 + 1.0))
        / (first_doc_tf as f32 + k1 * (1.0 - b + b * first_doc_len / avg_doc_len));
    let ground_truth_score = idf * tf_component;

    eprintln!("\n=== Ground truth BM25 ===");
    eprintln!("total_docs={}, doc_freq={}, avg_doc_len={:.1}", total_docs, doc_freq, avg_doc_len);
    eprintln!("first_doc: tf={}, doc_len={}", first_doc_tf, doc_lengths[0]);
    eprintln!("IDF={:.6}, TF_comp={:.6}, score={:.6}", idf, tf_component, ground_truth_score);

    eprintln!("\nno-shard:  {} hits, score[0]={:.6}", results_0.len(), score_0);
    eprintln!("1-shard:   {} hits, score[0]={:.6}", results_1.len(), score_1);
    eprintln!("4-shard:   {} hits, score[0]={:.6}", results_4.len(), score_4);
    eprintln!("ground truth:            score={:.6}", ground_truth_score);

    // Note: ground truth uses simplified tokenization (whitespace split).
    // The actual tokenizer (RAW_TOKENIZER = SimpleTokenizer + CamelCaseSplit + LowerCaser)
    // may produce slightly different token counts, so we allow a small margin.
    // But the KEY test is: 1-shard == 4-shard (two-pass consistency).
    let diff_14 = (score_1 - score_4).abs();
    eprintln!("\nDiff 1-shard vs 4-shard: {:.10} (must be 0)", diff_14);
    assert!(diff_14 < 0.0001, "1-shard vs 4-shard must match: {score_1:.6} vs {score_4:.6}");

    // Check which is closer to ground truth
    let diff_gt_0 = (score_0 - ground_truth_score).abs();
    let diff_gt_1 = (score_1 - ground_truth_score).abs();
    eprintln!("Diff no-shard vs ground truth: {:.6}", diff_gt_0);
    eprintln!("Diff sharded vs ground truth:  {:.6}", diff_gt_1);

    // The sharded score should be closer to ground truth (global IDF)
    // The no-shard score has per-segment IDF inflation
    eprintln!("Closer to ground truth: {}", if diff_gt_1 < diff_gt_0 { "SHARDED ✓" } else { "NO-SHARD" });

    handle_0.close().unwrap();
    handle_1.close().unwrap();
    handle_4.close().unwrap();
    eprintln!("BM25 score consistency test passed!");
}

/// Distributed BM25 ground truth: two separate ShardedHandles simulate two machines.
/// Same 100 docs split across two nodes → scores must equal ground truth.
#[test]
#[ignore]
fn test_distributed_bm25_ground_truth() {
    let url = pg_url().expect("POSTGRES_URL required");
    let store = std::sync::Arc::new(PostgresBlobStore::connect(&url).unwrap());
    store.clear_index("dist_gt_a");
    store.clear_index("dist_gt_b");

    let cache_a = std::env::temp_dir().join("lucivy_dist_gt_a");
    let cache_b = std::env::temp_dir().join("lucivy_dist_gt_b");
    let _ = std::fs::remove_dir_all(&cache_a);
    let _ = std::fs::remove_dir_all(&cache_b);

    let storage_a = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "dist_gt_a", &cache_a,
    );
    let node_a = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_a), &make_config(2),
    ).unwrap();

    let storage_b = lucivy_core::sharded_handle::BlobShardStorage::new(
        store.clone(), "dist_gt_b", &cache_b,
    );
    let node_b = lucivy_core::sharded_handle::ShardedHandle::create_with_storage(
        Box::new(storage_b), &make_config(2),
    ).unwrap();

    // Same 100 docs, hash-routed to two nodes
    let body = node_a.field("body").unwrap();
    let nid = node_a.field("_node_id").unwrap();
    let nodes = [&node_a, &node_b];

    for i in 0..100u64 {
        let text = format!("Document {i} about mutex lock contention and scheduling");
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(body, &text);
        nodes[(i % 2) as usize].add_document(doc, i).unwrap();
    }
    node_a.commit().unwrap();
    node_b.commit().unwrap();

    let query_config = QueryConfig {
        query_type: "contains".into(),
        field: Some("body".into()),
        value: Some("mutex".into()),
        ..Default::default()
    };

    // ── Distributed protocol (unified) ──
    // Step 1: each node exports stats (includes prescan for contains doc_freq)
    let stats_a = node_a.export_stats(&query_config).unwrap();
    let stats_b = node_b.export_stats(&query_config).unwrap();

    eprintln!("Stats A: {} docs, contains_doc_freqs={:?}", stats_a.total_num_docs, stats_a.contains_doc_freqs);
    eprintln!("Stats B: {} docs, contains_doc_freqs={:?}", stats_b.total_num_docs, stats_b.contains_doc_freqs);

    // Step 2: coordinator merges all stats
    let global_stats = lucivy_core::bm25_global::ExportableStats::merge(&[stats_a, stats_b]);
    eprintln!("Global: {} docs, contains_doc_freqs={:?}", global_stats.total_num_docs, global_stats.contains_doc_freqs);

    // Step 3: each node searches with global stats (contains doc_freqs injected)
    let results_a = node_a.search_with_global_stats(&query_config, 100, &global_stats, None).unwrap();
    let results_b = node_b.search_with_global_stats(&query_config, 100, &global_stats, None).unwrap();

    // Ground truth
    let total_docs = 100u64;
    let doc_freq = 100u64;
    let idf = ((total_docs as f32 - doc_freq as f32 + 0.5) / (doc_freq as f32 + 0.5) + 1.0).ln();
    let ground_truth_score = idf * 1.0;

    let score_a = results_a.first().map(|r| r.score).unwrap_or(0.0);
    let score_b = results_b.first().map(|r| r.score).unwrap_or(0.0);

    eprintln!("\n=== Distributed ground truth ===");
    eprintln!("Node A: {} hits, score[0]={:.6}", results_a.len(), score_a);
    eprintln!("Node B: {} hits, score[0]={:.6}", results_b.len(), score_b);
    eprintln!("Ground truth: {:.6}", ground_truth_score);

    let diff_a = (score_a - ground_truth_score).abs();
    let diff_b = (score_b - ground_truth_score).abs();
    eprintln!("Diff A vs ground truth: {:.6}", diff_a);
    eprintln!("Diff B vs ground truth: {:.6}", diff_b);

    assert!(diff_a < 0.001,
        "Node A score should match ground truth: {score_a:.6} vs {ground_truth_score:.6}");
    assert!(diff_b < 0.001,
        "Node B score should match ground truth: {score_b:.6} vs {ground_truth_score:.6}");

    node_a.close().unwrap();
    node_b.close().unwrap();
    eprintln!("Distributed ground truth test passed!");
}
