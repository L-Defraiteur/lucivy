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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::any::Any;

use ld_lucivy::collector::Collector;
use ld_lucivy::actor::envelope::{type_tag_hash, Envelope, Message, ReplyPort};
use ld_lucivy::actor::handler::TypedHandler;
use ld_lucivy::actor::generic_actor::GenericActor;
use ld_lucivy::actor::mailbox::{mailbox, ActorRef};
use ld_lucivy::actor::scheduler::global_scheduler;
use ld_lucivy::actor::{ActorStatus, Priority};
use ld_lucivy::query::Weight;
use ld_lucivy::schema::{Field, FieldType, Schema, Term, Value};
use ld_lucivy::{DocAddress, Index, LucivyDocument};

use crate::bm25_global::AggregatedBm25StatsOwned;
use crate::directory::StdFsDirectory;
use crate::handle::{LucivyHandle, NODE_ID_FIELD};
use crate::query::{QueryConfig, SchemaConfig};
use crate::shard_router::ShardRouter;

/// File storing shard router state (counters, thresholds).
const SHARD_STATS_FILE: &str = "_shard_stats.bin";

/// Config file for the sharded index (number of shards + schema).
const SHARD_CONFIG_FILE: &str = "_shard_config.json";

// ─── Shard Actor Messages ───────────────────────────────────────────────────
//
// Each message type implements the Message trait (type_tag + encode/decode).
// The Arc<dyn Weight> for search is passed via Envelope.local (not serialized).

/// Search: execute a pre-compiled Weight on this shard's segments.
/// The Weight is in Envelope.local as Arc<dyn Weight>.
struct ShardSearchMsg {
    top_k: usize,
}

impl Message for ShardSearchMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ShardSearchMsg") }
    fn encode(&self) -> Vec<u8> { (self.top_k as u32).to_le_bytes().to_vec() }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 4 { return Err("too short".into()); }
        Ok(Self { top_k: u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize })
    }
}

/// Insert a document into this shard.
/// The LucivyDocument is in Envelope.local (not serializable yet).
struct ShardInsertMsg;

impl Message for ShardInsertMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ShardInsertMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Commit pending writes on this shard.
struct ShardCommitMsg;

impl Message for ShardCommitMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ShardCommitMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Delete a document by term on this shard.
/// The Term is in Envelope.local.
struct ShardDeleteMsg;

impl Message for ShardDeleteMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ShardDeleteMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Reply with search results.
struct ShardSearchReply {
    results: Vec<(f32, DocAddress)>,
}

impl Message for ShardSearchReply {
    fn type_tag() -> u64 { type_tag_hash(b"ShardSearchReply") }
    fn encode(&self) -> Vec<u8> {
        // Encode as: [num_results: u32] [score: f32, seg_ord: u32, doc_id: u32] ...
        let mut buf = Vec::with_capacity(4 + self.results.len() * 12);
        buf.extend_from_slice(&(self.results.len() as u32).to_le_bytes());
        for (score, addr) in &self.results {
            buf.extend_from_slice(&score.to_le_bytes());
            buf.extend_from_slice(&addr.segment_ord.to_le_bytes());
            buf.extend_from_slice(&addr.doc_id.to_le_bytes());
        }
        buf
    }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 4 { return Err("too short".into()); }
        let n = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let mut results = Vec::with_capacity(n);
        let mut pos = 4;
        for _ in 0..n {
            if pos + 12 > bytes.len() { return Err("truncated".into()); }
            let score = f32::from_le_bytes(bytes[pos..pos+4].try_into().unwrap());
            let seg_ord = u32::from_le_bytes(bytes[pos+4..pos+8].try_into().unwrap());
            let doc_id = u32::from_le_bytes(bytes[pos+8..pos+12].try_into().unwrap());
            results.push((score, DocAddress { segment_ord: seg_ord, doc_id }));
            pos += 12;
        }
        Ok(Self { results })
    }
}

/// Simple OK/Error reply.
struct ShardOkReply;

impl Message for ShardOkReply {
    fn type_tag() -> u64 { type_tag_hash(b"ShardOkReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

// ─── Shard Actor Creation ───────────────────────────────────────────────────

/// Execute a pre-compiled Weight on a single shard's segments.
fn execute_weight_on_shard(
    handle: &LucivyHandle,
    shard_id: usize,
    weight: &dyn Weight,
    top_k: usize,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(top_k).order_by_score();
    collector.check_schema(searcher.schema())
        .map_err(|e| format!("schema check shard_{shard_id}: {e}"))?;
    let segment_readers = searcher.segment_readers();
    let mut fruits = Vec::with_capacity(segment_readers.len());
    for (seg_ord, seg_reader) in segment_readers.iter().enumerate() {
        let fruit = collector
            .collect_segment(weight, seg_ord as u32, seg_reader)
            .map_err(|e| format!("collect shard_{shard_id} seg_{seg_ord}: {e}"))?;
        fruits.push(fruit);
    }
    collector
        .merge_fruits(fruits)
        .map_err(|e| format!("merge shard_{shard_id}: {e}"))
}

/// Create a GenericActor for a shard with all roles: search, insert, commit, delete.
fn create_shard_actor(shard_id: usize, handle: Arc<LucivyHandle>) -> GenericActor {
    // Leak the name — one per shard, lives forever.
    let name: &'static str = Box::leak(format!("shard-{shard_id}").into_boxed_str());
    let mut actor = GenericActor::new(name);

    // State: the shard's handle + insert buffer
    actor.state_mut().insert::<Arc<LucivyHandle>>(handle);
    actor.state_mut().insert::<usize>(shard_id);
    actor.state_mut().insert::<Vec<LucivyDocument>>(Vec::new());

    // Search handler: Weight comes via envelope.local
    actor.register(TypedHandler::<ShardSearchMsg, _>::with_priority(
        |state, msg, reply, local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            let weight = local
                .and_then(|l| l.downcast::<Arc<dyn Weight>>().ok())
                .map(|w| *w);

            let result = match weight {
                Some(w) => execute_weight_on_shard(handle, shard_id, w.as_ref(), msg.top_k),
                None => Err("search: no Weight in envelope.local".into()),
            };

            if let Some(reply) = reply {
                match result {
                    Ok(hits) => reply.send(ShardSearchReply { results: hits }),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
        Priority::Critical,
    ));

    // Insert handler: LucivyDocument comes via envelope.local
    actor.register(TypedHandler::<ShardInsertMsg, _>::new(
        |state, _msg, reply, local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            let doc = local.and_then(|l| l.downcast::<LucivyDocument>().ok()).map(|d| *d);

            let result = match doc {
                Some(doc) => {
                    let mut guard = handle.writer.lock().unwrap();
                    match guard.as_mut() {
                        Some(writer) => {
                            writer.add_document(doc)
                                .map(|_| { handle.mark_uncommitted(); })
                                .map_err(|e| format!("insert shard_{shard_id}: {e}"))
                        }
                        None => Err(format!("shard_{shard_id} is closed")),
                    }
                }
                None => Err("insert: no LucivyDocument in envelope.local".into()),
            };

            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(ShardOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    // Commit handler
    actor.register(TypedHandler::<ShardCommitMsg, _>::new(
        |state, _msg, reply, _local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            let result = (|| -> Result<(), String> {
                let mut guard = handle.writer.lock().map_err(|_| "lock poisoned")?;
                if let Some(ref mut writer) = *guard {
                    writer.commit().map_err(|e| format!("commit shard_{shard_id}: {e}"))?;
                }
                handle.mark_committed();
                handle.reader.reload().map_err(|e| format!("reload shard_{shard_id}: {e}"))?;
                Ok(())
            })();

            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(ShardOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    // Delete handler: Term comes via envelope.local
    actor.register(TypedHandler::<ShardDeleteMsg, _>::new(
        |state, _msg, reply, local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            let term = local.and_then(|l| l.downcast::<Term>().ok()).map(|t| *t);

            let result = match term {
                Some(term) => {
                    let mut guard = handle.writer.lock().unwrap();
                    match guard.as_mut() {
                        Some(writer) => {
                            writer.delete_term(term);
                            handle.mark_uncommitted();
                            Ok(())
                        }
                        None => Err(format!("shard_{shard_id} is closed")),
                    }
                }
                None => Err("delete: no Term in envelope.local".into()),
            };

            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(ShardOkReply),
                    Err(e) => reply.send_err(e),
                }
            }
            ActorStatus::Continue
        },
    ));

    actor
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
    /// One ActorRef per shard (GenericActor, receives Envelopes).
    shard_actors: Vec<ActorRef<Envelope>>,
    router: Mutex<ShardRouter>,
    /// Base directory path (contains shard_0/, shard_1/, ...).
    base_path: String,
    /// Schema (same across all shards).
    pub schema: Schema,
    /// Field map (same across all shards).
    pub field_map: Vec<(String, Field)>,
    /// Original config.
    pub config: SchemaConfig,
    /// True if deletes happened since last resync.
    has_deletes: AtomicBool,
    /// Text field IDs (for tokenization at add_document).
    text_fields: Vec<Field>,
}

/// Spawn N GenericActors (one per shard) in the global scheduler.
fn spawn_shard_actors(
    shards: &[Arc<LucivyHandle>],
) -> Vec<ActorRef<Envelope>> {
    let scheduler = global_scheduler();
    let mut actors = Vec::with_capacity(shards.len());

    for (shard_id, handle) in shards.iter().enumerate() {
        let actor = create_shard_actor(shard_id, Arc::clone(handle));
        let (mb, mut actor_ref) = mailbox::<Envelope>(64);
        scheduler.spawn(actor, mb, &mut actor_ref, 64);
        actors.push(actor_ref);
    }

    actors
}

/// Extract text field IDs from the schema (for automatic tokenization at add_document).
fn find_text_fields(schema: &Schema) -> Vec<Field> {
    schema
        .fields()
        .filter_map(|(field, entry)| {
            if entry.name() == NODE_ID_FIELD {
                return None;
            }
            match entry.field_type() {
                FieldType::Str(opts) if opts.get_indexing_options().is_some() => Some(field),
                _ => None,
            }
        })
        .collect()
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
        let text_fields = find_text_fields(&schema);
        let df_threshold = config.df_threshold.unwrap_or(5000);
        let balance_weight = config.balance_weight.unwrap_or(0.2);
        let router = ShardRouter::with_options(num_shards, df_threshold, balance_weight);
        let shard_actors = spawn_shard_actors(&shards);

        Ok(Self {
            shards,
            shard_actors,
            router: Mutex::new(router),
            base_path: base_path.to_string(),
            schema,
            field_map,
            config: config.clone(),
            has_deletes: AtomicBool::new(false),
            text_fields,
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
            let df_threshold = config.df_threshold.unwrap_or(5000);
            let balance_weight = config.balance_weight.unwrap_or(0.2);
            ShardRouter::with_options(num_shards, df_threshold, balance_weight)
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
        let text_fields = find_text_fields(&schema);
        let shard_actors = spawn_shard_actors(&shards);

        Ok(Self {
            shards,
            shard_actors,
            router: Mutex::new(router),
            base_path: base_path.to_string(),
            schema,
            field_map,
            config,
            has_deletes: AtomicBool::new(false),
            text_fields,
        })
    }

    /// Add a document, automatically tokenizing text fields for routing.
    ///
    /// Extracts text from all text fields in the doc, tokenizes via the index's
    /// tokenizer, hashes the tokens, routes to the best shard, and sends the doc
    /// to the shard actor for insertion (non-blocking, parallel per shard).
    pub fn add_document(&self, doc: LucivyDocument, node_id: u64) -> Result<usize, String> {
        let token_hashes = self.extract_token_hashes(&doc);
        self.route_and_send(doc, node_id, &token_hashes)
    }

    /// Add a document with pre-computed token hashes (for callers who already tokenized).
    pub fn add_document_with_hashes(
        &self,
        doc: LucivyDocument,
        node_id: u64,
        token_hashes: &[u64],
    ) -> Result<usize, String> {
        self.route_and_send(doc, node_id, token_hashes)
    }

    /// Route a document to a shard and send it to the shard actor.
    /// Fire-and-forget: the actor handles the actual writer insert.
    fn route_and_send(
        &self,
        doc: LucivyDocument,
        node_id: u64,
        token_hashes: &[u64],
    ) -> Result<usize, String> {
        let shard_id = {
            let mut router = self.router.lock().map_err(|_| "router lock poisoned")?;
            let sid = router.route(token_hashes);
            router.record_node_id(node_id, sid);
            sid
        };

        // Send doc to shard actor — non-blocking, the actor writes it.
        let env = ShardInsertMsg.into_envelope_with_local(doc);
        self.shard_actors[shard_id]
            .send(env)
            .map_err(|_| format!("shard_{shard_id} actor channel closed"))?;

        Ok(shard_id)
    }

    /// Extract token hashes from a document's text fields.
    fn extract_token_hashes(&self, doc: &LucivyDocument) -> Vec<u64> {
        let mut hashes = Vec::new();
        for (field, value) in doc.field_values() {
            if !self.text_fields.contains(&field) {
                continue;
            }
            if let Some(text) = value.as_value().as_str() {
                // Tokenize using the index's tokenizer for this field.
                let index = &self.shards[0].index;
                let tokenizer_name = match self.schema.get_field_entry(field).field_type() {
                    FieldType::Str(opts) => opts
                        .get_indexing_options()
                        .map(|o| o.tokenizer())
                        .unwrap_or("default"),
                    _ => "default",
                };
                if let Some(mut tokenizer) = index.tokenizers().get(tokenizer_name) {
                    let mut stream = tokenizer.token_stream(text);
                    while let Some(token) = stream.next() {
                        hashes.push(ShardRouter::hash_token(&token.text));
                    }
                }
            }
        }
        hashes
    }

    /// Search all shards in parallel and merge top-K results.
    ///
    /// Scatter-gather: builds the query Weight ONCE with global BM25 stats
    /// (aggregated across all shards), then dispatches the pre-compiled Weight
    /// to each shard actor. Each actor just scores its local segments.
    ///
    /// Returns results sorted by descending score.
    pub fn search(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    ) -> Result<Vec<ShardedSearchResult>, String> {
        // Flush uncommitted changes on all shards before searching (via actors).
        {
            let scheduler = global_scheduler();
            let mut flush_rxs = Vec::new();
            for (i, (shard, actor_ref)) in self.shards.iter().zip(&self.shard_actors).enumerate() {
                if shard.has_uncommitted() {
                    let (env, rx) = ShardCommitMsg.into_request();
                    actor_ref
                        .send(env)
                        .map_err(|_| format!("shard_{i} actor closed"))?;
                    flush_rxs.push((i, rx));
                }
            }
            for (i, rx) in flush_rxs {
                rx.wait_cooperative(|| scheduler.run_one_step())
                    .map_err(|e| format!("flush shard_{i}: {e}"))?;
            }
        }

        // ── Scatter: build Weight once with global stats ────────────────

        // Collect searchers from all shards for aggregated BM25 stats.
        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();
        let global_stats = AggregatedBm25StatsOwned::new(searchers);

        // Build the query using shard 0's index (schema + tokenizers are identical).
        let query = crate::query::build_query(
            query_config,
            &self.schema,
            &self.shards[0].index,
            highlight_sink,
        )?;

        // Compile the Weight with global stats. Use shard 0's searcher for schema access.
        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            &global_stats,
            &searcher_0,
        );
        let weight: Arc<dyn Weight> = query
            .weight(enable_scoring)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        // ── Gather: dispatch Weight to all shard actors via Envelope ────

        let mut receivers = Vec::with_capacity(self.shard_actors.len());
        for actor_ref in &self.shard_actors {
            let msg = ShardSearchMsg { top_k };
            let (env, rx) = msg.into_request_with_local(Arc::clone(&weight));
            actor_ref
                .send(env)
                .map_err(|_| "shard actor channel closed")?;
            receivers.push(rx);
        }

        // Collect results and heap-merge top-K.
        let scheduler = global_scheduler();
        let mut heap = BinaryHeap::with_capacity(top_k + 1);

        for (shard_id, rx) in receivers.into_iter().enumerate() {
            let reply_bytes = rx.wait_cooperative(|| scheduler.run_one_step())
                .map_err(|e| format!("search shard_{shard_id}: {e}"))?;
            let shard_reply = ShardSearchReply::decode(&reply_bytes)
                .map_err(|e| format!("decode shard_{shard_id} reply: {e}"))?;
            let shard_hits = shard_reply.results;

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

    /// Commit all shards in parallel via shard actors, then persist router state.
    ///
    /// If deletes happened since last commit, resyncs the router's token counters
    /// from the actual term dictionaries so they stay accurate.
    pub fn commit(&self) -> Result<(), String> {
        // Send Commit to all shard actors in parallel.
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.shard_actors.len());
        for actor_ref in &self.shard_actors {
            let (env, rx) = ShardCommitMsg.into_request();
            actor_ref
                .send(env)
                .map_err(|_| "shard actor channel closed")?;
            receivers.push(rx);
        }

        // Wait for all commits to complete.
        for (i, rx) in receivers.into_iter().enumerate() {
            rx.wait_cooperative(|| scheduler.run_one_step())
                .map_err(|e| format!("commit shard_{i}: {e}"))?;
        }

        // Resync router counters from index if deletes happened.
        let mut router = self.router.lock().map_err(|_| "router lock poisoned")?;

        if self.has_deletes.swap(false, Ordering::Relaxed) {
            // Update doc counts from actual index state.
            for (i, shard) in self.shards.iter().enumerate() {
                router.shard_doc_counts_mut()[i] = shard.reader.searcher().num_docs();
            }

            // Resync token counters from term dictionaries.
            let text_fields = &self.text_fields;
            let shards = &self.shards;
            router.resync(|visitor| {
                for (shard_id, shard) in shards.iter().enumerate() {
                    visitor(shard_id, &|term_visitor| {
                        let searcher = shard.reader.searcher();
                        for seg_reader in searcher.segment_readers() {
                            for &field in text_fields {
                                let inv_index = seg_reader.inverted_index(field).unwrap();
                                let term_dict = inv_index.terms();
                                let mut stream = term_dict.stream().unwrap();
                                while stream.advance() {
                                    let key = stream.key();
                                    let doc_freq = stream.value().doc_freq;
                                    term_visitor(key, doc_freq);
                                }
                            }
                        }
                    });
                }
            });
        }

        // Persist router state.
        let stats_bytes = router.to_bytes();
        std::fs::write(
            Path::new(&self.base_path).join(SHARD_STATS_FILE),
            &stats_bytes,
        )
        .map_err(|e| format!("cannot write shard stats: {e}"))?;

        Ok(())
    }

    /// Close all shards (commit pending writes via actors, then release locks).
    pub fn close(&self) -> Result<(), String> {
        // Commit all pending writes via shard actors (flushes mailbox + writer).
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.shard_actors.len());
        for (i, actor_ref) in self.shard_actors.iter().enumerate() {
            let (env, rx) = ShardCommitMsg.into_request();
            actor_ref
                .send(env)
                .map_err(|_| format!("shard_{i} actor closed on close"))?;
            receivers.push((i, rx));
        }
        for (i, rx) in receivers {
            rx.wait_cooperative(|| scheduler.run_one_step())
                .map_err(|e| format!("commit on close shard_{i}: {e}"))?;
        }

        // Persist router state.
        {
            let router = self.router.lock().map_err(|_| "router lock poisoned")?;
            let stats_bytes = router.to_bytes();
            std::fs::write(
                Path::new(&self.base_path).join(SHARD_STATS_FILE),
                &stats_bytes,
            )
            .map_err(|e| format!("cannot write shard stats on close: {e}"))?;
        }

        // Release writer locks.
        for (i, shard) in self.shards.iter().enumerate() {
            shard
                .close()
                .map_err(|e| format!("close shard_{i}: {e}"))?;
        }
        Ok(())
    }

    /// Delete a document by its _node_id via shard actors.
    ///
    /// Uses the node_id → shard_id mapping to target only the correct shard.
    /// If the mapping is missing, broadcasts the delete to all shards.
    pub fn delete_by_node_id(&self, node_id: u64) -> Result<(), String> {
        let nid_field = self
            .field(NODE_ID_FIELD)
            .ok_or("_node_id field not found")?;
        let term = Term::from_field_u64(nid_field, node_id);

        let target_shard = {
            let mut router = self.router.lock().map_err(|_| "router lock poisoned")?;
            router.remove_node_id(node_id)
        };

        if let Some(shard_id) = target_shard {
            // Targeted delete via actor.
            let env = ShardDeleteMsg.into_envelope_with_local(term);
            self.shard_actors[shard_id]
                .send(env)
                .map_err(|_| format!("shard_{shard_id} actor channel closed"))?;
        } else {
            // Broadcast delete to all shard actors.
            for (i, actor_ref) in self.shard_actors.iter().enumerate() {
                let env = ShardDeleteMsg.into_envelope_with_local(term.clone());
                actor_ref
                    .send(env)
                    .map_err(|_| format!("shard_{i} actor channel closed"))?;
            }
        }

        self.has_deletes.store(true, Ordering::Relaxed);
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
            handle.add_document(doc, i).unwrap();
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
            handle.add_document(doc, i).unwrap();
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
                handle.add_document(doc, i).unwrap();
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
            handle.add_document(doc, i).unwrap();
        }
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 10);

        // Delete node_id=5 — should target a single shard + resync counters
        handle.delete_by_node_id(5).unwrap();
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 9);

        // Router doc counts should reflect the delete
        let (counts, total) = handle.router_stats().unwrap();
        assert_eq!(total, 9);
        assert_eq!(counts.iter().sum::<u64>(), 9);
    }
}
