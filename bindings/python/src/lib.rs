//! lucivy — Python bindings for ld-lucivy BM25 full-text search.
//!
//! Provides a Pythonic API for creating, managing, and querying Lucivy indexes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;


use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::{DocAddress, Searcher, LucivyDocument};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::directory::StdFsDirectory;
use lucivy_core::query;
use lucivy_core::snapshot;

// ─── SearchResult ──────────────────────────────────────────────────────────

#[pyclass]
#[derive(Clone)]
struct SearchResult {
    #[pyo3(get)]
    doc_id: u64,
    #[pyo3(get)]
    score: f32,
    #[pyo3(get)]
    highlights: Option<HashMap<String, Vec<(u32, u32)>>>,
    #[pyo3(get)]
    fields: Option<HashMap<String, String>>,
}

#[pymethods]
impl SearchResult {
    fn __repr__(&self) -> String {
        let mut parts = vec![
            format!("doc_id={}", self.doc_id),
            format!("score={:.4}", self.score),
        ];
        if let Some(ref h) = self.highlights {
            parts.push(format!("highlights={:?}", h));
        }
        if let Some(ref f) = self.fields {
            parts.push(format!("fields={:?}", f));
        }
        format!("SearchResult({})", parts.join(", "))
    }
}

// ─── Index ─────────────────────────────────────────────────────────────────

#[pyclass]
struct Index {
    handle: LucivyHandle,
    index_path: String,
    /// User field names (excludes _node_id).
    user_fields: Vec<(String, String)>, // (name, field_type)
    /// Text field names (for default parse query).
    text_fields: Vec<String>,
}

#[pymethods]
impl Index {
    /// Create a new index at the given path.
    ///
    /// Args:
    ///     path: Directory path for the index files.
    ///     fields: List of field definitions, e.g. [{"name": "body", "type": "text"}].
    ///     sfx: Whether to build suffix FST for contains/startsWith queries (default True).
    ///          Set to False for faster indexation and smaller indexes.
    #[staticmethod]
    #[pyo3(signature = (path, fields, sfx=None))]
    fn create(path: &str, fields: &Bound<'_, PyList>, sfx: Option<bool>) -> PyResult<Self> {
        let mut field_defs = Vec::new();
        for item in fields.iter() {
            let dict: &Bound<'_, PyDict> = item.downcast()?;
            let name: String = dict.get_item("name")?
                .ok_or_else(|| PyValueError::new_err("field missing 'name'"))?
                .extract()?;
            let field_type: String = dict.get_item("type")?
                .ok_or_else(|| PyValueError::new_err("field missing 'type'"))?
                .extract()?;
            let stored: Option<bool> = dict.get_item("stored")?.and_then(|v| v.extract().ok());
            let indexed: Option<bool> = dict.get_item("indexed")?.and_then(|v| v.extract().ok());
            let fast: Option<bool> = dict.get_item("fast")?.and_then(|v| v.extract().ok());
            field_defs.push(query::FieldDef {
                name,
                field_type,
                stored,
                indexed,
                fast,
            });
        }

        let config = query::SchemaConfig {
            fields: field_defs,
            tokenizer: None,
            sfx,
            ..Default::default()
        };

        let directory = StdFsDirectory::open(path)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let handle = LucivyHandle::create(directory, &config)
            .map_err(|e| PyValueError::new_err(e))?;

        let (user_fields, text_fields) = extract_user_fields(&config);

        Ok(Self {
            handle,
            index_path: path.to_string(),
            user_fields,
            text_fields,
        })
    }

    /// Open an existing index at the given path.
    #[staticmethod]
    fn open(path: &str) -> PyResult<Self> {
        let directory = StdFsDirectory::open(path)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let handle = LucivyHandle::open(directory)
            .map_err(|e| PyValueError::new_err(e))?;

        let (user_fields, text_fields) = match &handle.config {
            Some(config) => extract_user_fields(config),
            None => (Vec::new(), Vec::new()),
        };

        Ok(Self {
            handle,
            index_path: path.to_string(),
            user_fields,
            text_fields,
        })
    }

    /// Add a document. First positional arg is doc_id (u64), remaining are field kwargs.
    ///
    /// Example: index.add(1, title="Hello", body="World", price=9.99)
    #[pyo3(signature = (doc_id, **kwargs))]
    fn add(&self, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        let kwargs = kwargs.ok_or_else(|| PyValueError::new_err("at least one field is required"))?;
        let mut doc = LucivyDocument::new();

        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id);

        add_fields_from_dict(&self.handle, &mut doc, kwargs)?;

        let mut guard = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| PyValueError::new_err("index is closed"))?;
        writer.add_document(doc)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Add multiple documents at once.
    ///
    /// Each element must be a dict with a "doc_id" key and field values.
    /// Example: index.add_many([{"doc_id": 1, "title": "Hello"}, ...])
    fn add_many(&self, docs: &Bound<'_, PyList>) -> PyResult<()> {
        let mut guard = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| PyValueError::new_err("index is closed"))?;

        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;

        for item in docs.iter() {
            let dict: &Bound<'_, PyDict> = item.downcast()?;
            let doc_id: u64 = dict.get_item("doc_id")?
                .ok_or_else(|| PyValueError::new_err("each doc must have a 'doc_id' key"))?
                .extract()?;

            let mut doc = LucivyDocument::new();
            doc.add_u64(nid_field, doc_id);

            for (key, value) in dict.iter() {
                let field_name: String = key.extract()?;
                if field_name == "doc_id" { continue; }
                add_field_value(&self.handle, &mut doc, &field_name, &value)?;
            }

            writer.add_document(doc)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Delete a document by doc_id.
    fn delete(&self, doc_id: u64) -> PyResult<()> {
        let field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;
        let term = ld_lucivy::schema::Term::from_field_u64(field, doc_id);
        let mut guard = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| PyValueError::new_err("index is closed"))?;
        writer.delete_term(term);
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Update a document (delete + re-add).
    #[pyo3(signature = (doc_id, **kwargs))]
    fn update(&self, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        self.delete(doc_id)?;
        self.add(doc_id, kwargs)?;
        Ok(())
    }

    /// Commit pending changes (makes added/deleted docs visible to searches).
    /// Also waits for any pending merges to complete.
    fn commit(&self) -> PyResult<()> {
        let mut guard = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| PyValueError::new_err("index is closed"))?;
        writer.commit()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        writer.drain_merges()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        drop(guard);
        self.handle.reader.reload()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    /// Rollback pending changes.
    fn rollback(&self) -> PyResult<()> {
        let mut guard = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        let writer = guard.as_mut()
            .ok_or_else(|| PyValueError::new_err("index is closed"))?;
        writer.rollback()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    /// Close the index: commit pending writes and release the writer lock.
    fn close(&self) -> PyResult<()> {
        self.handle.close()
            .map_err(|e| PyValueError::new_err(e))
    }

    // ── Delta sync ──────────────────────────────────────────────────────

    /// Current version of the index (hash of meta.json).
    #[getter]
    fn version(&self) -> PyResult<String> {
        lucivy_core::sync::compute_version(&self.handle)
            .map_err(|e| PyValueError::new_err(e))
    }

    /// List of segment IDs in the current committed state.
    #[getter]
    fn segment_ids(&self) -> PyResult<Vec<String>> {
        let meta = self.handle.index.load_metas()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(meta.segments.iter().map(|s| s.id().uuid_string()).collect())
    }

    /// Export a LUCID delta blob from this index.
    ///
    /// Args:
    ///     client_version: The version the client currently has.
    ///     client_segment_ids: List of segment ID strings the client has.
    ///
    /// Returns: bytes (LUCID binary blob).
    #[pyo3(signature = (client_version, client_segment_ids))]
    fn export_delta<'py>(
        &self,
        py: Python<'py>,
        client_version: &str,
        client_segment_ids: Vec<String>,
    ) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let client_ids: std::collections::HashSet<String> = client_segment_ids.into_iter().collect();
        let delta = lucivy_core::sync::export_delta(
            &self.handle,
            std::path::Path::new(&self.index_path),
            &client_ids,
            client_version,
        ).map_err(|e| PyValueError::new_err(e))?;

        let blob = lucistore::delta::serialize_delta(&delta);
        Ok(pyo3::types::PyBytes::new(py, &blob))
    }

    /// Apply a LUCID delta blob to this index.
    ///
    /// Writes new segment files, removes old ones, writes new meta.json.
    /// Then reopens the reader so new docs are visible.
    fn apply_delta(&self, data: &[u8]) -> PyResult<()> {
        let delta = lucistore::delta::deserialize_delta(data)
            .map_err(|e| PyValueError::new_err(e))?;
        lucistore::fs_utils::apply_delta(
            std::path::Path::new(&self.index_path),
            &delta,
        ).map_err(|e| PyValueError::new_err(e))?;

        self.handle.reader.reload()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(())
    }

    /// Export a LUCID delta blob to a file.
    #[pyo3(signature = (path, client_version, client_segment_ids))]
    fn export_delta_to(
        &self,
        path: &str,
        client_version: &str,
        client_segment_ids: Vec<String>,
    ) -> PyResult<()> {
        let client_ids: std::collections::HashSet<String> = client_segment_ids.into_iter().collect();
        let delta = lucivy_core::sync::export_delta(
            &self.handle,
            std::path::Path::new(&self.index_path),
            &client_ids,
            client_version,
        ).map_err(|e| PyValueError::new_err(e))?;

        let blob = lucistore::delta::serialize_delta(&delta);
        std::fs::write(path, &blob)
            .map_err(|e| PyValueError::new_err(format!("cannot write delta: {e}")))?;
        Ok(())
    }

    /// Search the index.
    ///
    /// Args:
    ///     query: A string (parse query on all text fields) or a dict (raw QueryConfig).
    ///     limit: Max number of results (default 10).
    ///     highlights: Whether to include highlight offsets (default False).
    ///     allowed_ids: Optional list of doc_ids to restrict search to.
    #[pyo3(signature = (query, limit=10, highlights=false, allowed_ids=None, fields=false))]
    fn search(
        &self,
        query: &Bound<'_, PyAny>,
        limit: u32,
        highlights: bool,
        allowed_ids: Option<Vec<u64>>,
        fields: bool,
    ) -> PyResult<Vec<SearchResult>> {
        let query_config = self.parse_query(query)?;

        let highlight_sink = if highlights {
            Some(Arc::new(HighlightSink::new()))
        } else {
            None
        };

        let top_docs = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().collect();
                self.handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
                    .map_err(|e| PyValueError::new_err(e))?
            }
            None => self.handle.search(&query_config, limit as usize, highlight_sink.clone())
                .map_err(|e| PyValueError::new_err(e))?,
        };
        let searcher = self.handle.reader.searcher();

        collect_results(&searcher, &top_docs, &self.handle.schema, highlight_sink.as_deref(), fields)
    }

    /// Number of documents in the index.
    #[getter]
    fn num_docs(&self) -> u64 {
        self.handle.reader.searcher().num_docs()
    }

    /// Index path.
    #[getter]
    fn path(&self) -> &str {
        &self.index_path
    }

    /// Schema as a list of field dicts.
    #[getter]
    fn schema(&self) -> Vec<HashMap<String, String>> {
        self.user_fields.iter().map(|(name, ft)| {
            let mut m = HashMap::new();
            m.insert("name".to_string(), name.clone());
            m.insert("type".to_string(), ft.clone());
            m
        }).collect()
    }

    /// Export this index as a LUCE snapshot (bytes).
    ///
    /// Raises ValueError if there are uncommitted changes.
    fn export_snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        snapshot::check_committed(&self.handle, &self.index_path)
            .map_err(|e| PyValueError::new_err(e))?;

        let files = snapshot::read_directory_files(std::path::Path::new(&self.index_path))
            .map_err(|e| PyValueError::new_err(e))?;

        let idx = snapshot::SnapshotIndex {
            path: &self.index_path,
            files,
        };
        let blob = snapshot::export_snapshot(&[idx]);
        Ok(pyo3::types::PyBytes::new(py, &blob))
    }

    /// Export this index as a LUCE snapshot to a file.
    ///
    /// Raises ValueError if there are uncommitted changes.
    fn export_snapshot_to(&self, path: &str) -> PyResult<()> {
        snapshot::check_committed(&self.handle, &self.index_path)
            .map_err(|e| PyValueError::new_err(e))?;

        let files = snapshot::read_directory_files(std::path::Path::new(&self.index_path))
            .map_err(|e| PyValueError::new_err(e))?;

        let idx = snapshot::SnapshotIndex {
            path: &self.index_path,
            files,
        };
        let blob = snapshot::export_snapshot(&[idx]);
        std::fs::write(path, &blob)
            .map_err(|e| PyValueError::new_err(format!("cannot write snapshot: {e}")))?;
        Ok(())
    }

    /// Import an index from a LUCE snapshot (bytes).
    ///
    /// The snapshot must contain exactly one index.
    /// The index is restored at `dest_path` (or at the original path if None).
    #[staticmethod]
    #[pyo3(signature = (data, dest_path=None))]
    fn import_snapshot(data: &[u8], dest_path: Option<&str>) -> PyResult<Self> {
        let snap = snapshot::import_snapshot(data)
            .map_err(|e| PyValueError::new_err(e))?;

        if snap.indexes.len() != 1 {
            return Err(PyValueError::new_err(format!(
                "expected 1 index in snapshot, got {}. Use lucivy.import_snapshots() for multi-index.",
                snap.indexes.len()
            )));
        }

        let imported = &snap.indexes[0];
        let target_path = dest_path.unwrap_or(&imported.path);

        write_imported_files(target_path, &imported.files)?;

        Self::open(target_path)
    }

    /// Import an index from a LUCE snapshot file.
    #[staticmethod]
    #[pyo3(signature = (path, dest_path=None))]
    fn import_snapshot_from(path: &str, dest_path: Option<&str>) -> PyResult<Self> {
        let data = std::fs::read(path)
            .map_err(|e| PyValueError::new_err(format!("cannot read snapshot: {e}")))?;
        Self::import_snapshot(&data, dest_path)
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        // Don't auto-commit — the user controls transactions.
        Ok(false)
    }

    fn __repr__(&self) -> String {
        format!("Index(path='{}', num_docs={})", self.index_path, self.num_docs())
    }
}

impl Index {
    /// Parse a Python query arg (str or dict) into a QueryConfig.
    fn parse_query(&self, query: &Bound<'_, PyAny>) -> PyResult<query::QueryConfig> {
        if let Ok(s) = query.extract::<String>() {
            // String → contains_split on all text fields.
            // Each word becomes a `contains` query, combined with boolean should.
            // For multi-field, each word is a boolean should across all text fields.
            if self.text_fields.is_empty() {
                return Err(PyValueError::new_err("no text fields in schema for string query"));
            }
            Ok(build_contains_split_multi_field(&s, &self.text_fields, None))
        } else if let Ok(dict) = query.downcast::<PyDict>() {
            // Dict → serialize to JSON → parse as QueryConfig.
            let py = dict.py();
            let json_mod = py.import("json")?;
            let json_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
            let mut config: query::QueryConfig = serde_json::from_str(&json_str)
                .map_err(|e| PyValueError::new_err(format!("invalid query dict: {e}")))?;
            // contains_split and startsWith_split handled by build_query in core
            Ok(config)
        } else {
            Err(PyValueError::new_err("query must be a string or a dict"))
        }
    }
}

/// Build a contains_split query across multiple text fields.
///
/// For a single field: "rust safety" → boolean should [contains("rust"), contains("safety")]
/// For multiple fields: each word becomes a boolean should across all fields.
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

    let word_queries: Vec<query::QueryConfig> = words.iter().map(|word| {
        let field_queries: Vec<query::QueryConfig> = text_fields.iter().map(|f| {
            query::QueryConfig {
                query_type: "contains".into(),
                field: Some(f.clone()),
                value: Some(word.to_string()),
                distance,
                ..Default::default()
            }
        }).collect();
        query::QueryConfig {
            query_type: "boolean".into(),
            should: Some(field_queries),
            ..Default::default()
        }
    }).collect();

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
    let user_fields: Vec<(String, String)> = config.fields.iter()
        .map(|f| (f.name.clone(), f.field_type.clone()))
        .collect();
    let text_fields: Vec<String> = config.fields.iter()
        .filter(|f| f.field_type == "text")
        .map(|f| f.name.clone())
        .collect();
    (user_fields, text_fields)
}

fn add_fields_from_dict(
    handle: &LucivyHandle,
    doc: &mut LucivyDocument,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<()> {
    for (key, value) in kwargs.iter() {
        let field_name: String = key.extract()?;
        add_field_value(handle, doc, &field_name, &value)?;
    }
    Ok(())
}

fn add_field_value(
    handle: &LucivyHandle,
    doc: &mut LucivyDocument,
    field_name: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let field = handle.field(field_name)
        .ok_or_else(|| PyValueError::new_err(format!("unknown field: {field_name}")))?;
    let field_entry = handle.schema.get_field_entry(field);

    match field_entry.field_type() {
        FieldType::Str(_) => {
            let text: String = value.extract()?;
            doc.add_text(field, &text);
        }
        FieldType::U64(_) => {
            let v: u64 = value.extract()?;
            doc.add_u64(field, v);
        }
        FieldType::I64(_) => {
            let v: i64 = value.extract()?;
            doc.add_i64(field, v);
        }
        FieldType::F64(_) => {
            let v: f64 = value.extract()?;
            doc.add_f64(field, v);
        }
        _ => return Err(PyValueError::new_err(format!("unsupported field type for {field_name}"))),
    }
    Ok(())
}

fn collect_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> PyResult<Vec<SearchResult>> {
    let nid_field = schema.get_field(NODE_ID_FIELD)
        .map_err(|_| PyValueError::new_err("no _node_id field in schema"))?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher.doc(doc_addr)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
            let by_field = sink.get(seg_id, doc_addr.doc_id)?;
            let map: HashMap<String, Vec<(u32, u32)>> = by_field.into_iter()
                .map(|(name, offsets)| {
                    let ranges = offsets.into_iter().map(|[s, e]| (s as u32, e as u32)).collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        let fields = if include_fields {
            let mut map = HashMap::new();
            for (field, value) in doc.field_values() {
                let name = schema.get_field_name(field);
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

        results.push(SearchResult { doc_id, score, highlights, fields });
    }
    Ok(results)
}

fn write_imported_files(dest_path: &str, files: &[(String, Vec<u8>)]) -> PyResult<()> {
    std::fs::create_dir_all(dest_path)
        .map_err(|e| PyValueError::new_err(format!("cannot create directory '{}': {e}", dest_path)))?;
    for (name, data) in files {
        let file_path = std::path::Path::new(dest_path).join(name);
        std::fs::write(&file_path, data)
            .map_err(|e| PyValueError::new_err(format!("cannot write '{}': {e}", file_path.display())))?;
    }
    Ok(())
}

// ─── Module-level functions ─────────────────────────────────────────────

/// Export multiple indexes into a single LUCE snapshot.
#[pyfunction]
fn export_snapshots<'py>(py: Python<'py>, indexes: Vec<PyRef<'_, Index>>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    let mut files_storage = Vec::with_capacity(indexes.len());

    for idx in &indexes {
        snapshot::check_committed(&idx.handle, &idx.index_path)
            .map_err(|e| PyValueError::new_err(e))?;
        let files = snapshot::read_directory_files(std::path::Path::new(&idx.index_path))
            .map_err(|e| PyValueError::new_err(e))?;
        files_storage.push((idx.index_path.clone(), files));
    }

    let snapshot_indexes: Vec<snapshot::SnapshotIndex<'_>> = files_storage.iter()
        .map(|(path, files)| snapshot::SnapshotIndex { path, files: files.clone() })
        .collect();

    let blob = snapshot::export_snapshot(&snapshot_indexes);
    Ok(pyo3::types::PyBytes::new(py, &blob))
}

/// Import multiple indexes from a single LUCE snapshot.
#[pyfunction]
#[pyo3(signature = (data, dest_paths=None))]
fn import_snapshots(data: &[u8], dest_paths: Option<Vec<String>>) -> PyResult<Vec<Index>> {
    let snap = snapshot::import_snapshot(data)
        .map_err(|e| PyValueError::new_err(e))?;

    if let Some(ref paths) = dest_paths {
        if paths.len() != snap.indexes.len() {
            return Err(PyValueError::new_err(format!(
                "dest_paths length ({}) doesn't match snapshot index count ({})",
                paths.len(), snap.indexes.len()
            )));
        }
    }

    let mut result = Vec::with_capacity(snap.indexes.len());
    for (i, imported) in snap.indexes.iter().enumerate() {
        let target = match &dest_paths {
            Some(paths) => paths[i].as_str(),
            None => &imported.path,
        };
        write_imported_files(target, &imported.files)?;
        result.push(Index::open(target)?);
    }
    Ok(result)
}

// ─── Module ────────────────────────────────────────────────────────────────

#[pymodule]
fn lucivy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Index>()?;
    m.add_class::<SearchResult>()?;
    m.add_function(wrap_pyfunction!(export_snapshots, m)?)?;
    m.add_function(wrap_pyfunction!(import_snapshots, m)?)?;
    Ok(())
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
