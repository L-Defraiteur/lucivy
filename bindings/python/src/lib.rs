//! lucivy — Python bindings for ld-lucivy BM25 full-text search.
//!
//! Unified on ShardedHandle (even single-shard uses ShardedHandle with shards=1).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::snapshot;
use lucivy_core::sharded_handle::{ShardedHandle, ShardedSearchResult};

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
    handle: ShardedHandle,
    index_path: String,
    user_fields: Vec<(String, String)>,
    text_fields: Vec<String>,
}

#[pymethods]
impl Index {
    /// Create a new index at the given path.
    ///
    /// Args:
    ///     path: Directory path for the index files.
    ///     fields: List of field definitions, e.g. [{"name": "body", "type": "text"}].
    ///     sfx: Whether to build suffix FST (default True).
    ///     shards: Number of shards (default 1).
    #[staticmethod]
    #[pyo3(signature = (path, fields, sfx=None, shards=None))]
    fn create(path: &str, fields: &Bound<'_, PyList>, sfx: Option<bool>, shards: Option<usize>) -> PyResult<Self> {
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
            shards,
            ..Default::default()
        };

        let handle = ShardedHandle::create(path, &config)
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
        let handle = ShardedHandle::open(path)
            .map_err(|e| PyValueError::new_err(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

        Ok(Self {
            handle,
            index_path: path.to_string(),
            user_fields,
            text_fields,
        })
    }

    /// Add a document.
    #[pyo3(signature = (doc_id, **kwargs))]
    fn add(&self, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        let kwargs = kwargs.ok_or_else(|| PyValueError::new_err("at least one field is required"))?;
        let mut doc = LucivyDocument::new();

        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id);

        add_fields_from_dict(&self.handle, &mut doc, kwargs)?;

        self.handle.add_document(doc, doc_id)
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Add multiple documents at once.
    fn add_many(&self, docs: &Bound<'_, PyList>) -> PyResult<()> {
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

            self.handle.add_document(doc, doc_id)
                .map_err(|e| PyValueError::new_err(e))?;
        }
        Ok(())
    }

    /// Delete a document by doc_id.
    fn delete(&self, doc_id: u64) -> PyResult<()> {
        self.handle.delete_by_node_id(doc_id)
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Update a document (delete + re-add).
    #[pyo3(signature = (doc_id, **kwargs))]
    fn update(&self, doc_id: u64, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<()> {
        self.delete(doc_id)?;
        self.add(doc_id, kwargs)
    }

    /// Commit pending changes.
    fn commit(&self) -> PyResult<()> {
        self.handle.commit()
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Close the index.
    fn close(&self) -> PyResult<()> {
        self.handle.close()
            .map_err(|e| PyValueError::new_err(e))
    }

    /// Search the index.
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

        let results = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().collect();
                self.handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
                    .map_err(|e| PyValueError::new_err(e))?
            }
            None => self.handle.search(&query_config, limit as usize, highlight_sink.clone())
                .map_err(|e| PyValueError::new_err(e))?,
        };

        collect_sharded_results(&self.handle, &results, highlight_sink.as_deref(), fields)
    }

    #[getter]
    fn num_docs(&self) -> u64 {
        self.handle.num_docs()
    }

    #[getter]
    fn num_shards(&self) -> usize {
        self.handle.num_shards()
    }

    #[getter]
    fn path(&self) -> &str {
        &self.index_path
    }

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
    fn export_snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let blob = snapshot::export_to_snapshot(
            &self.handle,
            std::path::Path::new(&self.index_path),
        ).map_err(|e| PyValueError::new_err(e))?;
        Ok(pyo3::types::PyBytes::new(py, &blob))
    }

    /// Export this index as a LUCE snapshot to a file.
    fn export_snapshot_to(&self, path: &str) -> PyResult<()> {
        let blob = snapshot::export_to_snapshot(
            &self.handle,
            std::path::Path::new(&self.index_path),
        ).map_err(|e| PyValueError::new_err(e))?;
        std::fs::write(path, &blob)
            .map_err(|e| PyValueError::new_err(format!("cannot write snapshot: {e}")))?;
        Ok(())
    }

    /// Import an index from a LUCE snapshot (bytes).
    #[staticmethod]
    #[pyo3(signature = (data, dest_path=None))]
    fn import_snapshot(data: &[u8], dest_path: Option<&str>) -> PyResult<Self> {
        let dest = dest_path.unwrap_or("/tmp/lucivy_import");
        let dest_path = std::path::Path::new(dest);
        let handle = snapshot::import_from_snapshot(data, dest_path)
            .map_err(|e| PyValueError::new_err(e))?;
        let (user_fields, text_fields) = extract_user_fields(&handle.config);
        Ok(Self {
            handle,
            index_path: dest.to_string(),
            user_fields,
            text_fields,
        })
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
        Ok(false)
    }

    fn __repr__(&self) -> String {
        format!("Index(path='{}', num_docs={}, shards={})",
            self.index_path, self.num_docs(), self.num_shards())
    }
}

impl Index {
    fn parse_query(&self, query: &Bound<'_, PyAny>) -> PyResult<query::QueryConfig> {
        if let Ok(s) = query.extract::<String>() {
            if self.text_fields.is_empty() {
                return Err(PyValueError::new_err("no text fields in schema for string query"));
            }
            Ok(build_contains_split_multi_field(&s, &self.text_fields, None))
        } else if let Ok(dict) = query.downcast::<PyDict>() {
            let py = dict.py();
            let json_mod = py.import("json")?;
            let json_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
            let config: query::QueryConfig = serde_json::from_str(&json_str)
                .map_err(|e| PyValueError::new_err(format!("invalid query dict: {e}")))?;
            Ok(config)
        } else {
            Err(PyValueError::new_err("query must be a string or a dict"))
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

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
    handle: &ShardedHandle,
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
    handle: &ShardedHandle,
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

fn collect_sharded_results(
    handle: &ShardedHandle,
    results: &[ShardedSearchResult],
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> PyResult<Vec<SearchResult>> {
    let nid_field = handle.schema.get_field(NODE_ID_FIELD)
        .map_err(|_| PyValueError::new_err("no _node_id field"))?;

    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let shard = handle.shard(r.shard_id)
            .ok_or_else(|| PyValueError::new_err(format!("shard {} not found", r.shard_id)))?;
        let searcher = shard.reader.searcher();
        let doc: LucivyDocument = searcher.doc(r.doc_address)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;

        let doc_id = doc.get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
            let by_field = sink.get(seg_id, r.doc_address.doc_id)?;
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
                let name = handle.schema.get_field_name(field);
                if name == NODE_ID_FIELD { continue; }
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

        out.push(SearchResult { doc_id, score: r.score, highlights, fields });
    }
    Ok(out)
}

// ─── Module ────────────────────────────────────────────────────────────────

#[pymodule]
fn lucivy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Index>()?;
    m.add_class::<SearchResult>()?;
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
}
