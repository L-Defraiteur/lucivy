//! Sharded index handle.
//!
//! `ShardedHandle` wraps N `LucivyHandle` instances, each in its own sub-directory.
//! Documents are routed to shards via `ShardRouter` (token-aware IDF-weighted).
//! Search dispatches to all shard actors in parallel via the global scheduler
//! and merges results via a binary heap.
//!
//! WASM compatible: uses the actor system (persistent threads or cooperative).

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ld_lucivy::collector::Collector;
use ld_lucivy::actor::envelope::{type_tag_hash, Envelope, Message};
use ld_lucivy::actor::handler::TypedHandler;
use ld_lucivy::actor::generic_actor::GenericActor;
use ld_lucivy::actor::mailbox::{mailbox, ActorRef};
use ld_lucivy::actor::scheduler::global_scheduler;
use ld_lucivy::actor::{ActorStatus, Priority};
use ld_lucivy::query::Weight;
use ld_lucivy::schema::{Field, FieldType, Schema, Term, Value};
use ld_lucivy::tokenizer::{PreTokenizedString, Token, TokenizerManager};
use ld_lucivy::indexer::PreTokenizedData;
use ld_lucivy::{DocAddress, Index, LucivyDocument};

use crate::directory::StdFsDirectory;
use crate::handle::{LucivyHandle, NODE_ID_FIELD};
use crate::query::{QueryConfig, SchemaConfig};
use crate::shard_router::ShardRouter;

// ─── Storage abstraction ────────────────────────────────────────────────────

/// Abstraction for shard storage.
///
/// Each implementation creates `LucivyHandle`s with its own concrete Directory type
/// (StdFsDirectory, BlobDirectory, RamDirectory, etc.). Also handles root-level
/// file persistence (config, stats).
///
/// Implementations:
/// - `FsShardStorage` — filesystem (default)
/// - `BlobShardStorage` — ACID (mmap + DB blob) [future, in rag3weaver]
/// - `MemShardStorage` — in-memory (tests) [future]
pub trait ShardStorage: Send + Sync {
    /// Create a new LucivyHandle for a shard (index creation).
    fn create_shard_handle(
        &self,
        shard_id: usize,
        config: &SchemaConfig,
    ) -> Result<LucivyHandle, String>;

    /// Open an existing LucivyHandle for a shard.
    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String>;

    /// Write a root-level file (e.g. _shard_config.json, _shard_stats.bin).
    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String>;

    /// Read a root-level file.
    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String>;

    /// Check if a root-level file exists.
    fn root_file_exists(&self, name: &str) -> bool;
}

/// Filesystem-based shard storage (default).
pub struct FsShardStorage {
    base_path: String,
}

impl FsShardStorage {
    /// Create a new FsShardStorage, creating the base directory if needed.
    pub fn new(base_path: &str) -> Result<Self, String> {
        std::fs::create_dir_all(base_path)
            .map_err(|e| format!("cannot create base dir: {e}"))?;
        Ok(Self {
            base_path: base_path.to_string(),
        })
    }
}

impl ShardStorage for FsShardStorage {
    fn create_shard_handle(
        &self,
        shard_id: usize,
        config: &SchemaConfig,
    ) -> Result<LucivyHandle, String> {
        let shard_dir = Path::new(&self.base_path).join(format!("shard_{shard_id}"));
        std::fs::create_dir_all(&shard_dir)
            .map_err(|e| format!("cannot create shard_{shard_id} dir: {e}"))?;
        let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())
            .map_err(|e| format!("cannot open shard_{shard_id} dir: {e}"))?;
        LucivyHandle::create(dir, config)
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let shard_dir = Path::new(&self.base_path).join(format!("shard_{shard_id}"));
        let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())
            .map_err(|e| format!("cannot open shard_{shard_id} dir: {e}"))?;
        LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        std::fs::write(Path::new(&self.base_path).join(name), data)
            .map_err(|e| format!("cannot write {name}: {e}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        std::fs::read(Path::new(&self.base_path).join(name))
            .map_err(|e| format!("cannot read {name}: {e}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        Path::new(&self.base_path).join(name).exists()
    }
}

/// In-memory shard storage using RamDirectory (no filesystem).
///
/// Useful for WASM/emscripten, tests, and ephemeral indexes.
/// Root files are stored in a HashMap. Each shard gets its own RamDirectory.
///
/// Supports two workflows:
/// - **Create**: `create_shard_handle` creates a fresh RamDirectory per shard
/// - **Open**: pre-populate via `import_shard_file`, then `open_shard_handle`
pub struct RamShardStorage {
    root_files: Mutex<std::collections::HashMap<String, Vec<u8>>>,
    shard_dirs: Mutex<std::collections::HashMap<usize, ld_lucivy::directory::RamDirectory>>,
}

impl RamShardStorage {
    pub fn new() -> Self {
        Self {
            root_files: Mutex::new(std::collections::HashMap::new()),
            shard_dirs: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Import a file into a shard's directory (for open/import workflows).
    /// Creates the shard directory if it doesn't exist.
    pub fn import_shard_file(&self, shard_id: usize, name: &str, data: Vec<u8>) {
        use ld_lucivy::directory::Directory;
        let mut dirs = self.shard_dirs.lock().unwrap();
        let dir = dirs.entry(shard_id)
            .or_insert_with(ld_lucivy::directory::RamDirectory::create);
        dir.atomic_write(Path::new(name), &data).unwrap();
    }
}

impl ShardStorage for RamShardStorage {
    fn create_shard_handle(
        &self,
        shard_id: usize,
        config: &SchemaConfig,
    ) -> Result<LucivyHandle, String> {
        let dir = ld_lucivy::directory::RamDirectory::create();
        let mut dirs = self.shard_dirs.lock().map_err(|_| "lock poisoned")?;
        dirs.insert(shard_id, dir.clone());
        LucivyHandle::create(dir, config)
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let dirs = self.shard_dirs.lock().map_err(|_| "lock poisoned")?;
        let dir = dirs.get(&shard_id)
            .ok_or_else(|| format!("shard_{shard_id} not found in RamShardStorage"))?
            .clone();
        LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        let mut files = self.root_files.lock().map_err(|_| "lock poisoned")?;
        files.insert(name.to_string(), data.to_vec());
        Ok(())
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        let files = self.root_files.lock().map_err(|_| "lock poisoned")?;
        files.get(name).cloned()
            .ok_or_else(|| format!("root file '{name}' not found"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.root_files.lock()
            .map(|f| f.contains_key(name))
            .unwrap_or(false)
    }
}

/// BlobStore-backed shard storage for ACID persistence.
///
/// Each shard gets a `BlobDirectory` with a unique namespace in the store.
/// Reads are served from local mmap cache (zero-copy). Writes go to both
/// cache and BlobStore (source of truth).
///
/// Usage:
/// ```ignore
/// let store = Arc::new(MemBlobStore::new());  // or CypherBlobStore, PostgresBlobStore, etc.
/// let storage = BlobShardStorage::new(store, "my_entity", Path::new("/tmp/cache"));
/// let handle = ShardedHandle::create_with_storage(Box::new(storage), &config)?;
/// ```
pub struct BlobShardStorage<S: crate::blob_store::BlobStore> {
    store: std::sync::Arc<S>,
    /// Base index name (e.g. "my_entity"). Shards get "my_entity/shard_0", etc.
    index_name: String,
    /// Local cache base directory for mmap files.
    cache_base: std::path::PathBuf,
}

impl<S: crate::blob_store::BlobStore> BlobShardStorage<S> {
    /// Create a new BlobShardStorage.
    ///
    /// - `store`: the blob store backend (shared, Arc'd)
    /// - `index_name`: base name for this index (e.g. "entity_products")
    /// - `cache_base`: local directory for mmap cache files
    pub fn new(
        store: std::sync::Arc<S>,
        index_name: impl Into<String>,
        cache_base: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            store,
            index_name: index_name.into(),
            cache_base: cache_base.into(),
        }
    }

    fn shard_name(&self, shard_id: usize) -> String {
        format!("{}/shard_{shard_id}", self.index_name)
    }

    /// Root-level files use index_name directly (no shard prefix).
    fn root_blob_name(&self) -> &str {
        &self.index_name
    }
}

impl<S: crate::blob_store::BlobStore> ShardStorage for BlobShardStorage<S> {
    fn create_shard_handle(
        &self,
        shard_id: usize,
        config: &SchemaConfig,
    ) -> Result<LucivyHandle, String> {
        let shard_name = self.shard_name(shard_id);
        let dir = crate::blob_directory::BlobDirectory::new(
            self.store.clone(),
            &shard_name,
            &self.cache_base,
        )
        .map_err(|e| format!("cannot create blob dir shard_{shard_id}: {e}"))?;
        LucivyHandle::create(dir, config)
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let shard_name = self.shard_name(shard_id);
        let dir = crate::blob_directory::BlobDirectory::new(
            self.store.clone(),
            &shard_name,
            &self.cache_base,
        )
        .map_err(|e| format!("cannot open blob dir shard_{shard_id}: {e}"))?;
        LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        self.store
            .save(self.root_blob_name(), name, data)
            .map_err(|e| format!("cannot write root {name}: {e}"))
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.store
            .load(self.root_blob_name(), name)
            .map_err(|e| format!("cannot read root {name}: {e}"))
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.store
            .exists(self.root_blob_name(), name)
            .unwrap_or(false)
    }
}

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
struct ShardCommitMsg {
    fast: bool,
}

impl Message for ShardCommitMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ShardCommitMsg") }
    fn encode(&self) -> Vec<u8> { vec![if self.fast { 1 } else { 0 }] }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        Ok(Self { fast: bytes.first().copied().unwrap_or(0) == 1 })
    }
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

// ─── Pipeline Actor Messages ─────────────────────────────────────────────────
//
// Reader actors tokenize documents for routing. Router actor routes to shards.
// The pipeline: ReaderActor[pool] → RouterActor → ShardActor[target].

/// Tokenize a document for routing. (LucivyDocument, u64 node_id) in Envelope.local.
struct ReaderTokenizeMsg;

impl Message for ReaderTokenizeMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ReaderTokenizeMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Tokenize a batch of documents. Vec<(LucivyDocument, u64)> in Envelope.local.
struct ReaderBatchMsg;

impl Message for ReaderBatchMsg {
    fn type_tag() -> u64 { type_tag_hash(b"ReaderBatchMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Route a pre-tokenized document. (LucivyDocument, u64 node_id, Vec<u64> hashes) in local.
struct RouterRouteMsg;

impl Message for RouterRouteMsg {
    fn type_tag() -> u64 { type_tag_hash(b"RouterRouteMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Drain: request/reply — ensures all prior messages in the mailbox are processed.
struct PipelineDrainMsg;

impl Message for PipelineDrainMsg {
    fn type_tag() -> u64 { type_tag_hash(b"PipelineDrainMsg") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

/// Drain reply (ack).
struct PipelineDrainReply;

impl Message for PipelineDrainReply {
    fn type_tag() -> u64 { type_tag_hash(b"PipelineDrainReply") }
    fn encode(&self) -> Vec<u8> { vec![] }
    fn decode(_bytes: &[u8]) -> Result<Self, String> { Ok(Self) }
}

// ─── Pipeline Actor Creation ─────────────────────────────────────────────────

/// State needed by ReaderActors (shared, read-only).
#[allow(dead_code)]
struct ReaderContext {
    schema: Schema,
    text_fields: Vec<Field>,
    tokenizer_manager: TokenizerManager,
    router_actor: ActorRef<Envelope>,
}

/// Create a ReaderActor that tokenizes documents and forwards to RouterActor.
///
/// Produces both routing hashes AND PreTokenizedData in a single pass,
/// eliminating double tokenization in the SegmentWriter.
#[allow(dead_code)]
fn create_reader_actor(ctx: Arc<ReaderContext>) -> GenericActor {
    let mut actor = GenericActor::new("reader");

    let ctx2 = Arc::clone(&ctx);
    actor.register(TypedHandler::<ReaderTokenizeMsg, _>::new(
        move |_state, _msg, _reply, local| {
            let (doc, node_id) = *local.unwrap().downcast::<(LucivyDocument, u64)>().unwrap();

            // Tokenize once: produce hashes + PreTokenizedData (CPU-bound work)
            let (hashes, pre_tokenized) = tokenize_for_pipeline(
                &doc, &ctx2.schema, &ctx2.text_fields, &ctx2.tokenizer_manager,
            );

            // Forward to router with pre-tokenized data
            let payload: (LucivyDocument, u64, Vec<u64>, PreTokenizedData) =
                (doc, node_id, hashes, pre_tokenized);
            let env = RouterRouteMsg.into_envelope_with_local(payload);
            let _ = ctx2.router_actor.send(env);

            ActorStatus::Continue
        },
    ));

    // Batch handler: tokenize all docs in one message, forward each to router.
    let ctx3 = Arc::clone(&ctx);
    actor.register(TypedHandler::<ReaderBatchMsg, _>::new(
        move |_state, _msg, _reply, local| {
            let batch = *local.unwrap().downcast::<Vec<(LucivyDocument, u64)>>().unwrap();

            for (doc, node_id) in batch {
                let (hashes, pre_tokenized) = tokenize_for_pipeline(
                    &doc, &ctx3.schema, &ctx3.text_fields, &ctx3.tokenizer_manager,
                );
                let payload: (LucivyDocument, u64, Vec<u64>, PreTokenizedData) =
                    (doc, node_id, hashes, pre_tokenized);
                let env = RouterRouteMsg.into_envelope_with_local(payload);
                let _ = ctx3.router_actor.send(env);
            }

            ActorStatus::Continue
        },
    ));

    // Drain handler: ack when all prior messages are processed (FIFO guarantee).
    actor.register(TypedHandler::<PipelineDrainMsg, _>::new(
        |_state, _msg, reply, _local| {
            if let Some(reply) = reply {
                reply.send(PipelineDrainReply);
            }
            ActorStatus::Continue
        },
    ));

    actor
}

/// Create the RouterActor that routes tokenized documents to shard actors.
#[allow(dead_code)]
fn create_router_actor(
    router: Arc<Mutex<ShardRouter>>,
    shard_actors: Vec<ActorRef<Envelope>>,
) -> GenericActor {
    let mut actor = GenericActor::new("router");

    let router2 = Arc::clone(&router);
    let shard_actors2 = shard_actors.clone();
    actor.register(TypedHandler::<RouterRouteMsg, _>::new(
        move |_state, _msg, _reply, local| {
            let (doc, node_id, hashes, pre_tokenized) = *local.unwrap()
                .downcast::<(LucivyDocument, u64, Vec<u64>, PreTokenizedData)>().unwrap();

            let shard_id = {
                let mut r = router2.lock().unwrap();
                let sid = r.route(&hashes);
                r.record_node_id(node_id, sid);
                sid
            };

            // Send (doc, pre_tokenized) to shard actor.
            let pre_tok = if pre_tokenized.is_empty() { None } else { Some(pre_tokenized) };
            let payload: (LucivyDocument, Option<PreTokenizedData>) = (doc, pre_tok);
            let env = ShardInsertMsg.into_envelope_with_local(payload);
            let _ = shard_actors2[shard_id].send(env);

            ActorStatus::Continue
        },
    ));

    // Drain handler
    let _router3 = Arc::clone(&router);
    let _shard_actors3 = shard_actors;
    actor.register(TypedHandler::<PipelineDrainMsg, _>::new(
        move |_state, _msg, reply, _local| {
            if let Some(reply) = reply {
                reply.send(PipelineDrainReply);
            }
            ActorStatus::Continue
        },
    ));

    actor
}

/// Tokenize text fields once: produce routing hashes AND PreTokenizedData.
///
/// The document is NOT modified. The PreTokenizedData is passed alongside
/// the document through the pipeline to the SegmentWriter, which uses it
/// instead of re-tokenizing (eliminates double tokenization).
/// Tokenize text fields for routing hashes only (no PreTokenizedData).
/// Used by the single-shard direct path to avoid pipeline overhead.
fn extract_token_hashes_from(
    doc: &LucivyDocument,
    schema: &Schema,
    text_fields: &[Field],
    tokenizer_manager: &TokenizerManager,
) -> Vec<u64> {
    let mut hashes = Vec::new();
    for (field, value) in doc.field_values() {
        if !text_fields.contains(&field) {
            continue;
        }
        if let Some(text) = value.as_value().as_str() {
            let tokenizer_name = match schema.get_field_entry(field).field_type() {
                FieldType::Str(opts) => opts
                    .get_indexing_options()
                    .map(|o| o.tokenizer())
                    .unwrap_or("default"),
                _ => "default",
            };
            if let Some(mut tokenizer) = tokenizer_manager.get(tokenizer_name) {
                let mut stream = tokenizer.token_stream(text);
                while let Some(token) = stream.next() {
                    hashes.push(ShardRouter::hash_token(&token.text));
                }
            }
        }
    }
    hashes
}

fn tokenize_for_pipeline(
    doc: &LucivyDocument,
    schema: &Schema,
    text_fields: &[Field],
    tokenizer_manager: &TokenizerManager,
) -> (Vec<u64>, PreTokenizedData) {
    let mut hashes = Vec::new();
    // Group pre-tokenized data per field (supports multi-value fields).
    let mut field_map: std::collections::HashMap<Field, Vec<PreTokenizedString>> =
        std::collections::HashMap::new();

    for (field, value) in doc.field_values() {
        if !text_fields.contains(&field) {
            continue;
        }
        if let Some(text) = value.as_value().as_str() {
            let tokenizer_name = match schema.get_field_entry(field).field_type() {
                FieldType::Str(opts) => opts
                    .get_indexing_options()
                    .map(|o| o.tokenizer())
                    .unwrap_or("default"),
                _ => "default",
            };
            if let Some(mut tokenizer) = tokenizer_manager.get(tokenizer_name) {
                let mut tokens = Vec::new();
                let mut stream = tokenizer.token_stream(text);
                while stream.advance() {
                    let token = stream.token_mut();
                    hashes.push(ShardRouter::hash_token(&token.text));
                    tokens.push(Token {
                        offset_from: token.offset_from,
                        offset_to: token.offset_to,
                        position: token.position,
                        text: std::mem::take(&mut token.text), // move, not clone
                        position_length: token.position_length,
                    });
                }
                drop(stream);

                field_map.entry(field).or_default().push(PreTokenizedString {
                    text: text.to_string(),
                    tokens,
                });
            }
        }
    }
    let pre_tokenized: PreTokenizedData = field_map.into_iter().collect();
    (hashes, pre_tokenized)
}

// ─── Shard Actor Creation ───────────────────────────────────────────────────

/// Build an AliveBitSet that only marks docs whose _node_id is in allowed_ids.
fn build_node_filter_bitset(
    seg_reader: &ld_lucivy::SegmentReader,
    allowed_ids: &HashSet<u64>,
) -> Result<ld_lucivy::fastfield::AliveBitSet, String> {
    let node_id_column = seg_reader.fast_fields().u64(NODE_ID_FIELD)
        .map_err(|e| format!("fast field {NODE_ID_FIELD}: {e}"))?;
    let max_doc = seg_reader.max_doc();
    let mut bitset = ld_lucivy::BitSet::with_max_value(max_doc);
    for doc in 0..max_doc {
        let val = node_id_column.first(doc).unwrap_or(u64::MAX);
        if allowed_ids.contains(&val) {
            bitset.insert(doc);
        }
    }
    Ok(ld_lucivy::fastfield::AliveBitSet::from_bitset(&bitset))
}

/// Execute a pre-compiled Weight on a single shard's segments.
///
/// If `filter` is provided, injects an AliveBitSet pre-filter per segment
/// so that only docs with matching _node_id are visited by the scorer.
fn execute_weight_on_shard(
    handle: &LucivyHandle,
    shard_id: usize,
    weight: &dyn Weight,
    top_k: usize,
    filter: Option<Arc<HashSet<u64>>>,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let searcher = handle.reader.searcher();
    let collector = ld_lucivy::collector::TopDocs::with_limit(top_k).order_by_score();

    let segment_readers = searcher.segment_readers();
    let mut fruits = Vec::with_capacity(segment_readers.len());

    for (seg_ord, seg_reader) in segment_readers.iter().enumerate() {
        if let Some(ref allowed_ids) = filter {
            // Pre-filter: inject alive bitset so scorer skips non-matching docs.
            let filter_bitset = build_node_filter_bitset(seg_reader, allowed_ids)?;
            let mut filtered_reader = seg_reader.clone();
            filtered_reader.set_alive_bitset(filter_bitset);
            let fruit = collector
                .collect_segment(weight, seg_ord as u32, &filtered_reader)
                .map_err(|e| format!("collect shard_{shard_id} seg_{seg_ord}: {e}"))?;
            fruits.push(fruit);
        } else {
            let fruit = collector
                .collect_segment(weight, seg_ord as u32, seg_reader)
                .map_err(|e| format!("collect shard_{shard_id} seg_{seg_ord}: {e}"))?;
            fruits.push(fruit);
        }
    }

    collector
        .merge_fruits(fruits)
        .map_err(|e| format!("merge shard_{shard_id}: {e}"))
}

// ---------------------------------------------------------------------------
// ShardActor — typed actor replacing GenericActor<Envelope>
// ---------------------------------------------------------------------------

/// Message type for shard actors. Exhaustive enum — no Envelope overhead.
pub(crate) enum ShardMsg {
    Search {
        weight: Arc<dyn Weight>,
        top_k: usize,
        filter: Option<Arc<HashSet<u64>>>,
        reply: luciole::Reply<Result<Vec<(f32, DocAddress)>, String>>,
    },
    Insert {
        doc: LucivyDocument,
        pre_tokenized: Option<PreTokenizedData>,
    },
    Commit {
        fast: bool,
        reply: luciole::Reply<Result<(), String>>,
    },
    Delete {
        term: Term,
    },
    Drain(luciole::DrainMsg),
}

impl From<luciole::DrainMsg> for ShardMsg {
    fn from(d: luciole::DrainMsg) -> Self { ShardMsg::Drain(d) }
}

/// Typed shard actor: search, insert, commit, delete.
struct ShardActor {
    shard_id: usize,
    handle: Arc<LucivyHandle>,
}

impl luciole::Actor for ShardActor {
    type Msg = ShardMsg;

    fn name(&self) -> &'static str { "shard" }

    fn priority(&self) -> Priority { Priority::Medium }

    fn handle(&mut self, msg: ShardMsg) -> ActorStatus {
        match msg {
            ShardMsg::Search { weight, top_k, filter, reply } => {
                let result = execute_weight_on_shard(
                    &self.handle, self.shard_id, weight.as_ref(), top_k, filter,
                );
                reply.send(result);
            }
            ShardMsg::Insert { doc, pre_tokenized } => {
                let mut guard = self.handle.writer.lock().unwrap();
                if let Some(writer) = guard.as_mut() {
                    let _ = if let Some(pt) = pre_tokenized {
                        writer.add_document_pre_tokenized(doc, pt)
                    } else {
                        writer.add_document(doc)
                    };
                    self.handle.mark_uncommitted();
                }
            }
            ShardMsg::Commit { fast, reply } => {
                let result = (|| -> Result<(), String> {
                    let mut guard = self.handle.writer.lock()
                        .map_err(|_| "lock poisoned".to_string())?;
                    if let Some(ref mut writer) = *guard {
                        if fast {
                            writer.commit_fast()
                                .map_err(|e| format!("commit_fast shard_{}: {e}", self.shard_id))?;
                        } else {
                            writer.commit()
                                .map_err(|e| format!("commit shard_{}: {e}", self.shard_id))?;
                        }
                    }
                    self.handle.mark_committed();
                    self.handle.reader.reload()
                        .map_err(|e| format!("reload shard_{}: {e}", self.shard_id))?;
                    Ok(())
                })();
                reply.send(result);
            }
            ShardMsg::Delete { term } => {
                let mut guard = self.handle.writer.lock().unwrap();
                if let Some(writer) = guard.as_mut() {
                    writer.delete_term(term);
                    self.handle.mark_uncommitted();
                }
            }
            ShardMsg::Drain(d) => {
                d.ack();
            }
        }
        ActorStatus::Continue
    }
}

/// Spawn typed shard actors, return a Pool.
fn spawn_shard_pool(shards: &[Arc<LucivyHandle>]) -> luciole::Pool<ShardMsg> {
    let shards_clone: Vec<Arc<LucivyHandle>> = shards.to_vec();
    luciole::Pool::spawn(shards.len(), 8192, |i| {
        ShardActor {
            shard_id: i,
            handle: shards_clone[i].clone(),
        }
    })
}

// ---------------------------------------------------------------------------
// RouterActor — typed
// ---------------------------------------------------------------------------

pub(crate) enum RouterMsg {
    Route {
        doc: LucivyDocument,
        node_id: u64,
        hashes: Vec<u64>,
        pre_tokenized: PreTokenizedData,
    },
    Drain(luciole::DrainMsg),
}

impl From<luciole::DrainMsg> for RouterMsg {
    fn from(d: luciole::DrainMsg) -> Self { RouterMsg::Drain(d) }
}

struct RouterActor {
    router: Arc<Mutex<ShardRouter>>,
    shard_pool: luciole::Pool<ShardMsg>,
}

impl luciole::Actor for RouterActor {
    type Msg = RouterMsg;
    fn name(&self) -> &'static str { "router" }
    fn priority(&self) -> Priority { Priority::Medium }

    fn handle(&mut self, msg: RouterMsg) -> ActorStatus {
        match msg {
            RouterMsg::Route { doc, node_id, hashes, pre_tokenized } => {
                let shard_id = {
                    let mut r = self.router.lock().unwrap();
                    let sid = r.route(&hashes);
                    r.record_node_id(node_id, sid);
                    sid
                };
                let pre_tok = if pre_tokenized.is_empty() { None } else { Some(pre_tokenized) };
                let _ = self.shard_pool.send_to(shard_id, ShardMsg::Insert {
                    doc,
                    pre_tokenized: pre_tok,
                });
            }
            RouterMsg::Drain(d) => {
                d.ack();
            }
        }
        ActorStatus::Continue
    }
}

fn spawn_router(
    router: Arc<Mutex<ShardRouter>>,
    shard_pool: luciole::Pool<ShardMsg>,
) -> luciole::ActorRef<RouterMsg> {
    let scheduler = global_scheduler();
    let actor = RouterActor { router, shard_pool };
    let (mb, mut ar) = luciole::mailbox::<RouterMsg>(256);
    scheduler.spawn(actor, mb, &mut ar, 256);
    ar
}

// ---------------------------------------------------------------------------
// ReaderActor — typed
// ---------------------------------------------------------------------------

pub(crate) enum ReaderMsg {
    Tokenize { doc: LucivyDocument, node_id: u64 },
    Batch { docs: Vec<(LucivyDocument, u64)> },
    Drain(luciole::DrainMsg),
}

impl From<luciole::DrainMsg> for ReaderMsg {
    fn from(d: luciole::DrainMsg) -> Self { ReaderMsg::Drain(d) }
}

struct ReaderActor {
    schema: Schema,
    text_fields: Vec<Field>,
    tokenizer_manager: TokenizerManager,
    router_ref: luciole::ActorRef<RouterMsg>,
}

impl luciole::Actor for ReaderActor {
    type Msg = ReaderMsg;
    fn name(&self) -> &'static str { "reader" }
    fn priority(&self) -> Priority { Priority::Medium }

    fn handle(&mut self, msg: ReaderMsg) -> ActorStatus {
        match msg {
            ReaderMsg::Tokenize { doc, node_id } => {
                let (hashes, pre_tokenized) = tokenize_for_pipeline(
                    &doc, &self.schema, &self.text_fields, &self.tokenizer_manager,
                );
                let _ = self.router_ref.send(RouterMsg::Route {
                    doc, node_id, hashes, pre_tokenized,
                });
            }
            ReaderMsg::Batch { docs } => {
                for (doc, node_id) in docs {
                    let (hashes, pre_tokenized) = tokenize_for_pipeline(
                        &doc, &self.schema, &self.text_fields, &self.tokenizer_manager,
                    );
                    let _ = self.router_ref.send(RouterMsg::Route {
                        doc, node_id, hashes, pre_tokenized,
                    });
                }
            }
            ReaderMsg::Drain(d) => {
                d.ack();
            }
        }
        ActorStatus::Continue
    }
}

fn spawn_reader_pool(
    schema: &Schema,
    text_fields: &[Field],
    tokenizer_manager: &TokenizerManager,
    router_ref: luciole::ActorRef<RouterMsg>,
    num_readers: usize,
) -> luciole::Pool<ReaderMsg> {
    let schema = schema.clone();
    let text_fields = text_fields.to_vec();
    let tm = tokenizer_manager.clone();
    let rr = router_ref;
    luciole::Pool::spawn(num_readers, 128, |_| {
        ReaderActor {
            schema: schema.clone(),
            text_fields: text_fields.clone(),
            tokenizer_manager: tm.clone(),
            router_ref: rr.clone(),
        }
    })
}

// Legacy GenericActor creation kept for reference/removal later.
#[allow(dead_code)]
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
                Some(w) => execute_weight_on_shard(handle, shard_id, w.as_ref(), msg.top_k, None),
                None => Err("search: no Weight in envelope.local".into()),
            };

            if let Some(reply) = reply {
                match result {
                    Ok(hits) => reply.send(ShardSearchReply { results: hits }),
                    Err(e) => reply.send_err(ld_lucivy::LucivyError::SystemError(e)),
                }
            }
            ActorStatus::Continue
        },
        Priority::Critical,
    ));

    // Insert handler: (LucivyDocument, Option<PreTokenizedData>) in envelope.local
    actor.register(TypedHandler::<ShardInsertMsg, _>::new(
        |state, _msg, reply, local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            // Try (doc, pre_tokenized) tuple first (pipeline path),
            // then plain LucivyDocument (direct path / backward compat).
            let (doc, pre_tok) = if let Some(tuple) = local
                .and_then(|l| l.downcast::<(LucivyDocument, Option<PreTokenizedData>)>().ok())
            {
                (Some(tuple.0), tuple.1)
            } else {
                (None, None)
            };

            let result = match doc {
                Some(doc) => {
                    let mut guard = handle.writer.lock().unwrap();
                    match guard.as_mut() {
                        Some(writer) => {
                            let res = if let Some(pt) = pre_tok {
                                writer.add_document_pre_tokenized(doc, pt)
                            } else {
                                writer.add_document(doc)
                            };
                            res.map(|_| { handle.mark_uncommitted(); })
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
                    Err(e) => reply.send_err(ld_lucivy::LucivyError::SystemError(e)),
                }
            }
            ActorStatus::Continue
        },
    ));

    // Commit handler
    actor.register(TypedHandler::<ShardCommitMsg, _>::new(
        |state, msg, reply, _local| {
            let handle = state.get::<Arc<LucivyHandle>>().unwrap();
            let shard_id = *state.get::<usize>().unwrap();

            let result = (|| -> Result<(), String> {
                let mut guard = handle.writer.lock().map_err(|_| "lock poisoned")?;
                if let Some(ref mut writer) = *guard {
                    if msg.fast {
                        writer.commit_fast().map_err(|e| format!("commit_fast shard_{shard_id}: {e}"))?;
                    } else {
                        writer.commit().map_err(|e| format!("commit shard_{shard_id}: {e}"))?;
                    }
                }
                handle.mark_committed();
                handle.reader.reload().map_err(|e| format!("reload shard_{shard_id}: {e}"))?;
                Ok(())
            })();

            if let Some(reply) = reply {
                match result {
                    Ok(()) => reply.send(ShardOkReply),
                    Err(e) => reply.send_err(ld_lucivy::LucivyError::SystemError(e)),
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
                    Err(e) => reply.send_err(ld_lucivy::LucivyError::SystemError(e)),
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

/// A search result with the document and highlights already resolved.
/// Returned by `search_with_docs()` — ready to serialize/display.
#[derive(Debug)]
pub struct SearchHit {
    pub score: f32,
    pub shard_id: usize,
    pub doc: ld_lucivy::LucivyDocument,
    pub highlights: std::collections::HashMap<String, Vec<[usize; 2]>>,
}

/// Wrapper for BinaryHeap ordering (min-heap by score for top-K).
pub(crate) struct ScoredEntry {
    pub score: f32,
    pub shard_id: usize,
    pub doc_address: DocAddress,
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
    /// Pool of typed shard actors (key-routed by shard_id).
    shard_pool: luciole::Pool<ShardMsg>,
    router: Arc<Mutex<ShardRouter>>,
    /// Storage backend for root files and shard directories.
    storage: Box<dyn ShardStorage>,
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
    /// Pipeline: reader pool (typed, round-robin).
    reader_pool: luciole::Pool<ReaderMsg>,
    /// Pipeline: single router actor (typed).
    router_ref: luciole::ActorRef<RouterMsg>,
    /// Streaming pipeline topology for structured drain.
    pipeline: Arc<luciole::StreamDag>,
}

/// Spawn N GenericActors (one per shard) in the global scheduler.
#[allow(dead_code)]
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

/// Spawn pipeline actors: N ReaderActors + 1 RouterActor.
///
/// ReaderActors tokenize+hash in parallel, RouterActor routes sequentially.
#[allow(dead_code)]
fn spawn_pipeline_actors(
    schema: &Schema,
    text_fields: &[Field],
    index: &Index,
    router: &Arc<Mutex<ShardRouter>>,
    shard_actors: &[ActorRef<Envelope>],
    num_readers: usize,
) -> (Vec<ActorRef<Envelope>>, ActorRef<Envelope>) {
    let scheduler = global_scheduler();

    // Spawn router actor first (readers need its ActorRef).
    let router_generic = create_router_actor(
        Arc::clone(router),
        shard_actors.to_vec(),
    );
    let (router_mb, mut router_ref) = mailbox::<Envelope>(256);
    scheduler.spawn(router_generic, router_mb, &mut router_ref, 256);

    // Build shared reader context.
    let ctx = Arc::new(ReaderContext {
        schema: schema.clone(),
        text_fields: text_fields.to_vec(),
        tokenizer_manager: index.tokenizers().clone(),
        router_actor: router_ref.clone(),
    });

    // Spawn reader actors.
    let mut reader_refs = Vec::with_capacity(num_readers);
    for _ in 0..num_readers {
        let reader = create_reader_actor(Arc::clone(&ctx));
        let (mb, mut r) = mailbox::<Envelope>(128);
        scheduler.spawn(reader, mb, &mut r, 128);
        reader_refs.push(r);
    }

    (reader_refs, router_ref)
}

/// Build the ingestion pipeline topology: readers → router → shards.
fn build_pipeline(
    reader_pool: &luciole::Pool<ReaderMsg>,
    router_ref: &luciole::ActorRef<RouterMsg>,
    shard_pool: &luciole::Pool<ShardMsg>,
    num_readers: usize,
    num_shards: usize,
) -> Arc<luciole::StreamDag> {
    let mut pipeline = luciole::StreamDag::new("ingestion");
    pipeline.add_stage("readers", reader_pool.clone(), num_readers);
    pipeline.add_stage("router", router_ref.clone(), 1);
    pipeline.add_stage("shards", shard_pool.clone(), num_shards);
    pipeline.connect("readers", "router");
    pipeline.connect("router", "shards");
    Arc::new(pipeline)
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
    /// Create a new sharded index on the filesystem.
    pub fn create(base_path: &str, config: &SchemaConfig) -> Result<Self, String> {
        let storage = FsShardStorage::new(base_path)?;
        Self::create_with_storage(Box::new(storage), config)
    }

    /// Open an existing sharded index from the filesystem.
    pub fn open(base_path: &str) -> Result<Self, String> {
        let storage = FsShardStorage::new(base_path)?;
        Self::open_with_storage(Box::new(storage))
    }

    /// Create a new sharded index with a custom storage backend.
    ///
    /// Use this for BlobStore-backed indexes, in-memory indexes, etc.
    pub fn create_with_storage(
        storage: Box<dyn ShardStorage>,
        config: &SchemaConfig,
    ) -> Result<Self, String> {
        let num_shards = config.shards.unwrap_or(1);
        if num_shards == 0 {
            return Err("shards must be >= 1".into());
        }

        // Save shard config at root level.
        let config_json = serde_json::to_string(config)
            .map_err(|e| format!("cannot serialize shard config: {e}"))?;
        storage.write_root_file(SHARD_CONFIG_FILE, config_json.as_bytes())?;

        // Create each shard handle.
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            let handle = storage.create_shard_handle(i, config)?;
            shards.push(Arc::new(handle));
        }

        let schema = shards[0].schema.clone();
        let field_map = shards[0].field_map.clone();
        let text_fields = find_text_fields(&schema);
        let df_threshold = config.df_threshold.unwrap_or(5000);
        let balance_weight = config.balance_weight.unwrap_or(0.2);
        let router = Arc::new(Mutex::new(
            ShardRouter::with_options(num_shards, df_threshold, balance_weight),
        ));
        let shard_pool = spawn_shard_pool(&shards);

        // Pipeline: N readers → 1 router → shard pool
        let num_readers = num_shards.max(2);
        let router_ref = spawn_router(Arc::clone(&router), shard_pool.clone());
        let reader_pool = spawn_reader_pool(
            &schema, &text_fields,
            &shards[0].index.tokenizers(),
            router_ref.clone(),
            num_readers,
        );

        let pipeline = build_pipeline(&reader_pool, &router_ref, &shard_pool, num_readers, num_shards);

        Ok(Self {
            shards,
            shard_pool,
            router,
            storage,
            schema,
            field_map,
            config: config.clone(),
            has_deletes: AtomicBool::new(false),
            text_fields,
            reader_pool,
            router_ref,
            pipeline,
        })
    }

    /// Open an existing sharded index with a custom storage backend.
    pub fn open_with_storage(storage: Box<dyn ShardStorage>) -> Result<Self, String> {
        // Read shard config.
        let config_data = storage.read_root_file(SHARD_CONFIG_FILE)?;
        let config: SchemaConfig = serde_json::from_slice(&config_data)
            .map_err(|e| format!("cannot parse shard config: {e}"))?;

        let num_shards = config.shards.unwrap_or(1);

        // Read shard router state if available.
        let router_inner = if storage.root_file_exists(SHARD_STATS_FILE) {
            let stats_data = storage.read_root_file(SHARD_STATS_FILE)?;
            ShardRouter::from_bytes(&stats_data)?
        } else {
            let df_threshold = config.df_threshold.unwrap_or(5000);
            let balance_weight = config.balance_weight.unwrap_or(0.2);
            ShardRouter::with_options(num_shards, df_threshold, balance_weight)
        };
        let router = Arc::new(Mutex::new(router_inner));

        // Open each shard handle.
        let mut shards = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            let handle = storage.open_shard_handle(i)?;
            shards.push(Arc::new(handle));
        }

        let schema = shards[0].schema.clone();
        let field_map = shards[0].field_map.clone();
        let text_fields = find_text_fields(&schema);
        let shard_pool = spawn_shard_pool(&shards);

        let num_readers = num_shards.max(2);
        let router_ref = spawn_router(Arc::clone(&router), shard_pool.clone());
        let reader_pool = spawn_reader_pool(
            &schema, &text_fields,
            &shards[0].index.tokenizers(),
            router_ref.clone(),
            num_readers,
        );

        let pipeline = build_pipeline(&reader_pool, &router_ref, &shard_pool, num_readers, num_shards);

        Ok(Self {
            shards,
            shard_pool,
            router,
            storage,
            schema,
            field_map,
            config,
            has_deletes: AtomicBool::new(false),
            text_fields,
            reader_pool,
            router_ref,
            pipeline,
        })
    }

    /// Add a document via the ingestion pipeline (non-blocking).
    ///
    /// For multi-shard: sends to ReaderActor (round-robin) for parallel tokenization,
    /// then RouterActor routes to the right shard.
    /// For single-shard: direct path (tokenize + send to shard, no pipeline overhead).
    pub fn add_document(&self, doc: LucivyDocument, node_id: u64) -> Result<(), String> {
        if self.shards.len() == 1 {
            // Direct path: no pipeline overhead for single shard.
            let hashes = extract_token_hashes_from(
                &doc, &self.schema, &self.text_fields, &self.shards[0].index.tokenizers(),
            );
            self.route_and_send(doc, node_id, &hashes)?;
            return Ok(());
        }
        self.reader_pool
            .send(ReaderMsg::Tokenize { doc, node_id })
            .map_err(|_| "reader actor channel closed".to_string())
    }

    /// Add a batch of documents via the ingestion pipeline.
    ///
    /// Splits the batch into N sub-batches (one per ReaderActor) for parallel
    /// tokenization. Each sub-batch is a single message — much less overhead
    /// than N individual add_document calls.
    pub fn add_documents(&self, docs: Vec<(LucivyDocument, u64)>) -> Result<(), String> {
        let n = self.reader_pool.len();
        let mut batches: Vec<Vec<(LucivyDocument, u64)>> = (0..n).map(|_| Vec::new()).collect();
        for (i, doc) in docs.into_iter().enumerate() {
            batches[i % n].push(doc);
        }
        for (i, batch) in batches.into_iter().enumerate() {
            if batch.is_empty() { continue; }
            self.reader_pool.worker(i)
                .send(ReaderMsg::Batch { docs: batch })
                .map_err(|_| "reader actor channel closed".to_string())?;
        }
        Ok(())
    }

    /// Add a document with pre-computed token hashes (bypasses reader actors).
    pub fn add_document_with_hashes(
        &self,
        doc: LucivyDocument,
        node_id: u64,
        token_hashes: &[u64],
    ) -> Result<usize, String> {
        self.route_and_send(doc, node_id, token_hashes)
    }

    /// Route a document to a shard and send it to the shard actor (direct path).
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

        self.shard_pool.send_to(shard_id, ShardMsg::Insert {
            doc,
            pre_tokenized: None,
        }).map_err(|_| format!("shard_{shard_id} actor channel closed"))?;

        Ok(shard_id)
    }

    /// Drain all pipeline actors: wait for readers then router to finish pending work.
    fn drain_pipeline(&self) {
        // Drain readers first (upstream), then router (downstream).
        // Pool::drain sends DrainMsg to each worker and waits.
        self.reader_pool.drain("drain_readers");
        // Router: single actor, drain via request.
        self.router_ref.request(
            |r| RouterMsg::Drain(luciole::DrainMsg(r)),
            "drain_router",
        ).ok();
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
        self.search_internal(query_config, top_k, highlight_sink, None)
    }

    /// Search with node_id filter (only return docs whose _node_id is in allowed_ids).
    pub fn search_filtered(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
        allowed_ids: HashSet<u64>,
    ) -> Result<Vec<ShardedSearchResult>, String> {
        self.search_internal(query_config, top_k, highlight_sink, Some(Arc::new(allowed_ids)))
    }

    fn search_internal(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
        filter: Option<Arc<HashSet<u64>>>,
    ) -> Result<Vec<ShardedSearchResult>, String> {
        let mut dag = crate::search_dag::build_search_dag(
            &self.shards,
            &self.shard_pool,
            &self.pipeline,
            &self.schema,
            query_config,
            top_k,
            highlight_sink,
            filter,
        )?;

        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| format!("search DAG: {e}"))?;

        result.take_output::<Vec<ShardedSearchResult>>("output", "results")
            .ok_or_else(|| "search DAG: no results from output node".to_string())
    }

    // ── Distributed search support ──────────────────────────────────────

    /// Export BM25 statistics for the given query terms.
    /// Used by a coordinator to aggregate stats across multiple nodes.
    pub fn export_stats(
        &self,
        query_config: &QueryConfig,
    ) -> Result<crate::bm25_global::ExportableStats, String> {
        self.drain_pipeline();

        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();

        // Build the query and prescan for contains doc_freq
        let mut query = crate::query::build_query(
            query_config, &self.schema, &self.shards[0].index, None,
        )?;

        // Prescan all segments for contains doc_freq
        let all_segs: Vec<_> = self.shards.iter()
            .flat_map(|s| s.reader.searcher().segment_readers().to_vec())
            .collect();
        let seg_refs: Vec<&ld_lucivy::SegmentReader> = all_segs.iter().collect();
        query.prescan_segments(&seg_refs)
            .map_err(|e| format!("prescan: {e}"))?;

        // Collect standard BM25 terms
        let mut term_set = Vec::new();
        query.query_terms(&mut |term, _| {
            term_set.push(term.clone());
        });

        let mut stats = crate::bm25_global::ExportableStats::from_searchers(&searchers, &term_set);

        // Add contains doc_freqs from prescan
        query.collect_prescan_doc_freqs(&mut stats.contains_doc_freqs);

        // Add regex doc_freqs from prescan
        query.collect_regex_prescan_doc_freqs(&mut stats.regex_doc_freqs);

        Ok(stats)
    }

    /// Search with externally-provided global BM25 stats (distributed mode).
    /// The global_stats should be the merged stats from all nodes.
    pub fn search_with_global_stats(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
        global_stats: &crate::bm25_global::ExportableStats,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    ) -> Result<Vec<ShardedSearchResult>, String> {
        self.drain_pipeline();

        let mut query = crate::query::build_query(
            query_config, &self.schema, &self.shards[0].index,
            highlight_sink.clone(),
        )?;

        // Prescan local segments (populates cache + local doc_freq)
        let all_segs: Vec<_> = self.shards.iter()
            .flat_map(|s| s.reader.searcher().segment_readers().to_vec())
            .collect();
        let seg_refs: Vec<&ld_lucivy::SegmentReader> = all_segs.iter().collect();
        query.prescan_segments(&seg_refs)
            .map_err(|e| format!("prescan: {e}"))?;

        // Inject global contains doc_freqs from coordinator (overrides local prescan doc_freq)
        if !global_stats.contains_doc_freqs.is_empty() {
            query.set_global_contains_doc_freqs(&global_stats.contains_doc_freqs);
        }

        // Inject global regex doc_freqs from coordinator
        if !global_stats.regex_doc_freqs.is_empty() {
            query.set_global_regex_doc_freqs(&global_stats.regex_doc_freqs);
        }

        // Build weight with global stats (total_docs, total_tokens, term doc_freqs)
        let searcher_0 = self.shards[0].reader.searcher();
        let enable = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            Arc::new(global_stats.clone()), &searcher_0,
        );
        let weight: Arc<dyn ld_lucivy::query::Weight> = query
            .weight(enable)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        // Execute weight on each shard locally and collect top-K
        let mut all_hits: Vec<ShardedSearchResult> = Vec::new();
        for (shard_id, shard) in self.shards.iter().enumerate() {
            let searcher = shard.reader.searcher();
            for (seg_ord, seg_reader) in searcher.segment_readers().iter().enumerate() {
                let mut scorer = weight.scorer(seg_reader, 1.0)
                    .map_err(|e| format!("scorer shard {shard_id}: {e}"))?;

                let alive = seg_reader.alive_bitset();
                loop {
                    let doc = scorer.doc();
                    if doc == ld_lucivy::TERMINATED { break; }
                    if alive.map_or(true, |bs| bs.is_alive(doc)) {
                        all_hits.push(ShardedSearchResult {
                            score: scorer.score(),
                            shard_id,
                            doc_address: ld_lucivy::DocAddress::new(seg_ord as u32, doc),
                        });
                    }
                    scorer.advance();
                }
            }
        }

        // Sort by score descending and truncate to top_k
        all_hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        all_hits.truncate(top_k);
        Ok(all_hits)
    }

    /// Search and return results with resolved documents and highlights.
    pub fn search_with_docs(
        &self,
        query_config: &QueryConfig,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, String> {
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let results = self.search(query_config, top_k, Some(sink.clone()))?;

        results.iter().map(|r| {
            let shard = self.shard(r.shard_id)
                .ok_or_else(|| format!("shard {} not found", r.shard_id))?;
            let searcher = shard.reader.searcher();
            let seg_reader = searcher.segment_reader(r.doc_address.segment_ord);
            let doc: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address)
                .map_err(|e| format!("get doc: {e}"))?;
            let highlights = sink.get(seg_reader.segment_id(), r.doc_address.doc_id)
                .unwrap_or_default();

            Ok(SearchHit {
                score: r.score,
                shard_id: r.shard_id,
                doc,
                highlights,
            })
        }).collect()
    }

    /// Commit all shards in parallel via shard actors, then persist router state.
    ///
    /// Drains the ingestion pipeline first (readers → router → shards), then commits.
    /// If deletes happened since last commit, resyncs the router's token counters.
    pub fn commit(&self) -> Result<(), String> {
        self.drain_pipeline();

        // Scatter commit to all shards in parallel.
        let results: Vec<Result<(), String>> = self.shard_pool.scatter(
            |r| ShardMsg::Commit { fast: false, reply: r },
            "commit_shard",
        );
        for (i, result) in results.into_iter().enumerate() {
            result.map_err(|e| format!("commit shard_{i}: {e}"))?;
        }

        // Force reader reload so search sees committed data immediately.
        for shard in &self.shards {
            let _ = shard.reader.reload();
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
        self.storage.write_root_file(SHARD_STATS_FILE, &stats_bytes)?;

        Ok(())
    }

    /// Fast commit: persist data but skip suffix FST rebuild.
    /// Use during bulk indexation. Call commit() at the end to rebuild FSTs.
    pub fn commit_fast(&self) -> Result<(), String> {
        self.drain_pipeline();
        let results: Vec<Result<(), String>> = self.shard_pool.scatter(
            |r| ShardMsg::Commit { fast: true, reply: r },
            "commit_fast_shard",
        );
        for (i, result) in results.into_iter().enumerate() {
            result.map_err(|e| format!("commit_fast shard_{i}: {e}"))?;
        }
        for shard in &self.shards {
            let _ = shard.reader.reload();
        }
        Ok(())
    }

    /// Close all shards (drain pipeline, commit pending writes, release locks).
    pub fn close(&self) -> Result<(), String> {
        self.drain_pipeline();

        // Commit all shards in parallel.
        let results: Vec<Result<(), String>> = self.shard_pool.scatter(
            |r| ShardMsg::Commit { fast: false, reply: r },
            "close_shard",
        );
        for (i, result) in results.into_iter().enumerate() {
            result.map_err(|e| format!("close shard_{i}: {e}"))?;
        }

        // Persist router state.
        {
            let router = self.router.lock().map_err(|_| "router lock poisoned")?;
            let stats_bytes = router.to_bytes();
            self.storage.write_root_file(SHARD_STATS_FILE, &stats_bytes)?;
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
            self.shard_pool.send_to(shard_id, ShardMsg::Delete { term })
                .map_err(|_| format!("shard_{shard_id} actor channel closed"))?;
        } else {
            // Broadcast delete to all shards.
            self.shard_pool.broadcast(|| ShardMsg::Delete { term: term.clone() })
                .map_err(|e| format!("broadcast delete: {e}"))?;
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

    // ── Delta sync ─────────────────────────────────────────────────────

    /// Get per-shard version info (version hash + segment IDs).
    /// Used by clients to request deltas from a server.
    pub fn shard_versions(&self) -> Result<Vec<lucistore::delta_sharded::ShardVersion>, String> {
        let mut versions = Vec::with_capacity(self.shards.len());
        for (i, shard) in self.shards.iter().enumerate() {
            let version = crate::sync::compute_version(shard)?;
            let meta = shard.index.load_metas()
                .map_err(|e| format!("shard_{i} load metas: {e}"))?;
            let segment_ids: HashSet<String> = meta.segments.iter()
                .map(|s| s.id().uuid_string())
                .collect();
            versions.push(lucistore::delta_sharded::ShardVersion {
                shard_id: i,
                version,
                segment_ids,
            });
        }
        Ok(versions)
    }

    /// Export a sharded delta (LUCIDS) from this handle.
    ///
    /// `client_versions`: the per-shard versions the client currently has.
    /// Returns a LUCIDS blob containing only the shards that changed.
    ///
    /// Requires filesystem-backed storage (FsShardStorage). For in-memory
    /// storage (RamShardStorage), use snapshot export instead.
    pub fn export_sharded_delta(
        &self,
        base_path: &str,
        client_versions: &[lucistore::delta_sharded::ShardVersion],
    ) -> Result<Vec<u8>, String> {
        let base = std::path::Path::new(base_path);
        let shard_data: Vec<(usize, &LucivyHandle, std::path::PathBuf)> = self.shards.iter()
            .enumerate()
            .map(|(i, shard)| (i, shard.as_ref(), base.join(format!("shard_{i}"))))
            .collect();

        let shard_refs: Vec<(usize, &LucivyHandle, &std::path::Path)> = shard_data.iter()
            .map(|(i, h, p)| (*i, *h, p.as_path()))
            .collect();

        let shard_config = self.storage.read_root_file(SHARD_CONFIG_FILE).ok();

        let delta = crate::sync::export_sharded_delta(
            &shard_refs,
            client_versions,
            None,
            shard_config,
        )?;

        Ok(lucistore::delta_sharded::serialize_sharded_delta(&delta))
    }

    /// Apply a sharded delta (LUCIDS blob) to this handle.
    ///
    /// Writes new segment files, removes old ones, updates manifests per shard.
    /// Then reloads readers so new data is visible.
    ///
    /// Requires filesystem-backed storage (FsShardStorage).
    pub fn apply_sharded_delta(&self, base_path: &str, blob: &[u8]) -> Result<(), String> {
        let base = std::path::Path::new(base_path);
        let delta = lucistore::delta_sharded::deserialize_sharded_delta(blob)?;

        for (shard_id, shard_delta) in &delta.shard_deltas {
            if *shard_id >= self.shards.len() {
                return Err(format!("shard_{shard_id} not found (have {} shards)", self.shards.len()));
            }

            let shard_path = base.join(format!("shard_{shard_id}"));
            lucistore::fs_utils::apply_delta(&shard_path, shard_delta)?;

            self.shards[*shard_id].reader.reload()
                .map_err(|e| format!("shard_{shard_id} reload: {e}"))?;
        }

        Ok(())
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

    #[test]
    fn test_blob_shard_storage() {
        use crate::blob_store::MemBlobStore;

        let store = std::sync::Arc::new(MemBlobStore::new());
        let cache = std::env::temp_dir().join("lucivy_blob_shard_test");
        let _ = std::fs::remove_dir_all(&cache);

        let config = make_config(2);
        let storage = BlobShardStorage::new(store.clone(), "test_entity", &cache);
        let handle = ShardedHandle::create_with_storage(Box::new(storage), &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..20 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("blob persistence test doc {i}"));
            handle.add_document(doc, i).unwrap();
        }
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 20);

        // Search works
        let query: QueryConfig = serde_json::from_str(
            r#"{"type": "contains", "field": "body", "value": "persistence"}"#,
        ).unwrap();
        let results = handle.search(&query, 10, None).unwrap();
        assert!(!results.is_empty());

        // Close and reopen from same blob store
        handle.close().unwrap();

        let storage2 = BlobShardStorage::new(store.clone(), "test_entity", &cache);
        let handle2 = ShardedHandle::open_with_storage(Box::new(storage2)).unwrap();
        for shard in &handle2.shards {
            shard.reader.reload().unwrap();
        }
        assert_eq!(handle2.num_docs(), 20);

        // Router state restored
        let (_, total) = handle2.router_stats().unwrap();
        assert_eq!(total, 20);

        // Search still works after reopen
        let results2 = handle2.search(&query, 10, None).unwrap();
        assert!(!results2.is_empty());

        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn test_diag_rr_contains_search() {
        let dir = tmp_dir("lucivy_diag_rr");
        let config: SchemaConfig = serde_json::from_value(serde_json::json!({
            "fields": [
                {"name": "path", "type": "text", "stored": true},
                {"name": "content", "type": "text", "stored": true}
            ],
            "shards": 4,
            "balance_weight": 1.0
        })).unwrap();
        let handle = ShardedHandle::create(&dir, &config).unwrap();
        let path_f = handle.field("path").unwrap();
        let content_f = handle.field("content").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..100 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(path_f, &format!("file_{i}.rs"));
            doc.add_text(content_f, &format!("function test_{i}() {{ println!(\"hello\"); }}"));
            handle.add_document(doc, i).unwrap();
        }

        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 100);

        let query: QueryConfig = serde_json::from_str(
            r#"{"type": "contains", "field": "content", "value": "function"}"#
        ).unwrap();
        let results = handle.search(&query, 20, None).unwrap();
        eprintln!("diag_rr: {} results for 'function'", results.len());
        assert!(results.len() > 0, "should find docs with 'function'");
    }

    #[test]
    fn test_search_filtered_prefilter() {
        let dir = tmp_dir("lucivy_sharded_prefilter");
        let config = make_config(2);
        let handle = ShardedHandle::create(&dir, &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        // Insert 20 docs, all containing "rust". node_ids 0..20.
        for i in 0u64..20 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("rust programming doc {i}"));
            handle.add_document(doc, i).unwrap();
        }
        handle.commit().unwrap();

        let query: QueryConfig = serde_json::from_value(serde_json::json!({
            "type": "contains", "field": "body", "value": "rust"
        })).unwrap();

        // Unfiltered: should find all 20.
        let all = handle.search(&query, 100, None).unwrap();
        assert_eq!(all.len(), 20);

        // Filtered: only allow node_ids {3, 7, 15}.
        let allowed: HashSet<u64> = [3, 7, 15].into_iter().collect();
        let filtered = handle.search_filtered(&query, 100, None, allowed.clone()).unwrap();

        assert_eq!(filtered.len(), 3, "should return exactly 3 results, got {}", filtered.len());

        // Verify all returned results have node_ids in the allowed set.
        for r in &filtered {
            let shard = handle.shard(r.shard_id).unwrap();
            let searcher = shard.reader.searcher();
            let seg_reader = searcher.segment_reader(r.doc_address.segment_ord);
            let node_col = seg_reader.fast_fields().u64(NODE_ID_FIELD).unwrap();
            let node_id = node_col.first(r.doc_address.doc_id).unwrap();
            assert!(
                allowed.contains(&node_id),
                "result node_id {} not in allowed set {:?}", node_id, allowed
            );
        }
    }

    #[test]
    fn test_ram_shard_storage() {
        let config = make_config(1);
        let storage = RamShardStorage::new();
        let handle = ShardedHandle::create_with_storage(Box::new(storage), &config).unwrap();

        let body = handle.field("body").unwrap();
        let nid = handle.field(NODE_ID_FIELD).unwrap();

        for i in 0u64..5 {
            let mut doc = LucivyDocument::new();
            doc.add_u64(nid, i);
            doc.add_text(body, &format!("rust programming {i}"));
            handle.add_document(doc, i).unwrap();
        }
        handle.commit().unwrap();
        assert_eq!(handle.num_docs(), 5);

        let query: QueryConfig = serde_json::from_value(serde_json::json!({
            "type": "contains", "field": "body", "value": "rust"
        })).unwrap();
        let results = handle.search(&query, 10, None).unwrap();
        assert_eq!(results.len(), 5);
    }
}
