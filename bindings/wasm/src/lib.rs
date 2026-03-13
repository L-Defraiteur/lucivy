//! lucivy-wasm — WASM bindings for ld-lucivy BM25 full-text search.
//!
//! Runs in a Web Worker with OPFS persistence.
//! The Directory is in-memory (MemoryDirectory); OPFS sync happens
//! at the JS boundary via import_file / export_dirty.

mod directory;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::collector::{FilterCollector, TopDocs};
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::{DocAddress, LucivyDocument, Searcher};

use lucivy_core::handle::{LucivyHandle, NGRAM_SUFFIX, NODE_ID_FIELD, RAW_SUFFIX};
use lucivy_core::query;
use lucivy_core::snapshot;

use wasm_bindgen::prelude::*;

use crate::directory::MemoryDirectory;

// ── Index ──────────────────────────────────────────────────────────────────

#[wasm_bindgen]
pub struct Index {
    handle: LucivyHandle,
    text_fields: Vec<String>,
    directory: MemoryDirectory,
    index_path: String,
}

#[wasm_bindgen]
impl Index {
    /// Create a new index.
    /// `fields_json`: JSON array of field definitions.
    /// `stemmer`: language string ("english", "french", ...) or empty.
    /// Returns an Index. Caller should then call `export_all_files()` to persist to OPFS.
    #[wasm_bindgen(constructor)]
    pub fn create(path: &str, fields_json: &str, stemmer: &str) -> Result<Index, JsError> {
        let fields: Vec<query::FieldDef> = serde_json::from_str(fields_json)
            .map_err(|e| JsError::new(&format!("invalid fields JSON: {e}")))?;

        let config = query::SchemaConfig {
            fields,
            tokenizer: None,
            stemmer: if stemmer.is_empty() { None } else { Some(stemmer.to_string()) },
        };

        let directory = MemoryDirectory::new();
        let handle = LucivyHandle::create(directory.clone(), &config)
            .map_err(|e| JsError::new(&e))?;

        let text_fields = extract_text_fields(&config);

        Ok(Index {
            handle,
            text_fields,
            directory,
            index_path: path.to_string(),
        })
    }

    /// Open an existing index from files previously read from OPFS.
    /// `files`: JS Map of filename → Uint8Array.
    /// The JS side reads all files from OPFS and passes them here.
    #[wasm_bindgen]
    pub fn open(path: &str, files: &js_sys::Map) -> Result<Index, JsError> {
        let directory = MemoryDirectory::new();

        // Import all files from JS Map into MemoryDirectory.
        let entries = files.entries();
        loop {
            let next = entries.next().map_err(|e| JsError::new(&format!("map iteration error: {e:?}")))?;
            if next.done() {
                break;
            }
            let pair = js_sys::Array::from(&next.value());
            let key: String = pair.get(0).as_string()
                .ok_or_else(|| JsError::new("file key must be a string"))?;
            let value = js_sys::Uint8Array::new(&pair.get(1));
            directory.import_file(&key, value.to_vec());
        }

        let handle = LucivyHandle::open(directory.clone())
            .map_err(|e| JsError::new(&e))?;

        let text_fields = match &handle.config {
            Some(config) => extract_text_fields(config),
            None => Vec::new(),
        };

        Ok(Index {
            handle,
            text_fields,
            directory,
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

        let mut guard = self.handle.writer.lock()
            .map_err(|_| JsError::new("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| JsError::new("index is closed"))?;
        writer.add_document(doc)
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.handle.mark_uncommitted();
        Ok(())
    }

    #[wasm_bindgen(js_name = "addMany")]
    pub fn add_many(&self, docs_json: &str) -> Result<(), JsError> {
        let docs: Vec<serde_json::Value> = serde_json::from_str(docs_json)
            .map_err(|e| JsError::new(&format!("invalid docs JSON: {e}")))?;

        let mut guard = self.handle.writer.lock()
            .map_err(|_| JsError::new("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| JsError::new("index is closed"))?;
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
            writer.add_document(doc)
                .map_err(|e| JsError::new(&e.to_string()))?;
        }
        self.handle.mark_uncommitted();
        Ok(())
    }

    #[wasm_bindgen]
    pub fn remove(&self, doc_id: u32) -> Result<(), JsError> {
        let field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| JsError::new("no _node_id field in schema"))?;
        let term = ld_lucivy::schema::Term::from_field_u64(field, doc_id as u64);
        let mut guard = self.handle.writer.lock()
            .map_err(|_| JsError::new("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| JsError::new("index is closed"))?;
        writer.delete_term(term);
        self.handle.mark_uncommitted();
        Ok(())
    }

    #[wasm_bindgen]
    pub fn update(&self, doc_id: u32, fields_json: &str) -> Result<(), JsError> {
        self.remove(doc_id)?;
        self.add(doc_id, fields_json)?;
        Ok(())
    }

    // ── Transaction ────────────────────────────────────────────────────

    /// Commit changes to the in-memory directory.
    /// After this, call `exportDirtyFiles()` to get files to persist to OPFS.
    #[wasm_bindgen]
    pub fn commit(&self) -> Result<(), JsError> {
        let mut guard = self.handle.writer.lock()
            .map_err(|_| JsError::new("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| JsError::new("index is closed"))?;
        writer.commit()
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.handle.reader.reload()
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    #[wasm_bindgen]
    pub fn rollback(&self) -> Result<(), JsError> {
        let mut guard = self.handle.writer.lock()
            .map_err(|_| JsError::new("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| JsError::new("index is closed"))?;
        writer.rollback()
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    // ── OPFS sync ──────────────────────────────────────────────────────

    /// Export dirty files (modified + deleted) since last export.
    /// Returns a JS object: `{ modified: [[path, Uint8Array], ...], deleted: [path, ...] }`
    /// The JS side writes modified files to OPFS and removes deleted ones.
    #[wasm_bindgen(js_name = "exportDirtyFiles")]
    pub fn export_dirty_files(&self) -> Result<JsValue, JsError> {
        let (modified, deleted) = self.directory.export_dirty();

        let result = js_sys::Object::new();

        let mod_array = js_sys::Array::new();
        for (path, data) in modified {
            let pair = js_sys::Array::new();
            pair.push(&JsValue::from_str(&path));
            pair.push(&js_sys::Uint8Array::from(data.as_slice()).into());
            mod_array.push(&pair);
        }
        js_sys::Reflect::set(&result, &"modified".into(), &mod_array)
            .map_err(|_| JsError::new("reflect set failed"))?;

        let del_array = js_sys::Array::new();
        for path in deleted {
            del_array.push(&JsValue::from_str(&path));
        }
        js_sys::Reflect::set(&result, &"deleted".into(), &del_array)
            .map_err(|_| JsError::new("reflect set failed"))?;

        Ok(result.into())
    }

    /// Export ALL files (for initial persist after create).
    /// Returns: `[[path, Uint8Array], ...]`
    #[wasm_bindgen(js_name = "exportAllFiles")]
    pub fn export_all_files(&self) -> Result<JsValue, JsError> {
        let files = self.directory.export_all();
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
        snapshot::check_committed(&self.handle, &self.index_path)
            .map_err(|e| JsError::new(&e))?;

        let files = self.collect_snapshot_files();
        let snap = snapshot::SnapshotIndex {
            path: &self.index_path,
            files,
        };
        Ok(snapshot::export_snapshot(&[snap]))
    }

    /// Import a LUCE snapshot blob and return a new Index.
    /// `data`: the LUCE blob as `Uint8Array`.
    /// `path`: the logical path for the imported index.
    #[wasm_bindgen(js_name = "importSnapshot")]
    pub fn import_snapshot(data: &[u8], path: &str) -> Result<Index, JsError> {
        let mut indexes = snapshot::import_snapshot(data)
            .map_err(|e| JsError::new(&e))?;
        if indexes.is_empty() {
            return Err(JsError::new("snapshot contains no indexes"));
        }
        let imported = indexes.remove(0);

        let directory = MemoryDirectory::new();
        for (name, file_data) in &imported.files {
            directory.import_file(name, file_data.clone());
        }

        let handle = LucivyHandle::open(directory.clone())
            .map_err(|e| JsError::new(&e))?;

        let text_fields = match &handle.config {
            Some(config) => extract_text_fields(config),
            None => Vec::new(),
        };

        Ok(Index {
            handle,
            text_fields,
            directory,
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

        let lucivy_query = query::build_query(
            &query_config,
            &self.handle.schema,
            &self.handle.index,
            &self.handle.raw_field_pairs,
            &self.handle.ngram_field_pairs,
            highlight_sink.clone(),
        ).map_err(|e| JsError::new(&e))?;

        let searcher = self.handle.reader.searcher();
        let top_docs = execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?;
        let results = collect_results(&searcher, &top_docs, &self.handle.schema, highlight_sink.as_deref())?;
        serde_json::to_string(&results)
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

        let lucivy_query = query::build_query(
            &query_config,
            &self.handle.schema,
            &self.handle.index,
            &self.handle.raw_field_pairs,
            &self.handle.ngram_field_pairs,
            highlight_sink.clone(),
        ).map_err(|e| JsError::new(&e))?;

        let id_set: HashSet<u64> = allowed_ids.iter().map(|&id| id as u64).collect();
        let searcher = self.handle.reader.searcher();
        let top_docs = execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set)?;
        let results = collect_results(&searcher, &top_docs, &self.handle.schema, highlight_sink.as_deref())?;
        serde_json::to_string(&results)
            .map_err(|e| JsError::new(&format!("serialize error: {e}")))
    }

    // ── Info ───────────────────────────────────────────────────────────

    #[wasm_bindgen(js_name = "numDocs", getter)]
    pub fn num_docs(&self) -> u32 {
        self.handle.reader.searcher().num_docs() as u32
    }

    #[wasm_bindgen(getter)]
    pub fn path(&self) -> String {
        self.index_path.clone()
    }

    #[wasm_bindgen(js_name = "schemaJson", getter)]
    pub fn schema_json(&self) -> String {
        match &self.handle.config {
            Some(config) => serde_json::to_string(config).unwrap_or_default(),
            None => String::new(),
        }
    }
}

// ── Private helpers ────────────────────────────────────────────────────────

const EXCLUDED_FILES: &[&str] = &[".lock", ".tantivy-writer.lock", ".lucivy-writer.lock", ".managed.json"];

impl Index {
    fn collect_snapshot_files(&self) -> Vec<(String, Vec<u8>)> {
        self.directory.export_all()
            .into_iter()
            .filter(|(name, _)| !EXCLUDED_FILES.contains(&name.as_str()))
            .collect()
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
                self.auto_duplicate(doc, field_name, text);
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

    fn auto_duplicate(&self, doc: &mut LucivyDocument, field_name: &str, text: &str) {
        if let Some(raw_name) = self.handle.raw_field_pairs.iter()
            .find(|(user, _)| user == field_name)
            .map(|(_, raw)| raw.as_str())
        {
            if let Some(f) = self.handle.field(raw_name) {
                doc.add_text(f, text);
            }
        }
        if let Some(ngram_name) = self.handle.ngram_field_pairs.iter()
            .find(|(user, _)| user == field_name)
            .map(|(_, ngram)| ngram.as_str())
        {
            if let Some(f) = self.handle.field(ngram_name) {
                doc.add_text(f, text);
            }
        }
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

fn execute_top_docs(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
) -> Result<Vec<(f32, DocAddress)>, JsError> {
    let collector = TopDocs::with_limit(limit as usize).order_by_score();
    searcher.search(query, &collector)
        .map_err(|e| JsError::new(&format!("search error: {e}")))
}

fn execute_top_docs_filtered(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
    allowed_ids: HashSet<u64>,
) -> Result<Vec<(f32, DocAddress)>, JsError> {
    let inner = TopDocs::with_limit(limit as usize).order_by_score();
    let collector = FilterCollector::new(
        NODE_ID_FIELD.to_string(),
        move |value: u64| allowed_ids.contains(&value),
        inner,
    );
    searcher.search(query, &collector)
        .map_err(|e| JsError::new(&format!("filtered search error: {e}")))
}

#[derive(serde::Serialize)]
struct SearchResultJson {
    #[serde(rename = "docId")]
    doc_id: u32,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    highlights: Option<HashMap<String, Vec<[u32; 2]>>>,
}

fn collect_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
) -> Result<Vec<SearchResultJson>, JsError> {
    let nid_field = schema.get_field(NODE_ID_FIELD)
        .map_err(|_| JsError::new("no _node_id field in schema"))?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher.doc(doc_addr)
            .map_err(|e| JsError::new(&e.to_string()))?;
        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
            let by_field = sink.get(seg_id, doc_addr.doc_id)?;
            let map: HashMap<String, Vec<[u32; 2]>> = by_field.into_iter()
                .filter(|(name, _)| !name.ends_with(RAW_SUFFIX) && !name.ends_with(NGRAM_SUFFIX))
                .map(|(name, offsets)| {
                    let ranges: Vec<[u32; 2]> = offsets.into_iter()
                        .map(|[s, e]| [s as u32, e as u32])
                        .collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        results.push(SearchResultJson {
            doc_id: doc_id as u32,
            score,
            highlights,
        });
    }
    Ok(results)
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
