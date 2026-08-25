//! lucivy — Node.js bindings for ld-lucivy BM25 full-text search.
//!
//! Unified on ShardedHandle (even single-shard uses ShardedHandle with shards=1).
//! Distributed under the MIT License.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::snapshot;
use lucivy_core::sharded_handle::{ShardedHandle, ShardedSearchResult};

// ─── ShardVersion (Tier 2 — delta sync) ───────────────────────────────────

#[napi(object)]
#[derive(Clone)]
pub struct ShardVersion {
    pub shard_id: u32,
    pub version: String,
    pub segment_ids: Vec<String>,
}

// ─── SearchResult ──────────────────────────────────────────────────────────

#[napi(object)]
pub struct SearchResult {
    pub doc_id: u32,
    pub score: f64,
    pub highlights: Option<HashMap<String, Vec<Vec<u32>>>>,
    pub fields: Option<HashMap<String, String>>,
}

// ─── FieldDef (input) ──────────────────────────────────────────────────────

#[napi(object)]
#[derive(Clone)]
pub struct FieldDef {
    pub name: String,
    #[napi(ts_type = "'text' | 'string' | 'u64' | 'i64' | 'f64'")]
    pub r#type: String,
    pub stored: Option<bool>,
    pub indexed: Option<bool>,
    pub fast: Option<bool>,
}

// ─── SearchOptions ─────────────────────────────────────────────────────────

#[napi(object)]
pub struct SearchOptions {
    pub limit: Option<u32>,
    pub highlights: Option<bool>,
    pub allowed_ids: Option<Vec<u32>>,
    pub fields: Option<bool>,
}

// ─── Index ─────────────────────────────────────────────────────────────────

#[napi]
pub struct Index {
    /// `None` once `dropIndex()` consumed the handle: every later call errors.
    handle: Option<ShardedHandle>,
    /// Empty for a snapshot-backed index (`openSnapshot`): it has no directory.
    index_path: String,
    user_fields: Vec<(String, String)>,
    text_fields: Vec<String>,
}

impl Index {
    fn h(&self) -> Result<&ShardedHandle> {
        self.handle.as_ref().ok_or_else(|| Error::from_reason(
            "index was dropped with dropIndex(): no further calls allowed",
        ))
    }

    fn is_snapshot(&self) -> bool {
        self.index_path.is_empty()
    }

    /// The handle, for operations that write. A served snapshot buffers
    /// documents happily and only fails at commit — which a later search
    /// would trigger on its own — so writes are refused here, up front.
    fn writable(&self) -> Result<&ShardedHandle> {
        let handle = self.h()?;
        if self.is_snapshot() {
            return Err(Error::from_reason(
                "a snapshot opened with openSnapshot() is read-only: \
                 import it with Index.importSnapshot() to get a writable index",
            ));
        }
        Ok(handle)
    }

    /// The index directory, for the operations that read or write files
    /// next to the shards (snapshot export, delta sync).
    fn dir(&self) -> Result<&str> {
        if self.index_path.is_empty() {
            return Err(Error::from_reason(
                "a snapshot-backed index (openSnapshot) has no directory: \
                 import it with Index.importSnapshot() first",
            ));
        }
        Ok(&self.index_path)
    }
}

#[napi]
impl Index {
    /// Create a new index at the given path.
    ///
    /// @param path - Directory path for the index files.
    /// @param fields - Field definitions: `[{name: "body", type: "text", stored: true}]`.
    ///   Types: `"text"` (full-text), `"u64"`, `"i64"`, `"f64"`, `"bool"`, `"date"`.
    /// @param shards - Number of shards (default 1). More shards = faster search on large datasets.
    #[napi(factory)]
    pub fn create(path: String, fields: Vec<FieldDef>, shards: Option<u32>) -> Result<Self> {
        let field_defs: Vec<query::FieldDef> = fields
            .iter()
            .map(|f| query::FieldDef {
                name: f.name.clone(),
                field_type: f.r#type.clone(),
                stored: f.stored,
                indexed: f.indexed,
                fast: f.fast,
            })
            .collect();

        let config = query::SchemaConfig {
            fields: field_defs,
            tokenizer: None,
            shards: shards.map(|s| s as usize),
            ..Default::default()
        };

        let handle = ShardedHandle::create(&path, &config)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&config);

        Ok(Self {
            handle: Some(handle),
            index_path: path,
            user_fields,
            text_fields,
        })
    }

    /// Open an existing index at the given path.
    ///
    /// Reads the persisted schema and segment metadata from disk.
    /// The index must have been previously created with `Index.create()`.
    ///
    /// @param path - Directory path of the existing index (same path used in `create()`).
    /// @returns An `Index` ready for search, add, delete, etc.
    #[napi(factory)]
    pub fn open(path: String) -> Result<Self> {
        let handle = ShardedHandle::open(&path)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

        Ok(Self {
            handle: Some(handle),
            index_path: path,
            user_fields,
            text_fields,
        })
    }

    /// Add a document.
    ///
    /// @param docId - Unique document ID (_node_id).
    /// @param fields - Object with field names as keys: `{title: "Hello", body: "World", score: 3.14}`
    #[napi]
    pub fn add(&self, doc_id: u32, fields: HashMap<String, serde_json::Value>) -> Result<()> {
        let handle = self.writable()?;
        let mut doc = LucivyDocument::new();

        let nid_field = handle.field(NODE_ID_FIELD)
            .ok_or_else(|| Error::from_reason("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id as u64);

        add_fields_from_map(handle, &mut doc, &fields)?;

        handle.add_document(doc, doc_id as u64)
            .map_err(|e| Error::from_reason(e))
    }

    /// Add multiple documents at once.
    ///
    /// Each element must have a `docId` (or `doc_id`) key plus field values.
    ///
    /// @param docs - Array of objects: `[{docId: 1, title: "Hello"}, {docId: 2, title: "World"}]`.
    #[napi]
    pub fn add_many(&self, docs: Vec<HashMap<String, serde_json::Value>>) -> Result<()> {
        let handle = self.writable()?;
        let nid_field = handle.field(NODE_ID_FIELD)
            .ok_or_else(|| Error::from_reason("no _node_id field in schema"))?;

        for map in &docs {
            let doc_id = map.get("docId")
                .or_else(|| map.get("doc_id"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| Error::from_reason("each doc must have a 'docId' (number) key"))?;

            let mut doc = LucivyDocument::new();
            doc.add_u64(nid_field, doc_id);

            for (key, value) in map {
                if key == "docId" || key == "doc_id" {
                    continue;
                }
                add_field_value(handle, &mut doc, key, value)?;
            }

            handle.add_document(doc, doc_id)
                .map_err(|e| Error::from_reason(e))?;
        }
        Ok(())
    }

    /// Delete a document by its `_node_id`.
    ///
    /// The deletion is staged in memory. Call `commit()` or run a search
    /// (which auto-commits via lazy commit) to make it visible.
    ///
    /// @param docId - The `_node_id` of the document to delete.
    #[napi]
    pub fn delete(&self, doc_id: u32) -> Result<()> {
        self.writable()?.delete_by_node_id(doc_id as u64)
            .map_err(|e| Error::from_reason(e))
    }

    /// Update a document (delete old + re-add with new fields).
    ///
    /// @param docId - The `_node_id` of the document to update.
    /// @param fields - New field values: `{title: "Updated", body: "New content"}`.
    #[napi]
    pub fn update(&self, doc_id: u32, fields: HashMap<String, serde_json::Value>) -> Result<()> {
        self.delete(doc_id)?;
        self.add(doc_id, fields)?;
        Ok(())
    }

    /// Commit pending changes to disk, making them visible to subsequent searches.
    ///
    /// Lucivy uses lazy commit: if you search without calling `commit()`,
    /// uncommitted changes are auto-flushed before the search executes.
    /// Call `commit()` explicitly when you need to control the commit point
    /// (e.g., after a batch of adds/deletes).
    #[napi]
    pub fn commit(&self) -> Result<()> {
        self.writable()?.commit()
            .map_err(|e| Error::from_reason(e))
    }

    /// Flush any pending writes and release the writer lock.
    ///
    /// After `close()`, the index data remains on disk and can be re-opened
    /// with `Index.open()`. No further mutations are allowed on this instance.
    ///
    /// On a snapshot served with `openSnapshot()` there is nothing to flush
    /// and no lock to release: `close()` is a no-op.
    #[napi]
    pub fn close(&self) -> Result<()> {
        let handle = self.h()?;
        if self.is_snapshot() {
            return Ok(());
        }
        handle.close()
            .map_err(|e| Error::from_reason(e))
    }

    /// Search the index.
    ///
    /// @param query - String or query object.
    ///
    /// **String**: `"hello world"` — auto contains_split across all text fields.
    ///
    /// **Query object** (all substring queries are cross-token):
    /// - `{type: "contains", field: "body", value: "lock"}` — substring match
    /// - `{type: "contains", field: "body", value: "lock", distance: 1}` — fuzzy substring
    /// - `{type: "contains", field: "body", value: "lock.*init", regex: true}` — regex substring
    /// - `{type: "startsWith", field: "body", value: "lock"}` — token prefix
    /// - `{type: "contains_split", field: "body", value: "struct device"}` — words OR'd
    /// - `{type: "term", field: "body", value: "lock"}` — exact whole-token
    /// - `{type: "fuzzy", field: "body", value: "schdule", distance: 1}` — alias for contains+distance
    /// - `{type: "phrase", field: "body", value: "mutex lock"}` — adjacent tokens
    /// - `{type: "regex", field: "body", pattern: "sched[a-z]+"}` — regex on tokens
    /// - `{type: "boolean", must: [...], should: [...], must_not: [...]}` — boolean combo
    /// - `{type: "disjunction_max", queries: [...], tie_breaker: 0.1}` — best score
    /// - `{type: "more_like_this", field: "body", value: "sample text", min_doc_frequency: 1}` — similarity
    ///
    /// **Filtering:**
    /// - `allowedIds` in options: pre-filter by _node_id (fast, bitmap-based)
    /// - `filters` key in query: filter on non-text fields (AND'd with search):
    ///   ```json
    ///   {type: "contains", field: "body", value: "lock",
    ///    filters: [
    ///      {field: "category", op: "eq", value: "kernel"},
    ///      {field: "score", op: "gte", value: 0.5},
    ///      {field: "status", op: "in", value: ["active", "review"]}
    ///    ]}
    ///   ```
    ///   Ops: `eq`, `ne`, `lt`, `lte`, `gt`, `gte`, `in`, `not_in`, `between`, `starts_with`, `contains`.
    ///   Composite: `must`, `should`, `must_not` with nested `clauses`.
    ///
    /// Honest warnings for a query, without running it.
    ///
    /// Plain-text warnings describing what the engine will actually search
    /// and where it falls back to brute force: separators ignored in relaxed
    /// mode, fuzzy distance too loose for the query length, regex without a
    /// usable literal (full scan), segments written by the legacy indexer.
    /// Empty array when nothing applies.
    #[napi]
    pub fn query_warnings(&self, query: serde_json::Value) -> Result<Vec<String>> {
        let query_config = self.parse_query(&query)?;
        Ok(self.h()?.query_warnings(&query_config))
    }

    /// @param options - `{limit?: number, highlights?: boolean, fields?: boolean, allowedIds?: number[]}`
    #[napi]
    pub fn search(
        &self,
        query: serde_json::Value,
        options: Option<SearchOptions>,
    ) -> Result<Vec<SearchResult>> {
        let limit = options.as_ref().and_then(|o| o.limit).unwrap_or(10);
        let want_highlights = options.as_ref().and_then(|o| o.highlights).unwrap_or(false);
        let want_fields = options.as_ref().and_then(|o| o.fields).unwrap_or(false);
        let allowed_ids = options.as_ref().and_then(|o| o.allowed_ids.clone());

        let query_config = self.parse_query(&query)?;

        let highlight_sink = if want_highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        let results = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().map(|id| id as u64).collect();
                self.h()?.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
                    .map_err(|e| Error::from_reason(e))?
            }
            None => self.h()?.search(&query_config, limit as usize, highlight_sink.clone())
                .map_err(|e| Error::from_reason(e))?,
        };

        collect_sharded_results(
            self.h()?,
            &results,
            highlight_sink.as_deref(),
            want_fields,
        )
    }

    /// Number of documents in the index (getter, access as `index.numDocs`).
    ///
    /// @returns Total document count across all shards.
    #[napi(getter)]
    pub fn num_docs(&self) -> Result<u32> {
        Ok(self.h()?.num_docs() as u32)
    }

    /// Number of shards (getter, access as `index.numShards`).
    ///
    /// @returns Shard count (1 for single-shard indexes).
    #[napi(getter)]
    pub fn num_shards(&self) -> Result<u32> {
        Ok(self.h()?.num_shards() as u32)
    }

    /// Index directory path (getter, access as `index.path`).
    ///
    /// @returns The directory path where the index files are stored.
    #[napi(getter)]
    pub fn path(&self) -> &str {
        &self.index_path
    }

    /// Export this index as a LUCE snapshot.
    ///
    /// Returns the full index content (all shards, schema, segments) as a
    /// binary blob. Restore later with `Index.importSnapshot()`.
    ///
    /// @returns `Buffer` containing the LUCE snapshot bytes.
    #[napi]
    pub fn export_snapshot(&self) -> Result<Buffer> {
        let blob = snapshot::export_to_snapshot(
            self.h()?,
            std::path::Path::new(self.dir()?),
        ).map_err(|e| Error::from_reason(e))?;
        Ok(blob.into())
    }

    /// Export this index as a LUCE snapshot directly to a file.
    ///
    /// @param path - Destination file path (typically ending in `.luce`).
    #[napi]
    pub fn export_snapshot_to(&self, path: String) -> Result<()> {
        let blob = snapshot::export_to_snapshot(
            self.h()?,
            std::path::Path::new(self.dir()?),
        ).map_err(|e| Error::from_reason(e))?;
        std::fs::write(&path, &blob)
            .map_err(|e| Error::from_reason(format!("cannot write snapshot: {e}")))?;
        Ok(())
    }

    /// Import an index from a LUCE snapshot (Buffer).
    ///
    /// Restores a full index from a binary blob previously created by
    /// `exportSnapshot()`. The index files are written to `destPath`.
    ///
    /// @param data - Raw LUCE snapshot bytes (Buffer).
    /// @param destPath - Directory to write the restored index into.
    ///   Defaults to `"/tmp/lucivy_import"`.
    /// @returns A new `Index` instance ready for search.
    #[napi(factory)]
    pub fn import_snapshot(data: Buffer, dest_path: Option<String>) -> Result<Self> {
        let dest = dest_path.as_deref().unwrap_or("/tmp/lucivy_import");
        let dest_p = std::path::Path::new(dest);
        let handle = snapshot::import_from_snapshot(&data, dest_p)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

        Ok(Self {
            handle: Some(handle),
            index_path: dest.to_string(),
            user_fields,
            text_fields,
        })
    }

    /// Import an index from a LUCE snapshot file (.luce).
    ///
    /// Convenience wrapper that reads the file then calls `importSnapshot()`.
    ///
    /// @param path - Path to the `.luce` snapshot file.
    /// @param destPath - Directory to write the restored index into.
    ///   Defaults to `"/tmp/lucivy_import"`.
    /// @returns A new `Index` instance ready for search.
    #[napi(factory)]
    pub fn import_snapshot_from(path: String, dest_path: Option<String>) -> Result<Self> {
        let data = std::fs::read(&path)
            .map_err(|e| Error::from_reason(format!("cannot read snapshot: {e}")))?;
        Self::import_snapshot(data.into(), dest_path)
    }

    /// Serve a LUCE snapshot (Buffer) directly, without extracting it.
    ///
    /// The blob itself is the index: readers get slices of it, nothing is
    /// written to disk and the memory cost is the blob's own length. The
    /// result is **read-only** — `add()`, `delete()`, `commit()`, `compact()`
    /// and the delta/snapshot exports fail with a clear error. To get a
    /// writable index back, use `Index.importSnapshot()` instead.
    ///
    /// @param data - Raw LUCE snapshot bytes (Buffer), as produced by `exportSnapshot()`.
    /// @returns A read-only `Index` ready for search. Its `path` is `""`.
    #[napi(factory)]
    pub fn open_snapshot(data: Buffer) -> Result<Self> {
        let bytes = ld_lucivy::directory::OwnedBytes::new(data.to_vec());
        let handle = ShardedHandle::open_snapshot(bytes)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

        Ok(Self {
            handle: Some(handle),
            index_path: String::new(),
            user_fields,
            text_fields,
        })
    }

    /// Serve a LUCE snapshot file (.luce) directly, without extracting it.
    ///
    /// Convenience wrapper that reads the file then calls `openSnapshot()`.
    /// Same read-only semantics.
    ///
    /// @param path - Path to the `.luce` snapshot file.
    /// @returns A read-only `Index` ready for search.
    #[napi(factory)]
    pub fn open_snapshot_from(path: String) -> Result<Self> {
        let data = std::fs::read(&path)
            .map_err(|e| Error::from_reason(format!("cannot read snapshot: {e}")))?;
        Self::open_snapshot(data.into())
    }

    // ── Maintenance ────────────────────────────────────────────────────

    /// Merge every shard's segments into segments of at most `maxDocs`
    /// documents, then commit.
    ///
    /// Bulk loading leaves many small segments behind; one `compact()` after
    /// the load makes searches faster and the index smaller on disk. Not
    /// something to call on every commit.
    ///
    /// @param maxDocs - Upper bound on documents per merged segment (default 10000).
    /// @returns Number of merge rounds that actually reduced a shard's segment count.
    #[napi]
    pub fn compact(&self, max_docs: Option<u32>) -> Result<u32> {
        let max_docs = max_docs.unwrap_or(10_000) as usize;
        let merges = self.writable()?.compact(max_docs)
            .map_err(|e| Error::from_reason(e))?;
        Ok(merges as u32)
    }

    /// Block until no background merge is running or about to start.
    ///
    /// Segment merges run in the background after commits. Call this before
    /// anything that needs a stable set of files or the full address space —
    /// measuring `indexBytes()`, copying the directory, exporting a snapshot
    /// under memory pressure.
    ///
    /// @returns Number of rounds that still saw merge activity (0 = already quiet).
    #[napi]
    pub fn wait_merges_quiet(&self) -> Result<u32> {
        let rounds = self.h()?.wait_merges_quiet()
            .map_err(|e| Error::from_reason(e))?;
        Ok(rounds as u32)
    }

    /// On-disk bytes of every searchable segment of every shard.
    ///
    /// Sums the segment files; the number moves while merges run, so call
    /// `waitMergesQuiet()` first for a stable figure.
    ///
    /// @returns Total size in bytes (a `number`; exact below 2^53).
    #[napi]
    pub fn index_bytes(&self) -> Result<f64> {
        Ok(self.h()?.index_bytes() as f64)
    }

    /// Delete the whole index: commit and release everything (like `close()`),
    /// then remove the index files from disk.
    ///
    /// This consumes the underlying handle. After `dropIndex()` every other
    /// method on this instance throws; create or open a new `Index` instead.
    #[napi]
    pub fn drop_index(&mut self) -> Result<()> {
        let handle = self.handle.take().ok_or_else(|| Error::from_reason(
            "index was already dropped with dropIndex()",
        ))?;
        handle.drop_index().map_err(|e| Error::from_reason(e))
    }

    /// Schema as a list of field definitions (getter, access as `index.schema`).
    ///
    /// @returns `Array<{name: string, type: string}>` for each user-defined field.
    #[napi(getter)]
    pub fn schema(&self) -> Vec<FieldDef> {
        self.user_fields
            .iter()
            .map(|(name, ft)| FieldDef {
                name: name.clone(),
                r#type: ft.clone(),
                stored: None,
                indexed: None,
                fast: None,
            })
            .collect()
    }

    // ── Tier 2 — Delta sync ────────────────────────────────────────────

    /// Per-shard version info for delta sync (getter, access as `index.shardVersions`).
    ///
    /// Returns the current version and segment IDs for each shard.
    /// Pass this to a remote server's `exportShardedDelta()` to
    /// receive only the segments that changed since your last sync.
    ///
    /// @returns `Array<{shardId: number, version: string, segmentIds: string[]}>`.
    #[napi(getter)]
    pub fn shard_versions(&self) -> Result<Vec<ShardVersion>> {
        let versions = self.h()?.shard_versions()
            .map_err(|e| Error::from_reason(e))?;
        Ok(versions
            .iter()
            .map(|sv| ShardVersion {
                shard_id: sv.shard_id as u32,
                version: sv.version.clone(),
                segment_ids: sv.segment_ids.iter().cloned().collect(),
            })
            .collect())
    }

    /// Export a sharded delta (LUCIDS blob) containing only segments that
    /// changed since the client's known versions.
    ///
    /// Used for incremental sync: the client sends its `shardVersions`,
    /// the server computes and returns only the diff.
    ///
    /// @param clientVersions - Array of `{shardId, version, segmentIds}`,
    ///   typically obtained from the client's `shardVersions` getter.
    /// @returns `Buffer` containing the LUCIDS binary delta blob.
    #[napi]
    pub fn export_sharded_delta(&self, client_versions: Vec<ShardVersion>) -> Result<Buffer> {
        let versions: Vec<lucistore::delta_sharded::ShardVersion> = client_versions
            .iter()
            .map(|sv| lucistore::delta_sharded::ShardVersion {
                shard_id: sv.shard_id as usize,
                version: sv.version.clone(),
                segment_ids: sv.segment_ids.iter().cloned().collect(),
            })
            .collect();

        let blob = self.h()?.export_sharded_delta(self.dir()?, &versions)
            .map_err(|e| Error::from_reason(e))?;
        Ok(blob.into())
    }

    /// Apply a sharded delta (LUCIDS blob) to this index.
    ///
    /// Merges the delta's segments into the local index, bringing it
    /// up to date with the server. Only modified shards are touched.
    ///
    /// @param data - LUCIDS binary blob from `exportShardedDelta()`.
    #[napi]
    pub fn apply_sharded_delta(&self, data: Buffer) -> Result<()> {
        self.h()?.apply_sharded_delta(self.dir()?, &data)
            .map_err(|e| Error::from_reason(e))
    }

    // ── Tier 3 — Distributed search ────────────────────────────────────

    /// Export BM25 statistics for a query (for distributed search).
    ///
    /// In a distributed setup, each node exports its local BM25 stats.
    /// A coordinator merges them into global stats and sends them back
    /// for scoring with `searchWithGlobalStats()`.
    ///
    /// @param queryJson - JSON string of QueryConfig (same format as `search()` object).
    /// @returns JSON string of `ExportableStats` (document frequencies, doc counts).
    #[napi]
    pub fn export_stats(&self, query_json: String) -> Result<String> {
        let config: query::QueryConfig = serde_json::from_str(&query_json)
            .map_err(|e| Error::from_reason(format!("invalid query JSON: {e}")))?;
        let stats = self.h()?.export_stats(&config)
            .map_err(|e| Error::from_reason(e))?;
        serde_json::to_string(&stats)
            .map_err(|e| Error::from_reason(format!("serialize stats: {e}")))
    }

    /// Search using externally-provided global BM25 stats (distributed mode).
    ///
    /// Scores are computed using the merged global stats instead of local-only
    /// stats, ensuring consistent ranking across nodes.
    ///
    /// @param queryJson - JSON string of QueryConfig.
    /// @param globalStatsJson - JSON string of merged `ExportableStats`
    ///   from all nodes (obtained by merging `exportStats()` outputs).
    /// @param limit - Maximum number of results (default 10).
    /// @param highlights - If true, return highlight byte offsets per field.
    /// @returns `Array<SearchResult>` scored with global BM25 statistics.
    #[napi]
    pub fn search_with_global_stats(
        &self,
        query_json: String,
        global_stats_json: String,
        limit: Option<u32>,
        highlights: Option<bool>,
    ) -> Result<Vec<SearchResult>> {
        let query_config: query::QueryConfig = serde_json::from_str(&query_json)
            .map_err(|e| Error::from_reason(format!("invalid query JSON: {e}")))?;
        let global_stats: lucivy_core::bm25_global::ExportableStats =
            serde_json::from_str(&global_stats_json)
                .map_err(|e| Error::from_reason(format!("invalid stats JSON: {e}")))?;

        let limit = limit.unwrap_or(10) as usize;
        let want_highlights = highlights.unwrap_or(false);

        let highlight_sink = if want_highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        let results = self.h()?.search_with_global_stats(
            &query_config, limit, &global_stats, highlight_sink.clone(),
        ).map_err(|e| Error::from_reason(e))?;

        collect_sharded_results(
            self.h()?,
            &results,
            highlight_sink.as_deref(),
            false,
        )
    }
}

/// Merge BM25 stats from multiple nodes into global stats (for distributed search).
///
/// Each node calls `index.exportStats(queryJson)` which returns a JSON string.
/// The coordinator collects all JSON strings and merges them with this function.
/// The merged result is then passed back to each node via
/// `index.searchWithGlobalStats(queryJson, mergedJson)`.
///
/// @param statsList - Array of JSON strings, one per node (from `exportStats()`).
/// @returns JSON string of merged `ExportableStats` ready for `searchWithGlobalStats()`.
#[napi]
pub fn merge_stats(stats_list: Vec<String>) -> Result<String> {
    let parsed: Vec<lucivy_core::bm25_global::ExportableStats> = stats_list
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::from_reason(format!("invalid stats JSON: {e}")))?;
    let merged = lucivy_core::bm25_global::ExportableStats::merge(&parsed);
    serde_json::to_string(&merged)
        .map_err(|e| Error::from_reason(format!("serialize merged stats: {e}")))
}

// ─── Query parsing ─────────────────────────────────────────────────────────

impl Index {
    fn parse_query(&self, query: &serde_json::Value) -> Result<query::QueryConfig> {
        match query {
            serde_json::Value::String(s) => {
                if self.text_fields.is_empty() {
                    return Err(Error::from_reason(
                        "no text fields in schema for string query",
                    ));
                }
                Ok(build_contains_split_multi_field(s, &self.text_fields, None))
            }
            serde_json::Value::Object(_) => {
                let config: query::QueryConfig = serde_json::from_value(query.clone())
                    .map_err(|e| Error::from_reason(format!("invalid query object: {e}")))?;
                Ok(config)
            }
            _ => Err(Error::from_reason(
                "query must be a string or an object",
            )),
        }
    }
}

// ─── Contains split helpers ────────────────────────────────────────────────

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

// ─── Helpers ───────────────────────────────────────────────────────────────

fn extract_user_fields(config: &query::SchemaConfig) -> (Vec<(String, String)>, Vec<String>) {
    let user_fields: Vec<(String, String)> = config
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.field_type.clone()))
        .collect();
    let text_fields: Vec<String> = config
        .fields
        .iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect();
    (user_fields, text_fields)
}

fn add_fields_from_map(
    handle: &ShardedHandle,
    doc: &mut LucivyDocument,
    fields: &HashMap<String, serde_json::Value>,
) -> Result<()> {
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
) -> Result<()> {
    let field = handle
        .field(field_name)
        .ok_or_else(|| Error::from_reason(format!("unknown field: {field_name}")))?;
    let field_entry = handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text = value
                .as_str()
                .ok_or_else(|| Error::from_reason(format!("expected string for field {field_name}")))?;
            doc.add_text(field, text);
        }
        FieldType::U64(_) => {
            let v = value
                .as_u64()
                .ok_or_else(|| Error::from_reason(format!("expected u64 for field {field_name}")))?;
            doc.add_u64(field, v);
        }
        FieldType::I64(_) => {
            let v = value
                .as_i64()
                .ok_or_else(|| Error::from_reason(format!("expected i64 for field {field_name}")))?;
            doc.add_i64(field, v);
        }
        FieldType::F64(_) => {
            let v = value
                .as_f64()
                .ok_or_else(|| Error::from_reason(format!("expected f64 for field {field_name}")))?;
            doc.add_f64(field, v);
        }
        _ => {
            return Err(Error::from_reason(format!(
                "unsupported field type for {field_name}"
            )))
        }
    }
    Ok(())
}

fn collect_sharded_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> Result<Vec<SearchResult>> {
    let nid_field = handle.schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| Error::from_reason("no _node_id field in schema"))?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| Error::from_reason(format!("shard {} not found", r.shard_id)))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher
            .doc(r.doc_address)
            .map_err(|e| Error::from_reason(e.to_string()))?;

        let doc_id = doc
            .get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher
                .segment_reader(r.doc_address.segment_ord)
                .segment_id();
            let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
            let map: HashMap<String, Vec<Vec<u32>>> = by_field
                .into_iter()
                .map(|(name, offsets)| {
                    let ranges = offsets
                        .into_iter()
                        .map(|[s, e]| vec![s as u32, e as u32])
                        .collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() {
                None
            } else {
                Some(map)
            }
        });

        let fields = if include_fields {
            let mut map = HashMap::new();
            for (field, value) in doc.field_values() {
                let name = handle.schema.get_field_name(field);
                if name == NODE_ID_FIELD {
                    continue;
                }
                let rv = value.as_value();
                let val_str = if let Some(s) = rv.as_str() {
                    s.to_string()
                } else if let Some(n) = rv.as_u64() {
                    n.to_string()
                } else if let Some(n) = rv.as_i64() {
                    n.to_string()
                } else if let Some(n) = rv.as_f64() {
                    n.to_string()
                } else {
                    continue;
                };
                map.insert(name.to_string(), val_str);
            }
            if map.is_empty() { None } else { Some(map) }
        } else {
            None
        };

        out.push(SearchResult {
            doc_id: doc_id as u32,
            score: r.score as f64,
            highlights,
            fields,
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
}
