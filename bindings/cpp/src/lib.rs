//! lucivy-cpp — C++ bindings for ld-lucivy BM25 full-text search.
//!
//! Provides a CXX bridge for creating, managing, and querying Lucivy indexes.
//! Unified on ShardedHandle (even single-shard uses ShardedHandle with shards=1).
//! Distributed under the MIT License.
//!
//! API mirrors the Node.js and Python bindings:
//!   create/open, add/add_many/delete/update, commit/close, search, num_docs/path/schema

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::sharded_handle::{ShardedHandle, ShardedSearchResult};
use lucivy_core::snapshot;

// ── CXX bridge ─────────────────────────────────────────────────────────────

#[cxx::bridge(namespace = "lucivy")]
mod ffi {
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

        // Lifecycle
        fn lucivy_create(path: &str, fields_json: &str) -> Result<Box<LucivyIndex>>;
        fn lucivy_open(path: &str) -> Result<Box<LucivyIndex>>;

        // Document operations
        fn add(self: &LucivyIndex, doc_id: u64, fields_json: &str) -> Result<()>;
        fn add_many(self: &LucivyIndex, docs_json: &str) -> Result<()>;
        fn remove(self: &LucivyIndex, doc_id: u64) -> Result<()>;
        fn update(self: &LucivyIndex, doc_id: u64, fields_json: &str) -> Result<()>;

        // Transaction
        fn commit(self: &LucivyIndex) -> Result<()>;
        fn rollback(self: &LucivyIndex) -> Result<()>;
        fn close(self: &LucivyIndex) -> Result<()>;

        // Search
        fn search(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;

        fn search_with_highlights(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResultWithHighlights>>;

        fn search_filtered(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResult>>;

        fn search_filtered_with_highlights(
            self: &LucivyIndex,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResultWithHighlights>>;

        // Info
        fn num_docs(self: &LucivyIndex) -> u64;
        fn get_path(self: &LucivyIndex) -> &str;
        fn get_schema_json(self: &LucivyIndex) -> String;
        fn get_schema(self: &LucivyIndex) -> Vec<FieldInfo>;

        // Snapshot (LUCE format)
        fn export_snapshot(self: &LucivyIndex) -> Result<Vec<u8>>;
        fn export_snapshot_to(self: &LucivyIndex, path: &str) -> Result<()>;
        fn lucivy_import_snapshot(data: &[u8], dest_path: &str) -> Result<Box<LucivyIndex>>;
        fn lucivy_import_snapshot_from(path: &str, dest_path: &str) -> Result<Box<LucivyIndex>>;

        // Delta sync (Tier 2)
        fn shard_versions(self: &LucivyIndex) -> Result<Vec<ShardVersionInfo>>;
        fn export_sharded_delta(self: &LucivyIndex, client_versions_json: &str) -> Result<Vec<u8>>;
        fn apply_sharded_delta(self: &LucivyIndex, data: &[u8]) -> Result<()>;

        // Distributed search (Tier 3)
        fn export_stats(self: &LucivyIndex, query_json: &str) -> Result<String>;
        fn search_with_global_stats(
            self: &LucivyIndex,
            query_json: &str,
            global_stats_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;
    }
}

// ── LucivyIndex wrapper ────────────────────────────────────────────────────

pub struct LucivyIndex {
    handle: ShardedHandle,
    index_path: String,
    text_fields: Vec<String>,
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

fn lucivy_create(path: &str, fields_json: &str) -> Result<Box<LucivyIndex>, String> {
    let fields: Vec<query::FieldDef> = serde_json::from_str(fields_json)
        .map_err(|e| format!("invalid fields JSON: {e}"))?;

    let config = query::SchemaConfig {
        fields,
        tokenizer: None,
        ..Default::default()
    };

    let handle = ShardedHandle::create(path, &config)?;
    let text_fields = extract_text_fields(&config);

    Ok(Box::new(LucivyIndex {
        handle,
        index_path: path.to_string(),
        text_fields,
    }))
}

fn lucivy_open(path: &str) -> Result<Box<LucivyIndex>, String> {
    let handle = ShardedHandle::open(path)?;
    let text_fields = extract_text_fields(&handle.config);

    Ok(Box::new(LucivyIndex {
        handle,
        index_path: path.to_string(),
        text_fields,
    }))
}

// ── Document operations ────────────────────────────────────────────────────

impl LucivyIndex {
    fn add(&self, doc_id: u64, fields_json: &str) -> Result<(), String> {
        let fields: HashMap<String, serde_json::Value> = serde_json::from_str(fields_json)
            .map_err(|e| format!("invalid fields JSON: {e}"))?;

        let mut doc = LucivyDocument::new();

        let nid_field = self
            .handle
            .field(NODE_ID_FIELD)
            .ok_or("no _node_id field in schema")?;
        doc.add_u64(nid_field, doc_id);

        add_fields_from_map(&self.handle, &mut doc, &fields)?;

        self.handle.add_document(doc, doc_id)
    }

    fn add_many(&self, docs_json: &str) -> Result<(), String> {
        let docs: Vec<serde_json::Value> = serde_json::from_str(docs_json)
            .map_err(|e| format!("invalid docs JSON: {e}"))?;

        let nid_field = self
            .handle
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
                add_field_value(&self.handle, &mut doc, key, value)?;
            }

            self.handle.add_document(doc, doc_id)?;
        }
        Ok(())
    }

    fn remove(&self, doc_id: u64) -> Result<(), String> {
        self.handle.delete_by_node_id(doc_id)
    }

    fn update(&self, doc_id: u64, fields_json: &str) -> Result<(), String> {
        self.remove(doc_id)?;
        self.add(doc_id, fields_json)?;
        Ok(())
    }

    fn commit(&self) -> Result<(), String> {
        self.handle.commit()
    }

    fn rollback(&self) -> Result<(), String> {
        Err("rollback is not supported on ShardedHandle".to_string())
    }

    fn close(&self) -> Result<(), String> {
        self.handle.close()
    }
}

// ── Snapshot ────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn export_snapshot(&self) -> Result<Vec<u8>, String> {
        snapshot::export_to_snapshot(
            &self.handle,
            std::path::Path::new(&self.index_path),
        )
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
    let text_fields = extract_text_fields(&handle.config);

    Ok(Box::new(LucivyIndex {
        handle,
        index_path: dest_path.to_string(),
        text_fields,
    }))
}

fn lucivy_import_snapshot_from(path: &str, dest_path: &str) -> Result<Box<LucivyIndex>, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read snapshot: {e}"))?;
    lucivy_import_snapshot(&data, dest_path)
}

// ── Delta sync (Tier 2) ──────────────────────────────────────────────────

impl LucivyIndex {
    fn shard_versions(&self) -> Result<Vec<ffi::ShardVersionInfo>, String> {
        let versions = self.handle.shard_versions()?;
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

        self.handle.export_sharded_delta(&self.index_path, &versions)
    }

    fn apply_sharded_delta(&self, data: &[u8]) -> Result<(), String> {
        self.handle.apply_sharded_delta(&self.index_path, data)
    }
}

// ── Distributed search (Tier 3) ──────────────────────────────────────────

impl LucivyIndex {
    fn export_stats(&self, query_json: &str) -> Result<String, String> {
        let query_config = self.parse_query(query_json)?;
        let stats = self.handle.export_stats(&query_config)?;
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

        let results = self.handle.search_with_global_stats(
            &query_config,
            limit as usize,
            &global_stats,
            None,
        )?;

        collect_results(&self.handle, &results)
    }
}

// ── Search ─────────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn search(
        &self,
        query_json: &str,
        limit: u32,
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let results = self.handle.search(&query_config, limit as usize, None)?;
        collect_results(&self.handle, &results)
    }

    fn search_with_highlights(
        &self,
        query_json: &str,
        limit: u32,
    ) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
        let query_config = self.parse_query(query_json)?;
        let highlight_sink = Arc::new(HighlightSink::new());

        let results = self.handle.search(&query_config, limit as usize, Some(highlight_sink.clone()))?;
        collect_results_with_highlights(&self.handle, &results, Some(&highlight_sink))
    }

    fn search_filtered(
        &self,
        query_json: &str,
        limit: u32,
        allowed_ids: &[u64],
    ) -> Result<Vec<ffi::SearchResult>, String> {
        let query_config = self.parse_query(query_json)?;
        let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
        let results = self.handle.search_filtered(&query_config, limit as usize, None, id_set)?;
        collect_results(&self.handle, &results)
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
        let results = self.handle.search_filtered(&query_config, limit as usize, Some(highlight_sink.clone()), id_set)?;
        collect_results_with_highlights(&self.handle, &results, Some(&highlight_sink))
    }
}

// ── Info ───────────────────────────────────────────────────────────────────

impl LucivyIndex {
    fn num_docs(&self) -> u64 {
        self.handle.num_docs()
    }

    fn get_path(&self) -> &str {
        &self.index_path
    }

    fn get_schema_json(&self) -> String {
        serde_json::to_string(&self.handle.config).unwrap_or_default()
    }

    fn get_schema(&self) -> Vec<ffi::FieldInfo> {
        self.handle
            .field_map
            .iter()
            .filter(|(name, _)| {
                name != NODE_ID_FIELD
            })
            .map(|(name, field)| {
                let ft = match self.handle.schema.get_field_entry(*field).field_type() {
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
}
