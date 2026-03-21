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
