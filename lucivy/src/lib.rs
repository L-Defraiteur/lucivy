//! lucivy — Python bindings for ld-lucivy BM25 full-text search.
//!
//! Provides a Pythonic API for creating, managing, and querying Lucivy indexes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::collector::{FilterCollector, TopDocs};
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::{DocAddress, Searcher, LucivyDocument};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use lucivy_fts::handle::{LucivyHandle, NGRAM_SUFFIX, NODE_ID_FIELD, RAW_SUFFIX};
use lucivy_fts::query;

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
}

#[pymethods]
impl SearchResult {
    fn __repr__(&self) -> String {
        match &self.highlights {
            Some(h) => format!("SearchResult(doc_id={}, score={:.4}, highlights={:?})", self.doc_id, self.score, h),
            None => format!("SearchResult(doc_id={}, score={:.4})", self.doc_id, self.score),
        }
    }
}

// ─── Index ─────────────────────────────────────────────────────────────────

#[pyclass]
struct Index {
    handle: LucivyHandle,
    index_path: String,
    /// User field names (excludes _node_id, ._raw, ._ngram).
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
    ///     stemmer: Optional language stemmer ("english", "french", etc.).
    #[staticmethod]
    #[pyo3(signature = (path, fields, stemmer=None))]
    fn create(path: &str, fields: &Bound<'_, PyList>, stemmer: Option<&str>) -> PyResult<Self> {
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
            stemmer: stemmer.map(String::from),
        };

        let handle = LucivyHandle::create(path, &config)
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
        let handle = LucivyHandle::open(path)
            .map_err(|e| PyValueError::new_err(e))?;

        // Read config to get user fields.
        let config_path = std::path::Path::new(path).join("_config.json");
        let (user_fields, text_fields) = if config_path.exists() {
            let config_str = std::fs::read_to_string(&config_path)
                .map_err(|e| PyValueError::new_err(format!("cannot read config: {e}")))?;
            let config: query::SchemaConfig = serde_json::from_str(&config_str)
                .map_err(|e| PyValueError::new_err(format!("cannot parse config: {e}")))?;
            extract_user_fields(&config)
        } else {
            (Vec::new(), Vec::new())
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

        let writer = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        writer.add_document(doc)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(())
    }

    /// Add multiple documents at once.
    ///
    /// Each element must be a dict with a "doc_id" key and field values.
    /// Example: index.add_many([{"doc_id": 1, "title": "Hello"}, ...])
    fn add_many(&self, docs: &Bound<'_, PyList>) -> PyResult<()> {
        let writer = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;

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
        Ok(())
    }

    /// Delete a document by doc_id.
    fn delete(&self, doc_id: u64) -> PyResult<()> {
        let field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| PyValueError::new_err("no _node_id field in schema"))?;
        let term = ld_lucivy::schema::Term::from_field_u64(field, doc_id);
        let writer = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        writer.delete_term(term);
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
    fn commit(&self) -> PyResult<()> {
        let mut writer = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        writer.commit()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.handle.reader.reload()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(())
    }

    /// Rollback pending changes.
    fn rollback(&self) -> PyResult<()> {
        let mut writer = self.handle.writer.lock()
            .map_err(|_| PyValueError::new_err("writer lock poisoned"))?;
        writer.rollback()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(())
    }

    /// Search the index.
    ///
    /// Args:
    ///     query: A string (parse query on all text fields) or a dict (raw QueryConfig).
    ///     limit: Max number of results (default 10).
    ///     highlights: Whether to include highlight offsets (default False).
    ///     allowed_ids: Optional list of doc_ids to restrict search to.
    #[pyo3(signature = (query, limit=10, highlights=false, allowed_ids=None))]
    fn search(
        &self,
        query: &Bound<'_, PyAny>,
        limit: u32,
        highlights: bool,
        allowed_ids: Option<Vec<u64>>,
    ) -> PyResult<Vec<SearchResult>> {
        let query_config = self.parse_query(query)?;

        let highlight_sink = if highlights {
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
        ).map_err(|e| PyValueError::new_err(e))?;

        let searcher = self.handle.reader.searcher();
        let top_docs = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().collect();
                execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set)?
            }
            None => execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?,
        };

        collect_results(&searcher, &top_docs, &self.handle.schema, highlight_sink.as_deref())
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
            Ok(build_contains_split_multi_field(&s, &self.text_fields))
        } else if let Ok(dict) = query.downcast::<PyDict>() {
            // Dict → serialize to JSON → parse as QueryConfig.
            let py = dict.py();
            let json_mod = py.import("json")?;
            let json_str: String = json_mod.call_method1("dumps", (dict,))?.extract()?;
            let mut config: query::QueryConfig = serde_json::from_str(&json_str)
                .map_err(|e| PyValueError::new_err(format!("invalid query dict: {e}")))?;
            // Expand "contains_split" type into a boolean should of contains queries.
            if config.query_type == "contains_split" {
                config = expand_contains_split(&config);
            }
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
fn build_contains_split_multi_field(value: &str, text_fields: &[String]) -> query::QueryConfig {
    let words: Vec<&str> = value.split_whitespace().collect();

    if text_fields.len() == 1 {
        // Single text field: simple contains_split.
        return expand_contains_split_for_field(value, &words, &text_fields[0]);
    }

    // Multiple text fields: each word → should across fields, all words → should together.
    let word_queries: Vec<query::QueryConfig> = words.iter().map(|word| {
        if text_fields.len() == 1 {
            query::QueryConfig {
                query_type: "contains".into(),
                field: Some(text_fields[0].clone()),
                value: Some(word.to_string()),
                ..Default::default()
            }
        } else {
            // One word across multiple fields → boolean should.
            let field_queries: Vec<query::QueryConfig> = text_fields.iter().map(|f| {
                query::QueryConfig {
                    query_type: "contains".into(),
                    field: Some(f.clone()),
                    value: Some(word.to_string()),
                    ..Default::default()
                }
            }).collect();
            query::QueryConfig {
                query_type: "boolean".into(),
                should: Some(field_queries),
                ..Default::default()
            }
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

/// Expand a contains_split QueryConfig (from dict) into boolean should of contains.
/// Mirrors bridge.rs build_typed_query_config "contains_split" logic.
fn expand_contains_split(config: &query::QueryConfig) -> query::QueryConfig {
    let value = config.value.as_deref().unwrap_or("");
    let field = config.field.as_deref().unwrap_or("");
    let words: Vec<&str> = value.split_whitespace().collect();
    expand_contains_split_for_field(value, &words, field)
}

fn expand_contains_split_for_field(value: &str, words: &[&str], field: &str) -> query::QueryConfig {
    if words.len() <= 1 {
        return query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(value.to_string()),
            ..Default::default()
        };
    }
    let should: Vec<query::QueryConfig> = words.iter().map(|w| {
        query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(w.to_string()),
            ..Default::default()
        }
    }).collect();
    query::QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
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
            auto_duplicate(handle, doc, field_name, &text);
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

/// Auto-duplicate text values into ._raw and ._ngram counterparts.
fn auto_duplicate(handle: &LucivyHandle, doc: &mut LucivyDocument, field_name: &str, text: &str) {
    if let Some(raw_name) = handle.raw_field_pairs.iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, raw)| raw.as_str())
    {
        if let Some(raw_field) = handle.field(raw_name) {
            doc.add_text(raw_field, text);
        }
    }
    if let Some(ngram_name) = handle.ngram_field_pairs.iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, ngram)| ngram.as_str())
    {
        if let Some(ngram_field) = handle.field(ngram_name) {
            doc.add_text(ngram_field, text);
        }
    }
}

fn execute_top_docs(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
) -> PyResult<Vec<(f32, DocAddress)>> {
    let collector = TopDocs::with_limit(limit as usize).order_by_score();
    searcher.search(query, &collector)
        .map_err(|e| PyValueError::new_err(format!("search error: {e}")))
}

fn execute_top_docs_filtered(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
    allowed_ids: HashSet<u64>,
) -> PyResult<Vec<(f32, DocAddress)>> {
    let inner = TopDocs::with_limit(limit as usize).order_by_score();
    let collector = FilterCollector::new(
        NODE_ID_FIELD.to_string(),
        move |value: u64| allowed_ids.contains(&value),
        inner,
    );
    searcher.search(query, &collector)
        .map_err(|e| PyValueError::new_err(format!("filtered search error: {e}")))
}

fn collect_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
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
                .filter(|(name, _)| !name.ends_with(RAW_SUFFIX) && !name.ends_with(NGRAM_SUFFIX))
                .map(|(name, offsets)| {
                    let ranges = offsets.into_iter().map(|[s, e]| (s as u32, e as u32)).collect();
                    (name, ranges)
                })
                .collect();
            if map.is_empty() { None } else { Some(map) }
        });

        results.push(SearchResult { doc_id, score, highlights });
    }
    Ok(results)
}

// ─── Module ────────────────────────────────────────────────────────────────

#[pymodule]
fn lucivy(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Index>()?;
    m.add_class::<SearchResult>()?;
    Ok(())
}
