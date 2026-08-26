//! lucivy-cpp — C++ bindings for ld-lucivy BM25 full-text search.
//!
//! Provides a CXX bridge for creating, managing, and querying Lucivy indexes.
//! Unified on ShardedHandle (even single-shard uses ShardedHandle with shards=1).
//! Distributed under the MIT License.
//!
//! API mirrors the Node.js and Python bindings:
//!   create/open/open_snapshot, add/add_many/delete/update, commit/close/drop_index,
//!   search, num_docs/path/schema, compact/wait_merges_quiet/index_bytes
//!
//! Plus, specific to C++: bring-your-own storage. A C++ subclass of
//! `lucivy::BlobBackend` (include/lucivy/blob_backend.h) becomes the
//! `BlobStore` behind a `BlobShardStorage`, via
//! `lucivy_create_with_blob_store` / `lucivy_open_with_blob_store`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use cxx::{CxxVector, UniquePtr};

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use lucistore::blob_store::BlobStore;
use lucivy_core::blob_directory::BlobLoadMode;
use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::sharded_handle::{BlobShardStorage, ShardedHandle, ShardedSearchResult};
use lucivy_core::snapshot;

// ── CXX bridge ─────────────────────────────────────────────────────────────

// The factories of the in-memory backend are called from C++ (and from
// this crate's tests), not from the library itself.
#[allow(dead_code)]
#[cxx::bridge(namespace = "lucivy")]
mod ffi {
    // ── Bring your own storage: C++ side ───────────────────────────────
    //
    // `BlobBackend` is the abstract class of include/lucivy/blob_backend.h.
    // Every method is const and may be called concurrently from lucivy's
    // scheduler threads. A C++ exception becomes the `Err` of the lucivy
    // call that triggered the I/O; `load`/`load_range` answering `false`
    // is "not found"/"unsupported", not an error.
    unsafe extern "C++" {
        include!("lucivy/blob_backend.h");
        include!("lucivy/mem_blob_backend.h");

        type BlobBackend;

        fn load(
            self: &BlobBackend,
            index_name: &str,
            file_name: &str,
            out: Pin<&mut CxxVector<u8>>,
        ) -> Result<bool>;
        fn save(self: &BlobBackend, index_name: &str, file_name: &str, data: &[u8]) -> Result<()>;
        fn remove(self: &BlobBackend, index_name: &str, file_name: &str) -> Result<()>;
        fn exists(self: &BlobBackend, index_name: &str, file_name: &str) -> Result<bool>;
        fn list(self: &BlobBackend, index_name: &str) -> Result<Vec<String>>;
        fn blob_len(
            self: &BlobBackend,
            index_name: &str,
            file_name: &str,
            out: &mut u64,
        ) -> Result<bool>;
        fn load_range(
            self: &BlobBackend,
            index_name: &str,
            file_name: &str,
            offset: u64,
            len: u64,
            out: Pin<&mut CxxVector<u8>>,
        ) -> Result<bool>;

        // In-memory reference backend (include/lucivy/mem_blob_backend.h):
        // a std::map behind a std::mutex, shareable between backends.
        type MemBlobMap;
        fn new_mem_blob_map() -> SharedPtr<MemBlobMap>;
        fn new_mem_blob_backend(map: SharedPtr<MemBlobMap>) -> UniquePtr<BlobBackend>;
    }

    struct SearchResult {
        doc_id: u64,
        score: f32,
    }

    struct HighlightRange {
        start: u32,
        end: u32,
    }

    struct FieldHighlights {
        field_name: String,
        ranges: Vec<HighlightRange>,
    }

    struct SearchResultWithHighlights {
        doc_id: u64,
        score: f32,
        highlights: Vec<FieldHighlights>,
    }

    struct FieldInfo {
        name: String,
        field_type: String,
    }

    struct ShardVersionInfo {
        shard_id: u32,
        version: String,
        segment_ids: Vec<String>,
    }

    extern "Rust" {
        type LucivyIndex;

        // ── Lifecycle ──────────────────────────────────────────────────

        // Create a new index at `path` with the given schema and shard count.
        // fields_json: JSON array of field definitions, e.g.
        //   [{"name":"body","type":"text","stored":true}, {"name":"score","type":"f64","fast":true}]
        // Supported types: "text" (full-text tokenized), "u64", "i64", "f64", "bool", "date".
        // shards: number of shards (1 = single-shard). More shards = faster search on large datasets.
        fn lucivy_create(path: &str, fields_json: &str, shards: u32) -> Result<Box<LucivyIndex>>;

        // Open an existing index at `path`. Reads persisted schema and segment metadata.
        // The index must have been previously created with lucivy_create().
        fn lucivy_open(path: &str) -> Result<Box<LucivyIndex>>;

        // Serve a LUCE snapshot straight from its bytes, without extracting it.
        // The blob is the index: nothing is written to disk and the memory cost
        // is the blob's own length. Read-only by construction: add/update/remove
        // are queued but commit() (and close(), which commits) fail; export_snapshot,
        // delta sync and drop_index are refused; get_path() is empty.
        // For a writable copy use lucivy_import_snapshot() instead.
        fn lucivy_open_snapshot(data: &[u8]) -> Result<Box<LucivyIndex>>;

        // Same as lucivy_open_snapshot(), reading the blob from a .luce file.
        fn lucivy_open_snapshot_from(path: &str) -> Result<Box<LucivyIndex>>;

        // ── Bring your own storage (ACID) ──────────────────────────────
        //
        // Create an index whose files live in `backend` (a C++ subclass of
        // lucivy::BlobBackend, see include/lucivy/blob_backend.h) under the
        // name `index_name`. The backend is the durable truth; `cache_dir`
        // only holds a disposable mmap cache of the blobs (empty string: a
        // fresh temporary directory, removed when the index is destroyed).
        // config_json: either the fields array of lucivy_create(), or a full
        // schema object {"fields":[...],"shards":2,"sfx_version":3,...}.
        // lazy: pull each blob on its first read instead of at open (needs
        // blob_len()/load_range() in the backend to pay off).
        // The backend must be thread-safe: its methods run concurrently on
        // lucivy's scheduler threads. Snapshot export and delta sync are
        // not available on a blob-backed index (its files are not on disk);
        // drop_index() deletes every blob of the index from the backend.
        fn lucivy_create_with_blob_store(
            backend: UniquePtr<BlobBackend>,
            index_name: &str,
            config_json: &str,
            cache_dir: &str,
            lazy: bool,
        ) -> Result<Box<LucivyIndex>>;

        // Open an index previously created with lucivy_create_with_blob_store()
        // from the blobs `backend` holds under `index_name`.
        fn lucivy_open_with_blob_store(
            backend: UniquePtr<BlobBackend>,
            index_name: &str,
            cache_dir: &str,
            lazy: bool,
        ) -> Result<Box<LucivyIndex>>;

        // ── Document operations ────────────────────────────────────────

        // Add a single document with the given _node_id and field values.
        // fields_json: JSON object with field names as keys, e.g. {"body": "text content", "score": 3.14}
        fn add(self: &LucivyIndex, doc_id: u64, fields_json: &str) -> Result<()>;

        // Add multiple documents at once. Each element must have a "doc_id" key.
        // docs_json: JSON array of objects, e.g. [{"doc_id": 1, "body": "hello"}, ...]
        fn add_many(self: &LucivyIndex, docs_json: &str) -> Result<()>;

        // Delete a document by its _node_id. The deletion is staged in memory until commit.
        fn remove(self: &LucivyIndex, doc_id: u64) -> Result<()>;

        // Update a document (delete old + re-add with new fields).
        // fields_json: same format as add().
        fn update(self: &LucivyIndex, doc_id: u64, fields_json: &str) -> Result<()>;

        // ── Transaction ────────────────────────────────────────────────

        // Commit pending changes to disk, making them visible to subsequent searches.
        // Lucivy uses lazy commit: searches auto-flush uncommitted changes before executing.
        // Call commit() explicitly to control the commit point.
        fn commit(self: &LucivyIndex) -> Result<()>;

        // NOT SUPPORTED. Always returns an error and discards nothing: the
        // sharded handle has no rollback, pending documents stay queued and
        // land at the next commit (searches auto-flush them). Kept for
        // signature compatibility with the 2.x header only.
        fn rollback(self: &LucivyIndex) -> Result<()>;

        // Flush any pending writes and release the writer lock.
        // The index data remains on disk and can be re-opened with lucivy_open().
        // No further mutations are allowed on this instance after close().
        fn close(self: &LucivyIndex) -> Result<()>;

        // Delete the whole index: close() it, then remove its directory
        // (every shard and the root files). Consumes the underlying handle:
        // afterwards every call on this instance fails with an error
        // (num_docs()/index_bytes() answer 0, get_schema() is empty).
        // Refused on a served snapshot (lucivy_open_snapshot).
        fn drop_index(self: &LucivyIndex) -> Result<()>;

        // ── Maintenance ────────────────────────────────────────────────

        // Commit, then merge the committed segments of every shard into
        // groups of at most `max_docs` documents (use SIZE_MAX for one
        // segment per shard). Blocks until the merges are done. Returns how
        // many merge rounds actually reduced a shard's segment count.
        fn compact(self: &LucivyIndex, max_docs: usize) -> Result<usize>;

        // Wait until no background merge is running on any shard (two
        // consecutive looks at a stable segment count). Call it before
        // anything that is about to claim memory, e.g. export_snapshot().
        // Returns how many rounds saw merge activity.
        fn wait_merges_quiet(self: &LucivyIndex) -> Result<usize>;

        // On-disk bytes of every searchable segment across all shards
        // (for a served snapshot: the live bytes of the blob). 0 after drop_index().
        fn index_bytes(self: &LucivyIndex) -> u64;

        // ── Search ─────────────────────────────────────────────────────
        // query_json: JSON string — either a plain string (auto contains_split across all text fields)
        //   or a query object. Query types:
        //   {"type":"contains","field":"body","value":"lock"}              — substring match
        //   {"type":"contains","field":"body","value":"lock","distance":1} — fuzzy substring (Levenshtein)
        //   {"type":"contains","field":"body","value":"a.*b","regex":true} — regex substring
        //   {"type":"startsWith","field":"body","value":"lock"}            — token prefix
        //   {"type":"contains_split","field":"body","value":"struct dev"}  — words OR'd as contains
        //   {"type":"term","field":"body","value":"lock"}                  — exact whole-token match
        //   {"type":"phrase","field":"body","value":"mutex lock"}          — adjacent tokens in order
        //   {"type":"regex","field":"body","pattern":"sched[a-z]+"}        — regex on individual tokens
        //   {"type":"boolean","must":[...],"should":[...],"must_not":[...]} — boolean combination
        //   {"type":"disjunction_max","queries":[...],"tie_breaker":0.1}   — best-score sub-queries
        //
        // Filtering (in query_json):
        //   "filters": [{"field":"category","op":"eq","value":"kernel"},
        //               {"field":"score","op":"gte","value":0.5}]
        //   Ops: eq, ne, lt, lte, gt, gte, in, not_in, between, starts_with, contains
        //   Composite: must, should, must_not with nested "clauses"

        // Honest warnings for a query, without running it: what the engine
        // will actually search and where it falls back to brute force.
        // Empty when nothing applies.
        fn query_warnings(self: &LucivyIndex, query_json: &str) -> Result<Vec<String>>;

        // Search without highlights. Returns top `limit` results sorted by BM25 score.
        fn search(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;

        // Search with highlight byte offsets. Each result includes per-field
        // HighlightRange pairs (start, end) marking matched substrings.
        fn search_with_highlights(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResultWithHighlights>>;

        // Search restricted to a whitelist of _node_id values (bitmap-based pre-filter).
        // Only documents whose _node_id appears in allowed_ids can match.
        fn search_filtered(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResult>>;

        // Search restricted to allowed_ids, with highlight byte offsets.
        fn search_filtered_with_highlights(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResultWithHighlights>>;

        // ── Info ───────────────────────────────────────────────────────

        // Total number of documents across all shards.
        fn num_docs(self: &LucivyIndex) -> u64;

        // Directory path where the index files are stored. For a blob-backed
        // index: the local cache directory (disposable, see above).
        fn get_path(self: &LucivyIndex) -> &str;

        // Full schema as a JSON string (includes internal fields).
        fn get_schema_json(self: &LucivyIndex) -> String;

        // Schema as a vector of FieldInfo (name + type), excluding internal fields.
        fn get_schema(self: &LucivyIndex) -> Vec<FieldInfo>;

        // ── Snapshot (LUCE format) ─────────────────────────────────────

        // Export the full index as a LUCE snapshot (all shards, schema, segments).
        // Returns raw bytes that can be stored or transferred.
        fn export_snapshot(self: &LucivyIndex) -> Result<Vec<u8>>;

        // Export the full index as a LUCE snapshot directly to a file.
        // path: destination file path (typically ending in .luce).
        fn export_snapshot_to(self: &LucivyIndex, path: &str) -> Result<()>;

        // Restore a full index from LUCE snapshot bytes (extracts every file).
        // data: raw snapshot bytes from export_snapshot(). dest_path: directory for restored files.
        // To serve the blob in place, read-only, see lucivy_open_snapshot().
        fn lucivy_import_snapshot(data: &[u8], dest_path: &str) -> Result<Box<LucivyIndex>>;

        // Restore a full index from a .luce snapshot file.
        // path: source .luce file. dest_path: directory for restored files.
        fn lucivy_import_snapshot_from(path: &str, dest_path: &str) -> Result<Box<LucivyIndex>>;

        // ── Delta sync (Tier 2) ────────────────────────────────────────

        // Per-shard version info. Returns {shard_id, version, segment_ids} per shard.
        // Pass to a remote server's export_sharded_delta() to get only changed segments.
        fn shard_versions(self: &LucivyIndex) -> Result<Vec<ShardVersionInfo>>;

        // Export a LUCIDS delta blob containing only segments changed since the client's versions.
        // client_versions_json: JSON array of [{shard_id, version, segment_ids}, ...].
        fn export_sharded_delta(self: &LucivyIndex, client_versions_json: &str) -> Result<Vec<u8>>;

        // Apply a LUCIDS delta blob to this index (merges changed segments).
        // data: raw delta bytes from export_sharded_delta().
        fn apply_sharded_delta(self: &LucivyIndex, data: &[u8]) -> Result<()>;

        // ── Distributed search (Tier 3) ────────────────────────────────

        // Export local BM25 statistics for a query (document frequencies, doc counts).
        // Returns JSON string of ExportableStats. Merge stats from all nodes, then use
        // search_with_global_stats() for consistent cross-node ranking.
        fn export_stats(self: &LucivyIndex, query_json: &str) -> Result<String>;

        // Search using externally-provided global BM25 stats for consistent cross-node ranking.
        // global_stats_json: JSON string of merged ExportableStats from all nodes.
        fn search_with_global_stats(
            self: &LucivyIndex,
            query_json: &str,
            global_stats_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;

        // The same, restricted to a whitelist of _node_id values: a real
        // pre-filter under the federation's statistics — the ids decide which
        // documents are visited, the statistics how they score.
        fn search_filtered_with_global_stats(
            self: &LucivyIndex,
            query_json: &str,
            global_stats_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResult>>;

        // Merge BM25 stats from multiple nodes into global stats (for distributed search).
        // stats_json_list: JSON array of ExportableStats strings, one per node.
        // Returns merged JSON string ready for search_with_global_stats().
        fn lucivy_merge_stats(stats_json_list: &[String]) -> Result<String>;
    }
}

#[cfg(test)]
mod test_bridge;

// ── C++ BlobBackend as a BlobStore ────────────────────────────────────────

/// A C++ `lucivy::BlobBackend` seen from Rust as a [`BlobStore`].
///
/// Error mapping: a C++ exception thrown by any method becomes an
/// `io::Error` (kind `Other`) carrying `what()`; `load` answering `false`
/// becomes `io::ErrorKind::NotFound`; `blob_len`/`load_range` answering
/// `false` become `Ok(None)`, i.e. "unknown"/"unsupported" for lazy loading.
struct CxxBlobStore {
    backend: UniquePtr<ffi::BlobBackend>,
}

// SAFETY: `UniquePtr<T>` is neither Send nor Sync for an opaque C++ `T`
// because cxx cannot know how the C++ class behaves. The BlobBackend
// contract (include/lucivy/blob_backend.h) requires the implementation to
// be thread-safe: every method is const and lucivy calls them concurrently
// from its scheduler threads (shard actors, merge threads). The only Rust
// access is through `&self`, forwarding to those const methods.
unsafe impl Send for CxxBlobStore {}
unsafe impl Sync for CxxBlobStore {}

impl CxxBlobStore {
    fn new(backend: UniquePtr<ffi::BlobBackend>) -> Result<Self, String> {
        if backend.is_null() {
            return Err("blob backend is null: pass a std::unique_ptr to a lucivy::BlobBackend subclass".into());
        }
        Ok(Self { backend })
    }
}

fn cxx_io_error(what: &str, index_name: &str, file_name: &str, e: cxx::Exception) -> io::Error {
    io::Error::other(format!("blob backend {what} {index_name}/{file_name}: {}", e.what()))
}

impl BlobStore for CxxBlobStore {
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>> {
        let mut out = CxxVector::<u8>::new();
        let found = self
            .backend
            .load(index_name, file_name, out.pin_mut())
            .map_err(|e| cxx_io_error("load", index_name, file_name, e))?;
        if !found {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{index_name}/{file_name} not found in blob backend"),
            ));
        }
        Ok(out.as_slice().to_vec())
    }

    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> {
        self.backend
            .save(index_name, file_name, data)
            .map_err(|e| cxx_io_error("save", index_name, file_name, e))
    }

    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()> {
        self.backend
            .remove(index_name, file_name)
            .map_err(|e| cxx_io_error("remove", index_name, file_name, e))
    }

    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool> {
        self.backend
            .exists(index_name, file_name)
            .map_err(|e| cxx_io_error("exists", index_name, file_name, e))
    }

    fn list(&self, index_name: &str) -> io::Result<Vec<String>> {
        self.backend
            .list(index_name)
            .map_err(|e| cxx_io_error("list", index_name, "", e))
    }

    fn blob_len(&self, index_name: &str, file_name: &str) -> io::Result<Option<u64>> {
        let mut len = 0u64;
        let known = self
            .backend
            .blob_len(index_name, file_name, &mut len)
            .map_err(|e| cxx_io_error("blob_len", index_name, file_name, e))?;
        Ok(known.then_some(len))
    }

    fn load_range(
        &self,
        index_name: &str,
        file_name: &str,
        range: std::ops::Range<u64>,
    ) -> io::Result<Option<Vec<u8>>> {
        let mut out = CxxVector::<u8>::new();
        let supported = self
            .backend
            .load_range(index_name, file_name, range.start, range.end - range.start, out.pin_mut())
            .map_err(|e| cxx_io_error("load_range", index_name, file_name, e))?;
        Ok(supported.then(|| out.as_slice().to_vec()))
    }
}

/// Where a blob-backed index keeps its mmap cache. `owned` when the caller
/// passed an empty `cache_dir` and the binding made a temporary one, to be
/// removed with the index.
struct BlobCache {
    path: PathBuf,
    owned: bool,
}

static CACHE_SEQ: AtomicU64 = AtomicU64::new(0);

impl BlobCache {
    fn resolve(cache_dir: &str) -> Result<Self, String> {
        if !cache_dir.is_empty() {
            std::fs::create_dir_all(cache_dir)
                .map_err(|e| format!("cannot create cache dir {cache_dir}: {e}"))?;
            return Ok(Self { path: PathBuf::from(cache_dir), owned: false });
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "lucivy_blob_cache_{}_{nanos}_{}",
            std::process::id(),
            CACHE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("cannot create cache dir {}: {e}", path.display()))?;
        Ok(Self { path, owned: true })
    }
}

fn blob_storage(
    backend: UniquePtr<ffi::BlobBackend>,
    index_name: &str,
    cache: &BlobCache,
    lazy: bool,
) -> Result<BlobShardStorage<CxxBlobStore>, String> {
    if index_name.is_empty() {
        return Err("index_name must not be empty".into());
    }
    let store = Arc::new(CxxBlobStore::new(backend)?);
    let mode = if lazy { BlobLoadMode::Lazy } else { BlobLoadMode::Eager };
    Ok(BlobShardStorage::new(store, index_name, cache.path.clone()).with_load_mode(mode))
}

/// `config_json` is either the fields array `lucivy_create` takes, or a full
/// `SchemaConfig` object (`{"fields":[...],"shards":2,...}`).
fn parse_schema_config(config_json: &str) -> Result<query::SchemaConfig, String> {
    let value: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|e| format!("invalid config JSON: {e}"))?;
    match value {
        serde_json::Value::Array(_) => {
            let fields: Vec<query::FieldDef> = serde_json::from_value(value)
                .map_err(|e| format!("invalid fields JSON: {e}"))?;
            Ok(query::SchemaConfig { fields, tokenizer: None, shards: None, ..Default::default() })
        }
        serde_json::Value::Object(_) => serde_json::from_value(value)
            .map_err(|e| format!("invalid schema config JSON: {e}")),
        _ => Err("config JSON must be a fields array or a schema object".into()),
    }
}

fn lucivy_create_with_blob_store(
    backend: UniquePtr<ffi::BlobBackend>,
    index_name: &str,
    config_json: &str,
    cache_dir: &str,
    lazy: bool,
) -> Result<Box<LucivyIndex>, String> {
    let config = parse_schema_config(config_json)?;
    let cache = BlobCache::resolve(cache_dir)?;
    let storage = blob_storage(backend, index_name, &cache, lazy)?;
    let handle = ShardedHandle::create_with_storage(Box::new(storage), &config)?;
    Ok(LucivyIndex::wrap_blob(handle, cache))
}

fn lucivy_open_with_blob_store(
    backend: UniquePtr<ffi::BlobBackend>,
    index_name: &str,
    cache_dir: &str,
    lazy: bool,
) -> Result<Box<LucivyIndex>, String> {
    let cache = BlobCache::resolve(cache_dir)?;
    let storage = blob_storage(backend, index_name, &cache, lazy)?;
    let handle = ShardedHandle::open_with_storage(Box::new(storage))?;
    Ok(LucivyIndex::wrap_blob(handle, cache))
}

// ── LucivyIndex wrapper ────────────────────────────────────────────────────

pub struct LucivyIndex {
    /// `None` once `drop_index()` has consumed the handle. Every call goes
    /// through `handle()`, which answers with a clear error from then on.
    handle: RwLock<Option<ShardedHandle>>,
    /// Empty for a served snapshot (`lucivy_open_snapshot`); the cache
    /// directory for a blob-backed index.
    index_path: String,
    text_fields: Vec<String>,
    /// Served from a LUCE blob: read-only, and no directory behind it.
    served_snapshot: bool,
    /// Set for an index whose files live in a C++ `BlobBackend`.
    blob_cache: Option<BlobCache>,
}

impl Drop for LucivyIndex {
    fn drop(&mut self) {
        let Some(cache) = self.blob_cache.take() else { return };
        if !cache.owned {
            return;
        }
        // The handle goes first: each shard's BlobDirectory removes its own
        // cache subdirectory when its last clone drops. Then the base we
        // made for it. Best effort: the cache is disposable by definition.
        if let Ok(slot) = self.handle.get_mut() {
            slot.take();
        }
        let _ = std::fs::remove_dir_all(&cache.path);
    }
}

const DROPPED: &str = "index was dropped (drop_index): this instance no longer holds a handle";

/// Read guard that dereferences straight to the handle. Only built by
/// `LucivyIndex::handle()`, which has already checked the `Option`.
struct HandleRef<'a>(RwLockReadGuard<'a, Option<ShardedHandle>>);

impl Deref for HandleRef<'_> {
    type Target = ShardedHandle;
    fn deref(&self) -> &ShardedHandle {
        self.0.as_ref().expect("HandleRef is only built over Some")
    }
}

impl LucivyIndex {
    fn wrap(handle: ShardedHandle, index_path: &str, served_snapshot: bool) -> Box<LucivyIndex> {
        let text_fields = extract_text_fields(&handle.config);
        Box::new(LucivyIndex {
            handle: RwLock::new(Some(handle)),
            index_path: index_path.to_string(),
            text_fields,
            served_snapshot,
            blob_cache: None,
        })
    }

    fn wrap_blob(handle: ShardedHandle, cache: BlobCache) -> Box<LucivyIndex> {
        let text_fields = extract_text_fields(&handle.config);
        Box::new(LucivyIndex {
            handle: RwLock::new(Some(handle)),
            index_path: cache.path.to_string_lossy().to_string(),
            text_fields,
            served_snapshot: false,
            blob_cache: Some(cache),
        })
    }

    fn handle(&self) -> Result<HandleRef<'_>, String> {
        let guard = self.handle.read().map_err(|_| "handle lock poisoned".to_string())?;
        if guard.is_none() {
            return Err(DROPPED.to_string());
        }
        Ok(HandleRef(guard))
    }

    /// Operations that need the index directory (snapshot export, delta
    /// sync, drop) have nothing to work on for a served snapshot.
    fn require_directory(&self, what: &str) -> Result<(), String> {
        if self.served_snapshot {
            Err(format!(
                "{what} is not available on a served snapshot (lucivy_open_snapshot): \
                 it is read-only and has no directory; keep the original blob, or \
                 lucivy_import_snapshot() it into a writable directory"
            ))
        } else {
            Ok(())
        }
    }

    /// Snapshot export and delta sync read the shard files from the index
    /// directory on disk. A blob-backed index has none: its files are in
    /// the backend, its cache directory has another layout.
    fn require_filesystem(&self, what: &str) -> Result<(), String> {
        self.require_directory(what)?;
        if self.blob_cache.is_some() {
            Err(format!(
                "{what} is not available on a blob-backed index (lucivy_*_with_blob_store): \
                 its files live in the BlobBackend, not in a directory; the blobs themselves \
                 are the durable copy"
            ))
        } else {
            Ok(())
        }
    }
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

fn lucivy_create(path: &str, fields_json: &str, shards: u32) -> Result<Box<LucivyIndex>, String> {
    let fields: Vec<query::FieldDef> = serde_json::from_str(fields_json)
        .map_err(|e| format!("invalid fields JSON: {e}"))?;

    let config = query::SchemaConfig {
        fields,
        tokenizer: None,
        shards: if shards > 1 { Some(shards as usize) } else { None },
        ..Default::default()
    };

    let handle = ShardedHandle::create(path, &config)?;
    Ok(LucivyIndex::wrap(handle, path, false))
}

fn lucivy_open(path: &str) -> Result<Box<LucivyIndex>, String> {
    let handle = ShardedHandle::open(path)?;
    Ok(LucivyIndex::wrap(handle, path, false))
}

fn lucivy_open_snapshot(data: &[u8]) -> Result<Box<LucivyIndex>, String> {
    let blob = ld_lucivy::directory::OwnedBytes::new(data.to_vec());
    let handle = ShardedHandle::open_snapshot(blob)?;
    Ok(LucivyIndex::wrap(handle, "", true))
}

fn lucivy_open_snapshot_from(path: &str) -> Result<Box<LucivyIndex>, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read snapshot {path}: {e}"))?;
    lucivy_open_snapshot(&data)
}

// ── Document operations ────────────────────────────────────────────────────

impl LucivyIndex {
    fn add(&self, doc_id: u64, fields_json: &str) -> Result<(), String> {
        let fields: HashMap<String, serde_json::Value> = serde_json::from_str(fields_json)
            .map_err(|e| format!("invalid fields JSON: {e}"))?;

        let handle = self.handle()?;
        let mut doc = LucivyDocument::new();

        let nid_field = handle
            .field(NODE_ID_FIELD)
            .ok_or("no _node_id field in schema")?;
        doc.add_u64(nid_field, doc_id);

        add_fields_from_map(&handle, &mut doc, &fields)?;

        handle.add_document(doc, doc_id)
    }

    fn add_many(&self, docs_json: &str) -> Result<(), String> {
        let docs: Vec<serde_json::Value> = serde_json::from_str(docs_json)
            .map_err(|e| format!("invalid docs JSON: {e}"))?;

        let handle = self.handle()?;
        let nid_field = handle
            .field(NODE_ID_FIELD)
            .ok_or("no _node_id field in schema")?;

        for item in &docs {
            let obj = item
                .as_object()
                .ok_or("each doc must be an object")?;

            let doc_id = obj
                .get("docId")
                .or_else(|| obj.get("doc_id"))
                .and_then(|v| v.as_u64())
                .ok_or("each doc must have a 'docId' (number) key")?;

            let mut doc = LucivyDocument::new();
            doc.add_u64(nid_field, doc_id);

            for (key, value) in obj {
                if key == "docId" || key == "doc_id" {
                    continue;
                }
                add_field_value(&handle, &mut doc, key, value)?;
            }

            handle.add_document(doc, doc_id)?;
        }
        Ok(())
    }

    fn remove(&self, doc_id: u64) -> Result<(), String> {
        self.handle()?.delete_by_node_id(doc_id)
    }

    fn update(&self, doc_id: u64, fields_json: &str) -> Result<(), String> {
        self.remove(doc_id)?;
        self.add(doc_id, fields_json)?;
        Ok(())
    }

    fn commit(&self) -> Result<(), String> {
        self.handle()?.commit()
    }

    /// Honest stub: the sharded handle has no rollback. Nothing is discarded;
    /// queued documents still land at the next commit or auto-flush.
    fn rollback(&self) -> Result<(), String> {
        self.handle()?;
        Err("rollback is not supported on ShardedHandle: nothing was discarded, \
             pending documents will land at the next commit"
            .to_string())
    }

    fn close(&self) -> Result<(), String> {
        self.handle()?.close()
    }

    fn drop_index(&self) -> Result<(), String> {
        self.require_directory("drop_index")?;
        // Take the handle out under the write lock so no reader can observe
        // it half-dropped; the lock is released before the (slow) close+remove.
        let handle = {
            let mut guard = self.handle.write().map_err(|_| "handle lock poisoned".to_string())?;
            guard.take().ok_or_else(|| DROPPED.to_string())?
        };
        handle.drop_index()
    }
}

// ── Maintenance ───────────────────────────────────────────────────────────

impl LucivyIndex {
    fn compact(&self, max_docs: usize) -> Result<usize, String> {
        self.handle()?.compact(max_docs)
    }

    fn wait_merges_quiet(&self) -> Result<usize, String> {
        self.handle()?.wait_merges_quiet()
    }

    fn index_bytes(&self) -> u64 {
        self.handle().map(|h| h.index_bytes()).unwrap_or(0)
    }
}

// ── Snapshot ────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn export_snapshot(&self) -> Result<Vec<u8>, String> {
        self.require_filesystem("export_snapshot")?;
        let handle = self.handle()?;
        snapshot::export_to_snapshot(&handle, std::path::Path::new(&self.index_path))
    }

    fn export_snapshot_to(&self, path: &str) -> Result<(), String> {
        let blob = self.export_snapshot()?;
        std::fs::write(path, &blob)
            .map_err(|e| format!("cannot write snapshot: {e}"))?;
        Ok(())
    }
}

fn lucivy_import_snapshot(data: &[u8], dest_path: &str) -> Result<Box<LucivyIndex>, String> {
    let dest = std::path::Path::new(dest_path);
    let handle = snapshot::import_from_snapshot(data, dest)?;
    Ok(LucivyIndex::wrap(handle, dest_path, false))
}

fn lucivy_import_snapshot_from(path: &str, dest_path: &str) -> Result<Box<LucivyIndex>, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read snapshot: {e}"))?;
    lucivy_import_snapshot(&data, dest_path)
}

// ── Delta sync (Tier 2) ──────────────────────────────────────────────────

impl LucivyIndex {
    fn shard_versions(&self) -> Result<Vec<ffi::ShardVersionInfo>, String> {
        let versions = self.handle()?.shard_versions()?;
        Ok(versions
            .into_iter()
            .map(|sv| ffi::ShardVersionInfo {
                shard_id: sv.shard_id as u32,
                version: sv.version,
                segment_ids: sv.segment_ids.into_iter().collect(),
            })
            .collect())
    }

    fn export_sharded_delta(&self, client_versions_json: &str) -> Result<Vec<u8>, String> {
        let raw: Vec<serde_json::Value> = serde_json::from_str(client_versions_json)
            .map_err(|e| format!("invalid client_versions JSON: {e}"))?;

        let versions: Vec<lucistore::delta_sharded::ShardVersion> = raw
            .into_iter()
            .map(|v| {
                let shard_id = v["shard_id"].as_u64().unwrap_or(0) as usize;
                let version = v["version"].as_str().unwrap_or("").to_string();
                let segment_ids: std::collections::HashSet<String> = v["segment_ids"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                lucistore::delta_sharded::ShardVersion {
                    shard_id,
                    version,
                    segment_ids,
                }
            })
            .collect();

        self.require_filesystem("export_sharded_delta")?;
        self.handle()?.export_sharded_delta(&self.index_path, &versions)
    }

    fn apply_sharded_delta(&self, data: &[u8]) -> Result<(), String> {
        self.require_filesystem("apply_sharded_delta")?;
        self.handle()?.apply_sharded_delta(&self.index_path, data)
    }
}

// ── Distributed search (Tier 3) ──────────────────────────────────────────

impl LucivyIndex {
    fn export_stats(&self, query_json: &str) -> Result<String, String> {
        let query_config = self.parse_query(query_json)?;
        let stats = self.handle()?.export_stats(&query_config)?;
        serde_json::to_string(&stats)
            .map_err(|e| format!("serialize stats: {e}"))
    }

    fn search_with_global_stats(
        &self,
        query_json: &str,
        global_stats_json: &str,
        limit: u32,
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let global_stats: lucivy_core::bm25_global::ExportableStats =
            serde_json::from_str(global_stats_json)
                .map_err(|e| format!("invalid global_stats JSON: {e}"))?;

        let handle = self.handle()?;
        let results = handle.search_with_global_stats(
            &query_config,
            limit as usize,
            &global_stats,
            None,
        )?;

        collect_results(&handle, &results)
    }

    fn search_filtered_with_global_stats(
        &self,
        query_json: &str,
        global_stats_json: &str,
        limit: u32,
        allowed_ids: &[u64],
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let global_stats: lucivy_core::bm25_global::ExportableStats =
            serde_json::from_str(global_stats_json)
                .map_err(|e| format!("invalid global_stats JSON: {e}"))?;

        let handle = self.handle()?;
        let results = handle.search_filtered_with_global_stats(
            &query_config,
            limit as usize,
            &global_stats,
            None,
            allowed_ids.iter().copied().collect(),
        )?;

        collect_results(&handle, &results)
    }
}

// ── Merge stats (free-standing) ───────────────────────────────────────────

fn lucivy_merge_stats(stats_json_list: &[String]) -> Result<String, String> {
    let parsed: Vec<lucivy_core::bm25_global::ExportableStats> = stats_json_list
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid stats JSON: {e}"))?;
    let merged = lucivy_core::bm25_global::ExportableStats::merge(&parsed);
    serde_json::to_string(&merged)
        .map_err(|e| format!("serialize merged stats: {e}"))
}

// ── Search ─────────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn query_warnings(&self, query_json: &str) -> Result<Vec<String>, String> {
        let query_config = self.parse_query(query_json)?;
        Ok(self.handle()?.query_warnings(&query_config))
    }

    fn search(
        &self,
        query_json: &str,
        limit: u32,
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let handle = self.handle()?;
        let results = handle.search(&query_config, limit as usize, None)?;
        collect_results(&handle, &results)
    }

    fn search_with_highlights(
        &self,
        query_json: &str,
        limit: u32,
    ) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
        let query_config = self.parse_query(query_json)?;
        let highlight_sink = Arc::new(HighlightSink::new());

        let handle = self.handle()?;
        let results = handle.search(&query_config, limit as usize, Some(highlight_sink.clone()))?;
        collect_results_with_highlights(&handle, &results, Some(&highlight_sink))
    }

    fn search_filtered(
        &self,
        query_json: &str,
        limit: u32,
        allowed_ids: &[u64],
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
        let handle = self.handle()?;
        let results = handle.search_filtered(&query_config, limit as usize, None, id_set)?;
        collect_results(&handle, &results)
    }

    fn search_filtered_with_highlights(
        &self,
        query_json: &str,
        limit: u32,
        allowed_ids: &[u64],
    ) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
        let query_config = self.parse_query(query_json)?;
        let highlight_sink = Arc::new(HighlightSink::new());

        let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
        let handle = self.handle()?;
        let results = handle.search_filtered(&query_config, limit as usize, Some(highlight_sink.clone()), id_set)?;
        collect_results_with_highlights(&handle, &results, Some(&highlight_sink))
    }
}

// ── Info ───────────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn num_docs(&self) -> u64 {
        self.handle().map(|h| h.num_docs()).unwrap_or(0)
    }

    fn get_path(&self) -> &str {
        &self.index_path
    }

    fn get_schema_json(&self) -> String {
        self.handle()
            .map(|h| serde_json::to_string(&h.config).unwrap_or_default())
            .unwrap_or_default()
    }

    fn get_schema(&self) -> Vec<ffi::FieldInfo> {
        let Ok(handle) = self.handle() else {
            return Vec::new();
        };
        handle
            .field_map
            .iter()
            .filter(|(name, _)| {
                name != NODE_ID_FIELD
            })
            .map(|(name, field)| {
                let ft = match handle.schema.get_field_entry(*field).field_type() {
                    FieldType::Str(_) => "text",
                    FieldType::U64(_) => "u64",
                    FieldType::I64(_) => "i64",
                    FieldType::F64(_) => "f64",
                    _ => "unknown",
                };
                ffi::FieldInfo {
                    name: name.clone(),
                    field_type: ft.to_string(),
                }
            })
            .collect()
    }
}

// ── Query parsing ─────────────────────────────────────────────────────────

impl LucivyIndex {
    fn parse_query(&self, query_json: &str) -> Result<query::QueryConfig, String> {
        let value: serde_json::Value = serde_json::from_str(query_json)
            .map_err(|e| format!("invalid query JSON: {e}"))?;

        match &value {
            serde_json::Value::String(s) => {
                if self.text_fields.is_empty() {
                    return Err("no text fields in schema for string query".into());
                }
                Ok(build_contains_split_multi_field(s, &self.text_fields, None))
            }
            serde_json::Value::Object(_) => {
                let config: query::QueryConfig = serde_json::from_value(value)
                    .map_err(|e| format!("invalid query object: {e}"))?;
                Ok(config)
            }
            _ => Err("query must be a JSON string or object".into()),
        }
    }
}

// ── Contains split helpers ────────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String], distance: Option<u8>) -> query::QueryConfig {
    if text_fields.len() == 1 {
        return query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some(text_fields[0].clone()),
            value: Some(value.to_string()),
            distance,
            ..Default::default()
        };
    }

    let words: Vec<&str> = value.split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .collect();

    let word_queries: Vec<query::QueryConfig> = words
        .iter()
        .map(|word| {
            let field_queries: Vec<query::QueryConfig> = text_fields
                .iter()
                .map(|f| query::QueryConfig {
                    query_type: "contains".into(),
                    field: Some(f.clone()),
                    value: Some(word.to_string()),
                    distance,
                    ..Default::default()
                })
                .collect();
            query::QueryConfig {
                query_type: "boolean".into(),
                should: Some(field_queries),
                ..Default::default()
            }
        })
        .collect();

    if word_queries.len() == 1 {
        word_queries.into_iter().next().unwrap()
    } else {
        query::QueryConfig {
            query_type: "boolean".into(),
            should: Some(word_queries),
            ..Default::default()
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn extract_text_fields(config: &query::SchemaConfig) -> Vec<String> {
    config
        .fields
        .iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect()
}

fn add_fields_from_map(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    fields: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    for (key, value) in fields {
        add_field_value(handle, doc, key, value)?;
    }
    Ok(())
}

fn add_field_value(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    field_name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let field = handle
        .field(field_name)
        .ok_or_else(|| format!("unknown field: {field_name}"))?;
    let field_entry = handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("expected string for field {field_name}"))?;
            doc.add_text(field, text);
        }
        FieldType::U64(_) => {
            let v = value
                .as_u64()
                .ok_or_else(|| format!("expected u64 for field {field_name}"))?;
            doc.add_u64(field, v);
        }
        FieldType::I64(_) => {
            let v = value
                .as_i64()
                .ok_or_else(|| format!("expected i64 for field {field_name}"))?;
            doc.add_i64(field, v);
        }
        FieldType::F64(_) => {
            let v = value
                .as_f64()
                .ok_or_else(|| format!("expected f64 for field {field_name}"))?;
            doc.add_f64(field, v);
        }
        _ => return Err(format!("unsupported field type for {field_name}")),
    }
    Ok(())
}

fn collect_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
) -> Result<Vec<ffi::SearchResult>, String> {
    let nid_field = handle.schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| "no _node_id field in schema")?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| format!("shard {} not found", r.shard_id))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| e.to_string())?;

        let doc_id = doc
            .get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);
        out.push(ffi::SearchResult { doc_id, score: r.score });
    }
    Ok(out)
}

fn collect_results_with_highlights(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let nid_field = handle.schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| "no _node_id field in schema")?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| format!("shard {} not found", r.shard_id))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| e.to_string())?;

        let doc_id = doc
            .get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink
            .and_then(|sink| {
                let seg_id = searcher
                    .segment_reader(r.doc_address.segment_ord)
                    .segment_id();
                let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
                let entries: Vec<ffi::FieldHighlights> = by_field
                    .into_iter()
                    .map(|(field_name, offsets)| ffi::FieldHighlights {
                        field_name,
                        ranges: offsets
                            .into_iter()
                            .map(|[s, e]| ffi::HighlightRange {
                                start: s as u32,
                                end: e as u32,
                            })
                            .collect(),
                    })
                    .collect();
                if entries.is_empty() {
                    None
                } else {
                    Some(entries)
                }
            })
            .unwrap_or_default();

        out.push(ffi::SearchResultWithHighlights {
            doc_id,
            score: r.score,
            highlights,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields_one() -> Vec<String> { vec!["content".into()] }
    fn fields_two() -> Vec<String> { vec!["title".into(), "body".into()] }

    #[test]
    fn build_contains_split_propagates_distance_single_field() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), Some(3));
        // Single field delegates to core via "contains_split" query type
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, Some(3));
    }

    #[test]
    fn build_contains_split_propagates_distance_multi_field() {
        let q = build_contains_split_multi_field("hello", &fields_two(), Some(2));
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(2));
        }
    }

    #[test]
    fn build_contains_split_none_distance_stays_none() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), None);
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.distance, None);
    }

    #[test]
    fn build_contains_split_single_field_delegates_to_core() {
        let q = build_contains_split_multi_field("hello world", &fields_one(), Some(3));
        assert_eq!(q.query_type, "contains_split");
        assert_eq!(q.field.as_deref(), Some("content"));
        assert_eq!(q.distance, Some(3));
    }

    // ── Bridge-level tests: go through the same functions the C++ side calls ──

    const SCHEMA: &str = r#"[{"name":"title","type":"text"},{"name":"body","type":"text"}]"#;
    const QUERY: &str = r#"{"type":"contains","field":"body","value":"mutex"}"#;

    /// Fresh, unique directory under the system temp dir; removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("lucivy_cpp_{name}_{}_{nanos}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
        fn path(&self) -> String {
            self.0.to_str().unwrap().to_string()
        }
        fn join(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `n` documents in `commits` separate commits, so the shard holds
    /// several segments (one per commit) for compact() to work on.
    fn populate(idx: &LucivyIndex, n: u64, commits: u64) {
        for i in 0..n {
            let body = if i % 2 == 0 {
                format!("pthread_mutex_lock acquires the mutex number {i}")
            } else {
                format!("plain document number {i} about nothing")
            };
            idx.add(i, &serde_json::json!({"title": format!("doc {i}"), "body": body}).to_string())
                .unwrap();
            if commits > 1 && (i + 1) % (n / commits) == 0 {
                idx.commit().unwrap();
            }
        }
        idx.commit().unwrap();
    }

    fn segments(idx: &LucivyIndex, shard: usize) -> usize {
        let h = idx.handle().unwrap();
        h.shard(shard).unwrap().index.searchable_segment_metas().unwrap().len()
    }

    #[test]
    fn compact_merges_committed_segments() {
        let dir = TempDir::new("compact");
        let idx = lucivy_create(&dir.path(), SCHEMA, 1).unwrap();
        populate(&idx, 30, 3);
        assert!(segments(&idx, 0) >= 2, "several commits should leave several segments");

        let merges = idx.compact(usize::MAX).unwrap();
        assert!(merges >= 1, "compact should have merged at least once, got {merges}");
        assert_eq!(segments(&idx, 0), 1, "SIZE_MAX means one segment per shard");
        assert_eq!(idx.num_docs(), 30);
        let hits = idx.search(QUERY, 100).unwrap();
        assert_eq!(hits.len(), 15, "compaction must not lose documents");

        // Nothing left to do: a second pass is a no-op.
        assert_eq!(idx.compact(usize::MAX).unwrap(), 0);
        idx.close().unwrap();
    }

    #[test]
    fn wait_merges_quiet_returns_on_idle_index() {
        let dir = TempDir::new("quiet");
        let idx = lucivy_create(&dir.path(), SCHEMA, 2).unwrap();
        populate(&idx, 20, 2);
        let rounds = idx.wait_merges_quiet().unwrap();
        // Bounded by the 60-round cap per shard; an idle index is quiet fast.
        assert!(rounds < 120, "unexpected merge activity: {rounds} rounds");
        // And it is callable again without side effects.
        idx.wait_merges_quiet().unwrap();
        assert_eq!(idx.num_docs(), 20);
        idx.close().unwrap();
    }

    #[test]
    fn index_bytes_counts_committed_segments() {
        let dir = TempDir::new("bytes");
        let idx = lucivy_create(&dir.path(), SCHEMA, 1).unwrap();
        assert_eq!(idx.index_bytes(), 0, "empty index has no segment");
        populate(&idx, 10, 1);
        let bytes = idx.index_bytes();
        assert!(bytes > 0, "committed segments occupy bytes");
        populate(&idx, 10, 1);
        assert!(idx.index_bytes() > bytes, "more documents, more bytes");
        idx.close().unwrap();
    }

    #[test]
    fn open_snapshot_answers_like_its_source_and_is_read_only() {
        let dir = TempDir::new("served");
        let src = lucivy_create(&dir.path(), SCHEMA, 2).unwrap();
        populate(&src, 40, 2);
        let expected = src.search(QUERY, 100).unwrap();
        assert_eq!(expected.len(), 20);
        let expected_hl = src.search_with_highlights(QUERY, 5).unwrap();
        let blob = src.export_snapshot().unwrap();
        src.close().unwrap();

        // ── From bytes ──
        let served = lucivy_open_snapshot(&blob).unwrap();
        assert_eq!(served.num_docs(), 40);
        assert_eq!(served.get_path(), "", "a served snapshot has no directory");
        assert_eq!(served.get_schema().len(), 2);
        assert!(served.index_bytes() > 0 && served.index_bytes() <= blob.len() as u64);

        let got = served.search(QUERY, 100).unwrap();
        let key = |r: &ffi::SearchResult| (r.doc_id, r.score.to_bits());
        assert_eq!(
            got.iter().map(key).collect::<Vec<_>>(),
            expected.iter().map(key).collect::<Vec<_>>(),
            "served snapshot must rank exactly like the index it came from"
        );
        let got_hl = served.search_with_highlights(QUERY, 5).unwrap();
        for (a, b) in expected_hl.iter().zip(got_hl.iter()) {
            assert_eq!(a.doc_id, b.doc_id);
            assert_eq!(a.highlights.len(), b.highlights.len());
        }
        assert!(served.query_warnings(QUERY).is_ok());
        let filtered = served.search_filtered(QUERY, 100, &[0, 2, 999]).unwrap();
        assert_eq!(filtered.len(), 2);

        // Read-only: the write is queued through the pipeline, the commit is
        // where it fails (as in the core test), and close() commits too.
        let _ = served.add(999_999, r#"{"title":"x","body":"mutex"}"#);
        assert!(served.commit().is_err(), "commit into a served snapshot must fail");
        let err = served.export_snapshot().unwrap_err();
        assert!(err.contains("served snapshot"), "{err}");
        assert!(served.export_sharded_delta("[]").unwrap_err().contains("served snapshot"));
        assert!(served.apply_sharded_delta(&[]).unwrap_err().contains("served snapshot"));
        assert!(served.drop_index().unwrap_err().contains("served snapshot"));
        assert_eq!(served.num_docs(), 40, "refused operations leave the snapshot intact");
        let _ = served.close();

        // ── From a file ──
        let file = dir.join("snap.luce");
        std::fs::write(&file, &blob).unwrap();
        let served2 = lucivy_open_snapshot_from(&file).unwrap();
        assert_eq!(served2.num_docs(), 40);
        assert_eq!(served2.search(QUERY, 100).unwrap().len(), 20);
        let _ = served2.close();

        assert!(lucivy_open_snapshot(b"not a snapshot").is_err());
        assert!(lucivy_open_snapshot_from(&dir.join("missing.luce")).is_err());
    }

    #[test]
    fn drop_index_removes_the_directory_and_disarms_the_instance() {
        let dir = TempDir::new("drop");
        let path = dir.path();
        let idx = lucivy_create(&path, SCHEMA, 2).unwrap();
        populate(&idx, 10, 1);
        assert!(std::path::Path::new(&path).join("shard_0").exists());

        idx.drop_index().unwrap();
        assert!(!std::path::Path::new(&path).exists(), "drop_index removes the index directory");

        // Every call is refused from now on, with the same clear error.
        let dropped = |r: Result<(), String>| {
            let e = r.unwrap_err();
            assert!(e.contains("dropped"), "unexpected error: {e}");
        };
        dropped(idx.add(1, r#"{"title":"a","body":"b"}"#));
        dropped(idx.add_many(r#"[{"doc_id":1,"title":"a","body":"b"}]"#));
        dropped(idx.remove(1));
        dropped(idx.commit());
        dropped(idx.close());
        dropped(idx.drop_index());
        dropped(idx.compact(usize::MAX).map(|_| ()));
        dropped(idx.wait_merges_quiet().map(|_| ()));
        dropped(idx.search(QUERY, 10).map(|_| ()));
        dropped(idx.search_with_highlights(QUERY, 10).map(|_| ()));
        dropped(idx.search_filtered(QUERY, 10, &[1]).map(|_| ()));
        dropped(idx.query_warnings(QUERY).map(|_| ()));
        dropped(idx.export_snapshot().map(|_| ()));
        dropped(idx.shard_versions().map(|_| ()));
        dropped(idx.export_stats(QUERY).map(|_| ()));
        dropped(idx.rollback());
        assert_eq!(idx.num_docs(), 0);
        assert_eq!(idx.index_bytes(), 0);
        assert!(idx.get_schema().is_empty());
        assert_eq!(idx.get_schema_json(), "");
        assert_eq!(idx.get_path(), path, "the path is still reported, for the caller's logs");

        // The directory is gone: reopening fails.
        assert!(lucivy_open(&path).is_err());
    }

    // ── Bring your own storage: through the C++ BlobBackend ──────────────

    use crate::test_bridge::ffi as probe_ffi;

    const BLOB_CONFIG: &str = r#"{"fields":[{"name":"title","type":"text"},{"name":"body","type":"text"}],"shards":2}"#;

    /// (doc_id, score bits), sorted: the documents of these tests all score
    /// the same, and the order of tied hits follows segment order, which a
    /// reopen or a lazy open may change.
    fn hits_key(hits: &[ffi::SearchResult]) -> Vec<(u64, u32)> {
        let mut keys: Vec<(u64, u32)> = hits.iter().map(|r| (r.doc_id, r.score.to_bits())).collect();
        keys.sort();
        keys
    }

    /// A direct view on the same map, to inspect what lucivy stored.
    fn store_view(map: &cxx::SharedPtr<ffi::MemBlobMap>) -> CxxBlobStore {
        CxxBlobStore::new(ffi::new_mem_blob_backend(map.clone())).unwrap()
    }

    /// Run `f` on a helper thread; fail the test instead of hanging forever.
    fn with_timeout<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs))
            .unwrap_or_else(|_| panic!("operation did not finish within {secs}s"))
    }

    #[test]
    fn blob_store_create_reopen_search_and_keep_writing() {
        let map = ffi::new_mem_blob_map();
        let view = store_view(&map);
        let expected;
        let expected_hl;
        let cache_path;
        {
            let idx = lucivy_create_with_blob_store(
                ffi::new_mem_blob_backend(map.clone()), "acid", BLOB_CONFIG, "", false,
            ).unwrap();
            cache_path = idx.get_path().to_string();
            assert!(std::path::Path::new(&cache_path).is_dir(), "empty cache_dir → a temp dir is made");
            populate(&idx, 40, 2);
            assert_eq!(idx.num_docs(), 40);
            expected = idx.search(QUERY, 100).unwrap();
            assert_eq!(expected.len(), 20);
            expected_hl = idx.search_with_highlights(QUERY, 5).unwrap();
            idx.close().unwrap();

            // What the backend holds: shards under the prefixed namespace,
            // root files under the bare name.
            assert!(!view.list("Lucivy_acid/shard_0").unwrap().is_empty());
            assert!(!view.list("Lucivy_acid/shard_1").unwrap().is_empty());
            assert!(view.exists("acid", "_shard_config.json").unwrap());
            assert!(view.exists("acid", "_shard_stats.bin").unwrap());
            assert!(view.exists("Lucivy_acid/shard_0", "meta.json").unwrap());
            assert!(!view.list("Lucivy_acid/shard_0").unwrap().iter().any(|f| f.ends_with(".lock")),
                "lock files are process-local and never stored");
        }
        assert!(!std::path::Path::new(&cache_path).exists(),
            "the temporary cache goes with the index; the blobs are the truth");

        // "Another process": a new backend over the same map, own cache dir.
        let dir = TempDir::new("blob_reopen");
        let idx = lucivy_open_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "acid", &dir.path(), false,
        ).unwrap();
        assert_eq!(idx.get_path(), dir.path(), "a caller-supplied cache dir is used as is");
        assert_eq!(idx.num_docs(), 40);
        assert_eq!(idx.get_schema().len(), 2);
        assert_eq!(hits_key(&idx.search(QUERY, 100).unwrap()), hits_key(&expected),
            "reopened from blobs: same hits, same scores");
        let got_hl = idx.search_with_highlights(QUERY, 5).unwrap();
        for (a, b) in expected_hl.iter().zip(got_hl.iter()) {
            assert_eq!(a.doc_id, b.doc_id);
            assert_eq!(a.highlights.len(), b.highlights.len());
        }
        assert_eq!(idx.search_filtered(QUERY, 100, &[0, 2, 999]).unwrap().len(), 2);

        // Still writable, and the write lands in the backend.
        idx.add(1000, r#"{"title":"late","body":"a late mutex"}"#).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search(QUERY, 100).unwrap().len(), 21);
        idx.close().unwrap();
        drop(idx);

        let idx = lucivy_open_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "acid", "", false,
        ).unwrap();
        assert_eq!(idx.num_docs(), 41);
        idx.close().unwrap();
    }

    #[test]
    fn blob_store_config_forms_and_bad_input() {
        // Fields array, like lucivy_create.
        let map = ffi::new_mem_blob_map();
        let idx = lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "arr", SCHEMA, "", false,
        ).unwrap();
        populate(&idx, 4, 1);
        assert_eq!(idx.num_docs(), 4);
        idx.close().unwrap();

        // Null backend, empty name, bad config: clear errors, nothing stored.
        let map2 = ffi::new_mem_blob_map();
        let err = lucivy_create_with_blob_store(UniquePtr::null(), "x", SCHEMA, "", false).err().expect("an error");
        assert!(err.contains("null"), "{err}");
        let err = lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map2.clone()), "", SCHEMA, "", false,
        ).err().expect("an error");
        assert!(err.contains("index_name"), "{err}");
        let err = lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map2.clone()), "x", r#"{"fields":[],"shard":2}"#, "", false,
        ).err().expect("an error");
        assert!(err.contains("schema config"), "{err}");
        assert!(lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map2.clone()), "x", "42", "", false,
        ).is_err());
        assert!(store_view(&map2).list("x").unwrap().is_empty());

        // Opening a name the backend has never seen.
        let err = lucivy_open_with_blob_store(
            ffi::new_mem_blob_backend(map2.clone()), "nowhere", "", false,
        ).err().expect("an error");
        assert!(err.contains("_shard_config.json"), "{err}");
    }

    #[test]
    fn blob_store_save_failure_surfaces_in_commit_without_hanging() {
        let map = ffi::new_mem_blob_map();
        let probe = probe_ffi::new_backend_probe();
        let idx = lucivy_create_with_blob_store(
            probe_ffi::new_probed_backend(map.clone(), probe.clone()), "faulty", BLOB_CONFIG, "", false,
        ).unwrap();
        populate(&idx, 10, 1);
        assert_eq!(idx.search(QUERY, 100).unwrap().len(), 5);

        probe_ffi::probe_set_fail_saves(&probe, true);
        idx.add(100, r#"{"title":"x","body":"mutex after the outage"}"#).unwrap();
        let idx = Arc::new(idx);
        let idx2 = Arc::clone(&idx);
        let err = with_timeout(60, move || idx2.commit()).err().expect("an error");
        // The message depends on which save fails first. meta.json (the
        // commit point) fails on the commit path and carries the exception
        // text; a segment file fails inside the background finalize, whose
        // error the engine replaces by a generic one (indexer_actor.rs,
        // IndexerFinalizeCompleteMsg keeps only `success`).
        assert!(
            err.contains("injected save failure") || err.contains("background finalize failed"),
            "unexpected error: {err}"
        );

        // Real behaviour, pinned: the documents of the failed commit are
        // lost (the segment that could not be persisted is discarded), the
        // committed state is untouched, and the index stays usable.
        assert_eq!(idx.search(QUERY, 100).unwrap().len(), 5);
        assert_eq!(idx.num_docs(), 10);
        probe_ffi::probe_set_fail_saves(&probe, false);
        let idx2 = Arc::clone(&idx);
        with_timeout(60, move || idx2.commit()).unwrap();
        assert_eq!(idx.num_docs(), 10, "a retry does not resurrect the lost document");
        idx.add(101, r#"{"title":"y","body":"mutex after recovery"}"#).unwrap();
        idx.commit().unwrap();
        assert_eq!(idx.search(QUERY, 100).unwrap().len(), 6);
        idx.close().unwrap();

        // And what the backend holds is a consistent index.
        let reopened = lucivy_open_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "faulty", "", false,
        ).unwrap();
        assert_eq!(reopened.num_docs(), 11);
        assert_eq!(reopened.search(QUERY, 100).unwrap().len(), 6);
        reopened.close().unwrap();

        // A backend that fails at creation time: the create itself errors.
        let probe2 = probe_ffi::new_backend_probe();
        probe_ffi::probe_set_fail_saves(&probe2, true);
        let err = lucivy_create_with_blob_store(
            probe_ffi::new_probed_backend(ffi::new_mem_blob_map(), probe2), "dead", BLOB_CONFIG, "", false,
        ).err().expect("an error");
        assert!(err.contains("injected save failure"), "{err}");
    }

    #[test]
    fn blob_store_lazy_open_pulls_on_demand() {
        let map = ffi::new_mem_blob_map();
        let probe = probe_ffi::new_backend_probe();
        let view = store_view(&map);

        // Build eagerly with bodies big enough that some blobs exceed the
        // 64 KiB remote-read window of the lazy handle.
        let expected;
        {
            let idx = lucivy_create_with_blob_store(
                probe_ffi::new_probed_backend(map.clone(), probe.clone()), "lazy", BLOB_CONFIG, "", false,
            ).unwrap();
            for i in 0..300u64 {
                let filler: String = (0..40).map(|k| format!("word{}_{} ", i, (k * 7919) % 1000)).collect();
                let body = if i % 2 == 0 {
                    format!("pthread_mutex_lock acquires the mutex number {i} {filler}")
                } else {
                    format!("plain document number {i} {filler}")
                };
                idx.add(i, &serde_json::json!({"title": format!("doc {i}"), "body": body}).to_string()).unwrap();
            }
            idx.commit().unwrap();
            expected = idx.search(QUERY, 1000).unwrap();
            assert_eq!(expected.len(), 150);
            idx.close().unwrap();
        }

        // Everything the backend holds for the index, with sizes.
        let mut sizes: Vec<(String, u64)> = Vec::new();
        for ns in ["Lucivy_lazy/shard_0", "Lucivy_lazy/shard_1"] {
            for f in view.list(ns).unwrap() {
                sizes.push((format!("{ns}/{f}"), view.blob_len(ns, &f).unwrap().unwrap()));
            }
        }
        let total: u64 = sizes.iter().map(|(_, n)| n).sum();
        let (largest, largest_len) = sizes.iter().max_by_key(|(_, n)| *n).cloned().unwrap();
        assert!(largest_len > 64 * 1024, "test needs a blob over 64 KiB, largest is {largest_len}");

        // Eager open: every blob is loaded whole at open, blob_len unused.
        probe_ffi::probe_reset(&probe);
        {
            let idx = lucivy_open_with_blob_store(
                probe_ffi::new_probed_backend(map.clone(), probe.clone()), "lazy", "", false,
            ).unwrap();
            for (key, _) in &sizes {
                assert!(probe_ffi::probe_loads(&probe, key) >= 1, "eager open must load {key}");
            }
            assert_eq!(probe_ffi::probe_range_loads(&probe), 0);
            idx.close().unwrap();
        }

        // Lazy open, backend answering blob_len/load_range.
        probe_ffi::probe_set_lazy(&probe, true);
        probe_ffi::probe_reset(&probe);
        let idx = lucivy_open_with_blob_store(
            probe_ffi::new_probed_backend(map.clone(), probe.clone()), "lazy", "", true,
        ).unwrap();
        let loaded_at_open = probe_ffi::probe_loaded_keys(&probe);
        let loaded_bytes: u64 = sizes.iter()
            .filter(|(k, _)| loaded_at_open.iter().any(|l| l == k))
            .map(|(_, n)| n)
            .sum();
        assert!(loaded_bytes < total / 2,
            "lazy open loaded {loaded_bytes} of {total} bytes whole ({loaded_at_open:?}) — not lazy");
        assert_eq!(probe_ffi::probe_loads(&probe, &largest), 0,
            "the largest blob ({largest}, {largest_len} bytes) must not be loaded whole at open");
        assert!(probe_ffi::probe_range_loads(&probe) > 0,
            "open probes headers/footers through load_range");

        // Same answers as the eager index; the search pulls what it touches.
        assert_eq!(idx.num_docs(), 300);
        assert_eq!(hits_key(&idx.search(QUERY, 1000).unwrap()), hits_key(&expected),
            "lazy: same hits and scores as eager");
        let loaded_after_search = probe_ffi::probe_loaded_keys(&probe);
        assert!(loaded_after_search.len() > loaded_at_open.len(),
            "a query materializes the structures it reads");
        idx.close().unwrap();
    }

    #[test]
    fn blob_store_drop_index_deletes_every_blob() {
        let map = ffi::new_mem_blob_map();
        let view = store_view(&map);
        let idx = lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "gone", BLOB_CONFIG, "", false,
        ).unwrap();
        populate(&idx, 10, 1);
        let cache_path = idx.get_path().to_string();
        assert!(!view.list("Lucivy_gone/shard_0").unwrap().is_empty());
        assert!(!view.list("gone").unwrap().is_empty());

        idx.drop_index().unwrap();
        for ns in ["Lucivy_gone/shard_0", "Lucivy_gone/shard_1", "gone"] {
            assert!(view.list(ns).unwrap().is_empty(), "{ns} still has blobs after drop_index");
        }
        // Disarmed like a filesystem index.
        let err = idx.search(QUERY, 10).err().expect("an error");
        assert!(err.contains("dropped"), "{err}");
        assert_eq!(idx.num_docs(), 0);
        drop(idx);
        assert!(!std::path::Path::new(&cache_path).exists(), "temporary cache removed");

        // Nothing to reopen.
        assert!(lucivy_open_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "gone", "", false,
        ).is_err());
    }

    #[test]
    fn blob_store_refuses_filesystem_only_operations() {
        let map = ffi::new_mem_blob_map();
        let idx = lucivy_create_with_blob_store(
            ffi::new_mem_blob_backend(map.clone()), "nofs", BLOB_CONFIG, "", false,
        ).unwrap();
        populate(&idx, 4, 1);
        for (what, r) in [
            ("export_snapshot", idx.export_snapshot().map(|_| ())),
            ("export_sharded_delta", idx.export_sharded_delta("[]").map(|_| ())),
            ("apply_sharded_delta", idx.apply_sharded_delta(&[])),
        ] {
            let err = r.err().expect("an error");
            assert!(err.contains("blob-backed") && err.contains(what), "{what}: {err}");
        }
        // The rest works as usual.
        assert!(idx.shard_versions().unwrap().len() == 2);
        assert!(idx.compact(usize::MAX).is_ok());
        assert!(idx.index_bytes() > 0);
        idx.close().unwrap();
    }

    #[test]
    fn rollback_is_an_honest_error() {
        let dir = TempDir::new("rollback");
        let idx = lucivy_create(&dir.path(), SCHEMA, 1).unwrap();
        idx.add(1, r#"{"title":"a","body":"mutex"}"#).unwrap();
        let err = idx.rollback().unwrap_err();
        assert!(err.contains("not supported"), "{err}");
        // Nothing was discarded: the document lands at the next commit.
        idx.commit().unwrap();
        assert_eq!(idx.num_docs(), 1);
        idx.close().unwrap();
    }
}
