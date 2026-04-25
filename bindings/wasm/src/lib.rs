//! lucivy-wasm — WASM bindings for ld-lucivy BM25 full-text search.
//!
//! Runs in a Web Worker with OPFS persistence.
//! Uses ShardedHandle with RamShardStorage (in-memory for WASM).
//! OPFS sync happens at the JS boundary via import_file / export_dirty.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::directory::Directory;
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::snapshot;
use lucivy_core::sharded_handle::{RamShardStorage, ShardStorage, ShardedHandle, ShardedSearchResult};

use wasm_bindgen::prelude::*;

// ── Index ──────────────────────────────────────────────────────────────────

#[wasm_bindgen]
pub struct Index {
    handle: ShardedHandle,
    text_fields: Vec<String>,
    index_path: String,
}

#[wasm_bindgen]
impl Index {
    /// Create a new index.
    /// `fields_json`: JSON array of field definitions.
    /// `shards`: number of shards (default 1).
    /// Returns an Index.
    #[wasm_bindgen(constructor)]
    pub fn create(path: &str, fields_json: &str, shards: Option<usize>) -> Result<Index, JsError> {
        let fields: Vec<query::FieldDef> = serde_json::from_str(fields_json)
            .map_err(|e| JsError::new(&format!("invalid fields JSON: {e}")))?;

        let config = query::SchemaConfig {
            fields,
            tokenizer: None,
            shards,
            ..Default::default()
        };

        let storage = Box::new(RamShardStorage::new());
        let handle = ShardedHandle::create_with_storage(storage, &config)
            .map_err(|e| JsError::new(&e))?;

        let text_fields = extract_text_fields(&config);

        Ok(Index {
            handle,
            text_fields,
            index_path: path.to_string(),
        })
    }

    /// Open an existing index from files previously read from OPFS.
    /// `files`: JS Map of filename → Uint8Array.
    ///
    /// File keys should be prefixed with shard directory, e.g.:
    /// - `_shard_config.json` (root file)
    /// - `shard_0/meta.json` (shard file)
    /// - `shard_0/some_segment.term` (shard file)
    ///
    /// Legacy non-sharded files (no `shard_` prefix, no `_shard_` prefix)
    /// are placed into `shard_0/` automatically.
    #[wasm_bindgen]
    pub fn open(path: &str, files: &js_sys::Map) -> Result<Index, JsError> {
        let storage = RamShardStorage::new();

        // Import all files from JS Map into RamShardStorage.
        let entries = files.entries();
        let mut has_shard_config = false;
        let mut legacy_files: Vec<(String, Vec<u8>)> = Vec::new();

        loop {
            let next = entries.next().map_err(|e| JsError::new(&format!("map iteration error: {e:?}")))?;
            if next.done() {
                break;
            }
            let pair = js_sys::Array::from(&next.value());
            let key: String = pair.get(0).as_string()
                .ok_or_else(|| JsError::new("file key must be a string"))?;
            let value = js_sys::Uint8Array::new(&pair.get(1));
            let data = value.to_vec();

            if key == "_shard_config.json" || key == "_shard_stats.bin" {
                // Root-level file.
                has_shard_config = has_shard_config || key == "_shard_config.json";
                storage.write_root_file(&key, &data)
                    .map_err(|e| JsError::new(&e))?;
            } else if key.starts_with("shard_") {
                // Sharded file: "shard_N/filename"
                if let Some(slash_pos) = key.find('/') {
                    let shard_prefix = &key[..slash_pos]; // "shard_N"
                    let filename = &key[slash_pos + 1..];
                    let shard_id: usize = shard_prefix.strip_prefix("shard_")
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| JsError::new(&format!("invalid shard prefix: {shard_prefix}")))?;
                    storage.import_shard_file(shard_id, filename, data);
                } else {
                    // Just "shard_N" with no slash — skip or treat as shard_0
                    legacy_files.push((key, data));
                }
            } else {
                // Legacy non-sharded file — collect for shard_0
                legacy_files.push((key, data));
            }
        }

        // If no shard config found, this is a legacy non-sharded index.
        // Import legacy files into shard_0 and synthesize a config.
        if !has_shard_config && !legacy_files.is_empty() {
            for (name, data) in &legacy_files {
                storage.import_shard_file(0, name, data.clone());
            }
            // Try to read _config.json from shard_0 to build _shard_config.json.
            if let Some((_, config_data)) = legacy_files.iter().find(|(n, _)| n == "_config.json") {
                let mut config: query::SchemaConfig = serde_json::from_slice(config_data)
                    .map_err(|e| JsError::new(&format!("cannot parse _config.json: {e}")))?;
                config.shards = Some(1);
                let config_json = serde_json::to_string(&config)
                    .map_err(|e| JsError::new(&format!("cannot serialize shard config: {e}")))?;
                storage.write_root_file("_shard_config.json", config_json.as_bytes())
                    .map_err(|e| JsError::new(&e))?;
            } else {
                return Err(JsError::new("no _shard_config.json or _config.json found in files"));
            }
        }

        let handle = ShardedHandle::open_with_storage(Box::new(storage))
            .map_err(|e| JsError::new(&e))?;

        let text_fields = extract_text_fields(&handle.config);

        Ok(Index {
            handle,
            text_fields,
            index_path: path.to_string(),
        })
    }

    // ── Document operations ────────────────────────────────────────────

    #[wasm_bindgen]
    pub fn add(&self, doc_id: u32, fields_json: &str) -> Result<(), JsError> {
        let fields: HashMap<String, serde_json::Value> = serde_json::from_str(fields_json)
            .map_err(|e| JsError::new(&format!("invalid fields JSON: {e}")))?;

        let mut doc = LucivyDocument::new();
        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| JsError::new("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id as u64);

        for (key, value) in &fields {
            self.add_field_value(&mut doc, key, value)?;
        }

        self.handle.add_document(doc, doc_id as u64)
            .map_err(|e| JsError::new(&e))?;
        Ok(())
    }

    #[wasm_bindgen(js_name = "addMany")]
    pub fn add_many(&self, docs_json: &str) -> Result<(), JsError> {
        let docs: Vec<serde_json::Value> = serde_json::from_str(docs_json)
            .map_err(|e| JsError::new(&format!("invalid docs JSON: {e}")))?;

        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| JsError::new("no _node_id field in schema"))?;

        for item in &docs {
            let obj = item.as_object()
                .ok_or_else(|| JsError::new("each doc must be an object"))?;
            let doc_id = obj.get("docId").or_else(|| obj.get("doc_id"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| JsError::new("each doc must have a 'docId' key"))?;

            let mut doc = LucivyDocument::new();
            doc.add_u64(nid_field, doc_id);
            for (key, value) in obj {
                if key == "docId" || key == "doc_id" { continue; }
                self.add_field_value(&mut doc, key, value)?;
            }
            self.handle.add_document(doc, doc_id)
                .map_err(|e| JsError::new(&e))?;
        }
        Ok(())
    }

    #[wasm_bindgen]
    pub fn remove(&self, doc_id: u32) -> Result<(), JsError> {
        self.handle.delete_by_node_id(doc_id as u64)
            .map_err(|e| JsError::new(&e))
    }

    #[wasm_bindgen]
    pub fn update(&self, doc_id: u32, fields_json: &str) -> Result<(), JsError> {
        self.remove(doc_id)?;
        self.add(doc_id, fields_json)?;
        Ok(())
    }

    // ── Transaction ────────────────────────────────────────────────────

    /// Commit changes.
    /// After this, call `exportDirtyFiles()` to get files to persist to OPFS.
    #[wasm_bindgen]
    pub fn commit(&self) -> Result<(), JsError> {
        self.handle.commit()
            .map_err(|e| JsError::new(&e))
    }

    /// Close the index (flush + release writer locks).
    #[wasm_bindgen]
    pub fn close(&self) -> Result<(), JsError> {
        self.handle.close()
            .map_err(|e| JsError::new(&e))
    }

    // ── OPFS sync ──────────────────────────────────────────────────────

    /// Export ALL shard files (for OPFS sync).
    /// Returns: `[[path, Uint8Array], ...]`
    ///
    /// Paths are prefixed with shard directory, e.g.:
    /// - `_shard_config.json`
    /// - `shard_0/meta.json`
    /// - `shard_0/some_segment.term`
    #[wasm_bindgen(js_name = "exportAllFiles")]
    pub fn export_all_files(&self) -> Result<JsValue, JsError> {
        let files = self.collect_all_shard_files()?;
        let array = js_sys::Array::new();
        for (path, data) in files {
            let pair = js_sys::Array::new();
            pair.push(&JsValue::from_str(&path));
            pair.push(&js_sys::Uint8Array::from(data.as_slice()).into());
            array.push(&pair);
        }
        Ok(array.into())
    }

    // ── Snapshot (LUCE format) ─────────────────────────────────────────

    /// Export the index as a LUCE snapshot blob.
    /// The index must have no uncommitted changes (call `commit()` first).
    /// Returns a `Uint8Array`.
    #[wasm_bindgen(js_name = "exportSnapshot")]
    pub fn export_snapshot(&self) -> Result<Vec<u8>, JsError> {
        // Collect shard files for snapshot.
        let num_shards = self.handle.num_shards();

        // Check all shards are committed.
        for i in 0..num_shards {
            let shard = self.handle.shard(i)
                .ok_or_else(|| JsError::new(&format!("shard {i} not found")))?;
            snapshot::check_committed(shard, &format!("shard_{i}"))
                .map_err(|e| JsError::new(&e))?;
        }

        // Collect root files.
        let mut root_files = Vec::new();
        let config_json = serde_json::to_string(&self.handle.config)
            .map_err(|e| JsError::new(&format!("cannot serialize config: {e}")))?;
        root_files.push(("_shard_config.json".to_string(), config_json.into_bytes()));

        // Collect per-shard files.
        let shard_paths: Vec<String> = (0..num_shards)
            .map(|i| format!("shard_{i}"))
            .collect();
        let mut shard_data: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
        for i in 0..num_shards {
            shard_data.push(self.collect_shard_files(i)?);
        }
        let shard_indexes: Vec<snapshot::SnapshotIndex<'_>> = shard_paths.iter()
            .zip(shard_data.iter())
            .map(|(path, files)| snapshot::SnapshotIndex {
                path: path.as_str(),
                files: files.clone(),
            })
            .collect();

        Ok(snapshot::export_snapshot_sharded(&shard_indexes, &root_files))
    }

    /// Import a LUCE snapshot blob and return a new Index.
    /// `data`: the LUCE blob as `Uint8Array`.
    /// `path`: the logical path for the imported index.
    #[wasm_bindgen(js_name = "importSnapshot")]
    pub fn import_snapshot(data: &[u8], path: &str) -> Result<Index, JsError> {
        let imported = snapshot::import_snapshot(data)
            .map_err(|e| JsError::new(&e))?;

        let storage = RamShardStorage::new();

        if imported.is_sharded {
            // Sharded snapshot: import root files + per-shard files.
            for (name, file_data) in &imported.root_files {
                storage.write_root_file(name, file_data)
                    .map_err(|e| JsError::new(&e))?;
            }
            for index in &imported.indexes {
                // Parse shard_id from path (e.g. "shard_0").
                let shard_id: usize = index.path.strip_prefix("shard_")
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| JsError::new(&format!("invalid shard path: {}", index.path)))?;
                for (name, file_data) in &index.files {
                    storage.import_shard_file(shard_id, name, file_data.clone());
                }
            }
        } else {
            // Non-sharded snapshot: import into shard_0.
            if imported.indexes.is_empty() {
                return Err(JsError::new("snapshot contains no indexes"));
            }
            let first = &imported.indexes[0];
            for (name, file_data) in &first.files {
                storage.import_shard_file(0, name, file_data.clone());
            }
            // Build _shard_config.json from _config.json.
            if let Some((_, config_data)) = first.files.iter().find(|(n, _)| n == "_config.json") {
                let mut config: query::SchemaConfig = serde_json::from_slice(config_data)
                    .map_err(|e| JsError::new(&format!("cannot parse _config.json: {e}")))?;
                config.shards = Some(1);
                let config_json = serde_json::to_string(&config)
                    .map_err(|e| JsError::new(&format!("cannot serialize shard config: {e}")))?;
                storage.write_root_file("_shard_config.json", config_json.as_bytes())
                    .map_err(|e| JsError::new(&e))?;
            } else {
                return Err(JsError::new("no _config.json in snapshot"));
            }
        }

        let handle = ShardedHandle::open_with_storage(Box::new(storage))
            .map_err(|e| JsError::new(&e))?;

        let text_fields = extract_text_fields(&handle.config);

        Ok(Index {
            handle,
            text_fields,
            index_path: path.to_string(),
        })
    }

    // ── Search ─────────────────────────────────────────────────────────

    /// Search the index.
    /// `query_json`: a JSON string (contains_split on all text fields) or a query object.
    /// Returns JSON: `[{docId, score, highlights?}, ...]`
    #[wasm_bindgen]
    pub fn search(&self, query_json: &str, limit: u32, highlights: Option<bool>) -> Result<String, JsError> {
        let query_config = self.parse_query(query_json)?;
        let want_highlights = highlights.unwrap_or(false);

        let highlight_sink = if want_highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        let results = self.handle.search(&query_config, limit as usize, highlight_sink.clone())
            .map_err(|e| JsError::new(&e))?;

        let json_results = collect_sharded_results(&self.handle, &results, highlight_sink.as_deref())?;
        serde_json::to_string(&json_results)
            .map_err(|e| JsError::new(&format!("serialize error: {e}")))
    }

    /// Search with allowed_ids filter.
    /// `allowed_ids`: JS array of u32 doc IDs.
    #[wasm_bindgen(js_name = "searchFiltered")]
    pub fn search_filtered(
        &self,
        query_json: &str,
        limit: u32,
        allowed_ids: &[u32],
        highlights: Option<bool>,
    ) -> Result<String, JsError> {
        let query_config = self.parse_query(query_json)?;
        let want_highlights = highlights.unwrap_or(false);

        let highlight_sink = if want_highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        let id_set: HashSet<u64> = allowed_ids.iter().map(|&id| id as u64).collect();
        let results = self.handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
            .map_err(|e| JsError::new(&e))?;

        let json_results = collect_sharded_results(&self.handle, &results, highlight_sink.as_deref())?;
        serde_json::to_string(&json_results)
            .map_err(|e| JsError::new(&format!("serialize error: {e}")))
    }

    // ── Info ───────────────────────────────────────────────────────────

    #[wasm_bindgen(js_name = "numDocs", getter)]
    pub fn num_docs(&self) -> u32 {
        self.handle.num_docs() as u32
    }

    #[wasm_bindgen(js_name = "numShards", getter)]
    pub fn num_shards(&self) -> usize {
        self.handle.num_shards()
    }

    #[wasm_bindgen(getter)]
    pub fn path(&self) -> String {
        self.index_path.clone()
    }

    #[wasm_bindgen(js_name = "schemaJson", getter)]
    pub fn schema_json(&self) -> String {
        serde_json::to_string(&self.handle.config).unwrap_or_default()
    }
}

// ── Private helpers ────────────────────────────────────────────────────────

const EXCLUDED_FILES: &[&str] = &[".lock", ".tantivy-writer.lock", ".lucivy-writer.lock", ".managed.json"];

impl Index {
    /// Collect all files from a single shard (excluding lock files).
    fn collect_shard_files(&self, shard_id: usize) -> Result<Vec<(String, Vec<u8>)>, JsError> {
        let shard = self.handle.shard(shard_id)
            .ok_or_else(|| JsError::new(&format!("shard {shard_id} not found")))?;
        let directory = shard.index.directory();
        let managed = directory.list_managed_files();
        let mut files = Vec::new();
        for path in managed {
            let name = path.to_string_lossy().to_string();
            if EXCLUDED_FILES.contains(&name.as_str()) {
                continue;
            }
            if let Ok(data) = directory.atomic_read(&path) {
                files.push((name, data));
            }
        }
        // Also include meta.json and _config.json (atomic files not in managed list).
        for extra in &["meta.json", "_config.json"] {
            let p = std::path::Path::new(extra);
            if !files.iter().any(|(n, _)| n == *extra) {
                if let Ok(data) = directory.atomic_read(p) {
                    files.push((extra.to_string(), data));
                }
            }
        }
        Ok(files)
    }

    /// Collect all files from all shards (prefixed with shard directory).
    fn collect_all_shard_files(&self) -> Result<Vec<(String, Vec<u8>)>, JsError> {
        let num_shards = self.handle.num_shards();
        let mut all_files = Vec::new();

        // Root config.
        let config_json = serde_json::to_string(&self.handle.config)
            .map_err(|e| JsError::new(&format!("cannot serialize config: {e}")))?;
        all_files.push(("_shard_config.json".to_string(), config_json.into_bytes()));

        // Per-shard files.
        for i in 0..num_shards {
            let shard_files = self.collect_shard_files(i)?;
            for (name, data) in shard_files {
                all_files.push((format!("shard_{i}/{name}"), data));
            }
        }
        Ok(all_files)
    }

    fn add_field_value(
        &self,
        doc: &mut LucivyDocument,
        field_name: &str,
        value: &serde_json::Value,
    ) -> Result<(), JsError> {
        let field = self.handle.field(field_name)
            .ok_or_else(|| JsError::new(&format!("unknown field: {field_name}")))?;
        let field_entry = self.handle.schema.get_field_entry(field);

        match field_entry.field_type() {
            FieldType::Str(_) => {
                let text = value.as_str()
                    .ok_or_else(|| JsError::new(&format!("expected string for {field_name}")))?;
                doc.add_text(field, text);
            }
            FieldType::U64(_) => {
                let v = value.as_u64()
                    .ok_or_else(|| JsError::new(&format!("expected u64 for {field_name}")))?;
                doc.add_u64(field, v);
            }
            FieldType::I64(_) => {
                let v = value.as_i64()
                    .ok_or_else(|| JsError::new(&format!("expected i64 for {field_name}")))?;
                doc.add_i64(field, v);
            }
            FieldType::F64(_) => {
                let v = value.as_f64()
                    .ok_or_else(|| JsError::new(&format!("expected f64 for {field_name}")))?;
                doc.add_f64(field, v);
            }
            _ => return Err(JsError::new(&format!("unsupported field type for {field_name}"))),
        }
        Ok(())
    }

    fn parse_query(&self, query_json: &str) -> Result<query::QueryConfig, JsError> {
        let value: serde_json::Value = serde_json::from_str(query_json)
            .map_err(|e| JsError::new(&format!("invalid query JSON: {e}")))?;

        match &value {
            serde_json::Value::String(s) => {
                if self.text_fields.is_empty() {
                    return Err(JsError::new("no text fields in schema for string query"));
                }
                Ok(build_contains_split_multi_field(s, &self.text_fields, None))
            }
            serde_json::Value::Object(_) => {
                let mut config: query::QueryConfig = serde_json::from_value(value)
                    .map_err(|e| JsError::new(&format!("invalid query object: {e}")))?;
                if config.query_type == "contains_split" {
                    config = expand_contains_split(&config);
                }
                Ok(config)
            }
            _ => Err(JsError::new("query must be a JSON string or object")),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn extract_text_fields(config: &query::SchemaConfig) -> Vec<String> {
    config.fields.iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect()
}

// ── Contains split helpers ─────────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String], distance: Option<u8>) -> query::QueryConfig {
    let words: Vec<&str> = value.split_whitespace().collect();
    if text_fields.len() == 1 {
        return expand_contains_split_for_field(value, &words, &text_fields[0], distance);
    }
    let word_queries: Vec<query::QueryConfig> = words.iter()
        .map(|word| {
            let field_queries: Vec<query::QueryConfig> = text_fields.iter()
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

fn expand_contains_split(config: &query::QueryConfig) -> query::QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let field = config.field.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().collect();
    expand_contains_split_for_field(value, &words, field, config.distance)
}

fn expand_contains_split_for_field(value: &str, words: &[&str], field: &str, distance: Option<u8>) -> query::QueryConfig {
    if words.len() <= 1 {
        return query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(value.to_string()),
            distance,
            ..Default::default()
        };
    }
    let should: Vec<query::QueryConfig> = words.iter()
        .map(|w| query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(w.to_string()),
            distance,
            ..Default::default()
        })
        .collect();
    query::QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
    }
}

// ── Search helpers ─────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct SearchResultJson {
    #[serde(rename = "docId")]
    doc_id: u32,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlights: Option<HashMap<String, Vec<[u32; 2]>>>,
}

fn collect_sharded_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
) -> Result<Vec<SearchResultJson>, JsError> {
    let nid_field = handle.schema.get_field(NODE_ID_FIELD)
        .map_err(|_| JsError::new("no _node_id field in schema"))?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| JsError::new(&format!("shard {} not found", r.shard_id)))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| JsError::new(&e.to_string()))?;

        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
            let map: HashMap<String, Vec<[u32; 2]>> = by_field.into_iter()
                .map(|(name, offsets)| {
                    let ranges: Vec<[u32; 2]> = offsets.into_iter()
                        .map(|[s, e]| [s as u32, e as u32])
                        .collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        out.push(SearchResultJson {
            doc_id: doc_id as u32,
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
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(3));
        }
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
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.distance, None);
        }
    }

    #[test]
    fn expand_contains_split_propagates_distance() {
        let config = query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some("body".into()),
            value: Some("hello world".into()),
            distance: Some(3),
            ..Default::default()
        };
        let q = expand_contains_split(&config);
        assert_eq!(q.query_type, "boolean");
        for sub in q.should.as_ref().unwrap() {
            assert_eq!(sub.query_type, "contains");
            assert_eq!(sub.distance, Some(3));
        }
    }
}
