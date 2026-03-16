//! Sharded index handle.
//!
//! `ShardedHandle` wraps N `LucivyHandle` instances, each in its own sub-directory.
//! Documents are routed to shards via `ShardRouter` (token-aware IDF-weighted).
//! Search dispatches to all shard actors in parallel via the global scheduler
//! and merges results via a binary heap.
//!
//! WASM compatible: uses the actor system (persistent threads or cooperative).

use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ld_lucivy::actor::mailbox::{mailbox, ActorRef};
use ld_lucivy::actor::reply::{reply, Reply};
use ld_lucivy::actor::scheduler::global_scheduler;
use ld_lucivy::actor::{Actor, ActorStatus, Priority};
use ld_lucivy::schema::{Field, Schema};
use ld_lucivy::{DocAddress, Index, LucivyDocument};

use crate::directory::StdFsDirectory;
use crate::handle::{LucivyHandle, NODE_ID_FIELD};
use crate::query::{QueryConfig, SchemaConfig};
use crate::shard_router::ShardRouter;

/// File storing shard router state (counters, thresholds).
const SHARD_STATS_FILE: &str = "_shard_stats.bin";

/// Config file for the sharded index (number of shards + schema).
const SHARD_CONFIG_FILE: &str = "_shard_config.json";

// ─── Actor ──────────────────────────────────────────────────────────────────

/// Message sent to a shard search actor.
pub enum ShardSearchMsg {
    /// Execute a search query and reply with results.
    Search {
        query_config: QueryConfig,
        top_k: usize,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
        reply: Reply<Result<Vec<(f32, DocAddress)>, String>>,
    },
}

/// Persistent actor for a single shard. Holds an Arc to the shard's handle.
/// Spawned once at create/open and lives until the ShardedHandle is dropped.
struct ShardSearchActor {
    shard_id: usize,
    handle: Arc<LucivyHandle>,
}

impl Actor for ShardSearchActor {
    type Msg = ShardSearchMsg;

    fn name(&self) -> &'static str {
        "shard-search"
    }

    fn handle(&mut self, msg: ShardSearchMsg) -> ActorStatus {
        match msg {
            ShardSearchMsg::Search {
                query_config,
                top_k,
                highlight_sink,
                reply,
            } => {
                let result =
                    search_shard(&self.handle, self.shard_id, &query_config, top_k, highlight_sink);
                reply.send(result);
                ActorStatus::Continue
            }
        }
    }

    fn priority(&self) -> Priority {
        // Critical: the caller blocks on the reply.
        Priority::Critical
    }
}

/// Execute a search on a single shard.
fn search_shard(
    handle: &LucivyHandle,
    shard_id: usize,
    query_config: &QueryConfig,
    top_k: usize,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let query = crate::query::build_query(query_config, &handle.schema, &handle.index, highlight_sink)?;
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(top_k).order_by_score();
    searcher
        .search(&*query, &collector)
        .map_err(|e| format!("search shard_{shard_id}: {e}"))
}

// ─── Search Result ──────────────────────────────────────────────────────────

/// A search result from a sharded search: score, shard index, document address.
#[derive(Debug, Clone)]
pub struct ShardedSearchResult {
    pub score: f32,
    pub shard_id: usize,
    pub doc_address: DocAddress,
}

/// Wrapper for BinaryHeap ordering (min-heap by score for top-K).
struct ScoredEntry {
    score: f32,
    shard_id: usize,
    doc_address: DocAddress,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse: min-heap so we evict the lowest score when over capacity.
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ─── ShardedHandle ──────────────────────────────────────────────────────────

/// Sharded index: N `LucivyHandle` instances with token-aware routing.
///
/// Search is dispatched to N persistent `ShardSearchActor` instances via the
/// global scheduler. In multi-threaded mode, all shards are searched in parallel.
/// In WASM single-threaded mode, the scheduler processes them cooperatively.
pub struct ShardedHandle {
    shards: Vec<Arc<LucivyHandle>>,
    /// One ActorRef per shard, for sending search messages.
    search_actors: Vec<ActorRef<ShardSearchMsg>>,
    router: Mutex<ShardRouter>,
    /// Base directory path (contains shard_0/, shard_1/, ...).
    base_path: String,
    /// Schema (same across all shards).
    pub schema: Schema,
    /// Field map (same across all shards).
    pub field_map: Vec<(String, Field)>,
    /// Original config.
    pub config: SchemaConfig,
}

/// Spawn N ShardSearchActors in the global scheduler.
fn spawn_search_actors(
    shards: &[Arc<LucivyHandle>],
) -> Vec<ActorRef<ShardSearchMsg>> {
    let scheduler = global_scheduler();
    let mut actors = Vec::with_capacity(shards.len());

    for (shard_id, handle) in shards.iter().enumerate() {
        let actor = ShardSearchActor {
            shard_id,
            handle: Arc::clone(handle),
        };
        let (mailbox, mut actor_ref) = mailbox::<ShardSearchMsg>(64);
        scheduler.spawn(actor, mailbox, &mut actor_ref, 64);
        actors.push(actor_ref);
    }

    actors
}

impl ShardedHandle {
    /// Create a new sharded index.
    ///
    /// Creates `config.shards` sub-directories, each with its own `LucivyHandle`.
    /// Spawns N `ShardSearchActor`s in the global scheduler.
    pub fn create(base_path: &str, config: &SchemaConfig) -> Result<Self, String> {
        let num_shards = config.shards.unwrap_or(1);
        if num_shards == 0 {
            return Err("shards must be >= 1".into());
        }

        std::fs::create_dir_all(base_path)
            .map_err(|e| format!("cannot create base dir: {e}"))?;

        // Save shard config at root level.
        let config_json = serde_json::to_string(config)
            .map_err(|e| format!("cannot serialize shard config: {e}"))?;
        std::fs::write(
            Path::new(base_path).join(SHARD_CONFIG_FILE),
            config_json.as_bytes(),
        )
        .map_err(|e| format!("cannot write shard config: {e}"))?;

        // Create each shard sub-directory + handle.
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            let shard_dir = Path::new(base_path).join(format!("shard_{i}"));
            std::fs::create_dir_all(&shard_dir)
                .map_err(|e| format!("cannot create shard_{i} dir: {e}"))?;
            let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())
                .map_err(|e| format!("cannot open shard_{i} dir: {e}"))?;
            let handle = LucivyHandle::create(dir, config)?;
            shards.push(Arc::new(handle));
        }

        let schema = shards[0].schema.clone();
        let field_map = shards[0].field_map.clone();
        let router = ShardRouter::new(num_shards);
        let search_actors = spawn_search_actors(&shards);

        Ok(Self {
            shards,
            search_actors,
            router: Mutex::new(router),
            base_path: base_path.to_string(),
            schema,
            field_map,
            config: config.clone(),
        })
    }

    /// Open an existing sharded index.
    pub fn open(base_path: &str) -> Result<Self, String> {
        // Read shard config.
        let config_path = Path::new(base_path).join(SHARD_CONFIG_FILE);
        let config_data = std::fs::read(&config_path)
            .map_err(|e| format!("cannot read shard config: {e}"))?;
        let config: SchemaConfig = serde_json::from_slice(&config_data)
            .map_err(|e| format!("cannot parse shard config: {e}"))?;

        let num_shards = config.shards.unwrap_or(1);

        // Read shard router state if available.
        let stats_path = Path::new(base_path).join(SHARD_STATS_FILE);
        let router = if stats_path.exists() {
            let stats_data = std::fs::read(&stats_path)
                .map_err(|e| format!("cannot read shard stats: {e}"))?;
            ShardRouter::from_bytes(&stats_data)?
        } else {
            ShardRouter::new(num_shards)
        };

        // Open each shard.
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            let shard_dir = Path::new(base_path).join(format!("shard_{i}"));
            let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())
                .map_err(|e| format!("cannot open shard_{i}: {e}"))?;
            let handle = LucivyHandle::open(dir)?;
            shards.push(Arc::new(handle));
        }

        let schema = shards[0].schema.clone();
        let field_map = shards[0].field_map.clone();
        let search_actors = spawn_search_actors(&shards);

        Ok(Self {
            shards,
            search_actors,
            router: Mutex::new(router),
            base_path: base_path.to_string(),
            schema,
            field_map,
            config,
        })
    }

    /// Route a document to a shard and add it.
    ///
    /// `token_hashes` are the pre-hashed tokens of the document (via `ShardRouter::hash_token`).
    /// The caller is responsible for tokenizing and hashing — this keeps the handle generic.
    pub fn add_document(
        &self,
        doc: LucivyDocument,
        token_hashes: &[u64],
    ) -> Result<usize, String> {
        let shard_id = {
            let mut router = self.router.lock().map_err(|_| "router lock poisoned")?;
            router.route(token_hashes)
        };

        let handle = &self.shards[shard_id];
        let mut guard = handle.writer.lock().map_err(|_| "writer lock poisoned")?;
        let writer = guard.as_mut().ok_or("shard is closed")?;
        writer
            .add_document(doc)
            .map_err(|e| format!("add_document to shard_{shard_id}: {e}"))?;
        handle.mark_uncommitted();

        Ok(shard_id)
    }

    /// Search all shards in parallel and merge top-K results.
    ///
    /// Dispatches search to all shard actors via the global scheduler.
    /// Returns results sorted by descending score.
    pub fn search(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    ) -> Result<Vec<ShardedSearchResult>, String> {
        // Flush uncommitted changes on all shards before searching.
        for (i, shard) in self.shards.iter().enumerate() {
            if shard.has_uncommitted() {
                let mut guard = shard.writer.lock().map_err(|_| "writer lock poisoned")?;
                if let Some(ref mut writer) = *guard {
                    writer
                        .commit()
                        .map_err(|e| format!("commit shard_{i}: {e}"))?;
                }
                shard.mark_committed();
                shard
                    .reader
                    .reload()
                    .map_err(|e| format!("reload shard_{i}: {e}"))?;
            }
        }

        // Send search to all shard actors in parallel.
        let mut receivers = Vec::with_capacity(self.search_actors.len());
        for actor_ref in &self.search_actors {
            let (tx, rx) = reply();
            actor_ref
                .send(ShardSearchMsg::Search {
                    query_config: query_config.clone(),
                    top_k,
                    highlight_sink: highlight_sink.clone(),
                    reply: tx,
                })
                .map_err(|_| "search actor channel closed")?;
            receivers.push(rx);
        }

        // Collect results and heap-merge top-K.
        let scheduler = global_scheduler();
        let mut heap = BinaryHeap::with_capacity(top_k + 1);

        for (shard_id, rx) in receivers.into_iter().enumerate() {
            // wait_cooperative pumps the scheduler when in single-thread mode (WASM).
            // In multi-thread mode, the actor threads process the work and the condvar
            // wakes us up — the run_one_step calls are no-ops (return false quickly).
            let shard_hits =
                rx.wait_cooperative(|| scheduler.run_one_step())?;

            for (score, doc_addr) in shard_hits {
                heap.push(ScoredEntry {
                    score,
                    shard_id,
                    doc_address: doc_addr,
                });
                if heap.len() > top_k {
                    heap.pop();
                }
            }
        }

        // Extract in descending score order.
        let mut results: Vec<ShardedSearchResult> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|e| ShardedSearchResult {
                score: e.score,
                shard_id: e.shard_id,
                doc_address: e.doc_address,
            })
            .collect();
        results.reverse();
        Ok(results)
    }

    /// Commit all shards and persist the shard router state.
    pub fn commit(&self) -> Result<(), String> {
        for (i, shard) in self.shards.iter().enumerate() {
            let mut guard = shard.writer.lock().map_err(|_| "writer lock poisoned")?;
            if let Some(ref mut writer) = *guard {
                writer
                    .commit()
                    .map_err(|e| format!("commit shard_{i}: {e}"))?;
            }
            shard.mark_committed();
            shard
                .reader
                .reload()
                .map_err(|e| format!("reload shard_{i}: {e}"))?;
        }

        // Persist router state.
        let router = self.router.lock().map_err(|_| "router lock poisoned")?;
        let stats_bytes = router.to_bytes();
        std::fs::write(
            Path::new(&self.base_path).join(SHARD_STATS_FILE),
            &stats_bytes,
        )
        .map_err(|e| format!("cannot write shard stats: {e}"))?;

        Ok(())
    }

    /// Close all shards (flush + release writer locks).
    pub fn close(&self) -> Result<(), String> {
        // Persist router state first.
        {
            let router = self.router.lock().map_err(|_| "router lock poisoned")?;
            let stats_bytes = router.to_bytes();
            std::fs::write(
                Path::new(&self.base_path).join(SHARD_STATS_FILE),
                &stats_bytes,
            )
            .map_err(|e| format!("cannot write shard stats on close: {e}"))?;
        }

        for (i, shard) in self.shards.iter().enumerate() {
            shard
                .close()
                .map_err(|e| format!("close shard_{i}: {e}"))?;
        }
        Ok(())
    }

    /// Delete a document by its _node_id from ALL shards.
    /// (We don't know which shard holds it.)
    pub fn delete_by_node_id(&self, node_id: u64) -> Result<(), String> {
        let nid_field = self
            .field(NODE_ID_FIELD)
            .ok_or("_node_id field not found")?;
        let term = ld_lucivy::schema::Term::from_field_u64(nid_field, node_id);

        for (i, shard) in self.shards.iter().enumerate() {
            let mut guard = shard.writer.lock().map_err(|_| "writer lock poisoned")?;
            if let Some(ref mut writer) = *guard {
                writer.delete_term(term.clone());
                shard.mark_uncommitted();
            } else {
                return Err(format!("shard_{i} is closed"));
            }
        }
        Ok(())
    }

    /// Get a field by name.
    pub fn field(&self, name: &str) -> Option<Field> {
        self.field_map
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, f)| *f)
    }

    /// Number of shards.
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Total documents across all shards.
    pub fn num_docs(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.reader.searcher().num_docs())
            .sum()
    }

    /// Access a shard's LucivyHandle (for advanced use, e.g. reading stored fields).
    pub fn shard(&self, shard_id: usize) -> Option<&LucivyHandle> {
        self.shards.get(shard_id).map(|arc| arc.as_ref())
    }

    /// Get a reference to the index of shard 0 (useful for tokenizer access).
    pub fn index(&self) -> &Index {
        &self.shards[0].index
    }

    /// Get router statistics (doc counts per shard, etc.).
    pub fn router_stats(&self) -> Result<(Vec<u64>, u64), String> {
        let router = self.router.lock().map_err(|_| "router lock poisoned")?;
        Ok((router.shard_doc_counts().to_vec(), router.total_docs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> String {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn make_config(shards: usize) -> SchemaConfig {
        serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "body", "type": "text", "stored": true}
            ],
            "shards": shards
        }))
        .unwrap()
    }

    #[test]
    fn test_create_and_search_single_shard() {
        let dir = tmp_dir("lucivy_sharded_single");
        let config = make_config(1);
        let handle = ShardedHandle::create(&dir, &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..10 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("document number {i} about rust"));
            let tokens = vec![
                ShardRouter::hash_token("document"),
                ShardRouter::hash_token("number"),
                ShardRouter::hash_token("rust"),
            ];
            handle.add_document(doc, &tokens).unwrap();
        }

        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 10);

        let query: QueryConfig =
            serde_json::from_str(r#"{"type": "contains", "field": "body", "value": "rust"}"#)
                .unwrap();
        let results = handle.search(&query, 5, None).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].shard_id, 0);
    }

    #[test]
    fn test_create_and_search_multi_shard() {
        let dir = tmp_dir("lucivy_sharded_multi");
        let config = make_config(3);
        let handle = ShardedHandle::create(&dir, &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..30 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            let text = if i % 3 == 0 {
                format!("rust programming language {i}")
            } else if i % 3 == 1 {
                format!("python scripting language {i}")
            } else {
                format!("java enterprise language {i}")
            };
            doc.add_text(body, &text);

            let words: Vec<u64> = text
                .split_whitespace()
                .map(|w| ShardRouter::hash_token(w))
                .collect();
            handle.add_document(doc, &words).unwrap();
        }

        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 30);

        // Verify docs are distributed across shards
        let (counts, total) = handle.router_stats().unwrap();
        assert_eq!(total, 30);
        assert_eq!(counts.len(), 3);
        for &c in &counts {
            assert!(
                c > 0,
                "each shard should have at least one doc, got {:?}",
                counts
            );
        }

        // Search for "rust"
        let query: QueryConfig =
            serde_json::from_str(r#"{"type": "contains", "field": "body", "value": "rust"}"#)
                .unwrap();
        let results = handle.search(&query, 20, None).unwrap();
        assert_eq!(results.len(), 10, "should find 10 rust docs");
    }

    #[test]
    fn test_close_and_reopen() {
        let dir = tmp_dir("lucivy_sharded_reopen");
        let config = make_config(2);

        // Create and insert
        {
            let handle = ShardedHandle::create(&dir, &config).unwrap();
            let body = handle.field("body").unwrap();
            let nid = handle.field(NODE_ID_FIELD).unwrap();

            for i in 0u64..20 {
                let mut doc = LucivyDocument::new();
                doc.add_u64(nid, i);
                doc.add_text(body, &format!("persistence test doc {i}"));
                let tokens = vec![ShardRouter::hash_token("persistence")];
                handle.add_document(doc, &tokens).unwrap();
            }
            handle.close().unwrap();
        }

        // Reopen
        let handle = ShardedHandle::open(&dir).unwrap();
        assert_eq!(handle.num_shards(), 2);

        // Reload readers to see committed data.
        for shard in &handle.shards {
            shard.reader.reload().unwrap();
        }
        assert_eq!(handle.num_docs(), 20);

        // Router state should be restored
        let (_, total) = handle.router_stats().unwrap();
        assert_eq!(total, 20);
    }

    #[test]
    fn test_delete_by_node_id() {
        let dir = tmp_dir("lucivy_sharded_delete");
        let config = make_config(2);
        let handle = ShardedHandle::create(&dir, &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..10 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("deletable doc {i}"));
            handle
                .add_document(doc, &[ShardRouter::hash_token("deletable")])
                .unwrap();
        }
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 10);

        // Delete node_id=5
        handle.delete_by_node_id(5).unwrap();
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 9);
    }
}
