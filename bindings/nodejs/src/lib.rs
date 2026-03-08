//! lucivy — Node.js bindings for ld-lucivy BM25 full-text search.
//!
//! Provides a JS/TS API for creating, managing, and querying Lucivy indexes.
//! Distributed under the MIT License.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ld_lucivy::collector::{FilterCollector, TopDocs};
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{FieldType, Value as LucivyValue};
use ld_lucivy::{DocAddress, LucivyDocument, Searcher};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use lucivy_core::handle::{LucivyHandle, NGRAM_SUFFIX, NODE_ID_FIELD, RAW_SUFFIX};
use lucivy_core::directory::StdFsDirectory;
use lucivy_core::query;
use lucivy_core::snapshot;

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
    handle: LucivyHandle,
    index_path: String,
    user_fields: Vec<(String, String)>,
    text_fields: Vec<String>,
}

#[napi]
impl Index {
    /// Create a new index at the given path.
    #[napi(factory)]
    pub fn create(path: String, fields: Vec<FieldDef>, stemmer: Option<String>) -> Result<Self> {
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
            stemmer,
        };

        let directory = StdFsDirectory::open(&path)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let handle = LucivyHandle::create(directory, &config)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&config);

        Ok(Self {
            handle,
            index_path: path,
            user_fields,
            text_fields,
        })
    }

    /// Open an existing index at the given path.
    #[napi(factory)]
    pub fn open(path: String) -> Result<Self> {
        let directory = StdFsDirectory::open(&path)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let handle = LucivyHandle::open(directory)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = match &handle.config {
            Some(config) => extract_user_fields(config),
            None => (Vec::new(), Vec::new()),
        };

        Ok(Self {
            handle,
            index_path: path,
            user_fields,
            text_fields,
        })
    }

    /// Add a document. `fields` is an object with field names as keys.
    #[napi]
    pub fn add(&self, doc_id: u32, fields: HashMap<String, serde_json::Value>) -> Result<()> {
        let mut doc = LucivyDocument::new();

        let nid_field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| Error::from_reason("no _node_id field in schema"))?;
        doc.add_u64(nid_field, doc_id as u64);

        add_fields_from_map(&self.handle, &mut doc, &fields)?;

        let writer = self.handle.writer.lock()
            .map_err(|_| Error::from_reason("writer lock poisoned"))?;
        writer.add_document(doc)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Add multiple documents at once.
    /// Each element must have a `docId` key and field values.
    #[napi]
    pub fn add_many(&self, docs: Vec<HashMap<String, serde_json::Value>>) -> Result<()> {
        let writer = self.handle.writer.lock()
            .map_err(|_| Error::from_reason("writer lock poisoned"))?;

        let nid_field = self.handle.field(NODE_ID_FIELD)
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
                add_field_value(&self.handle, &mut doc, key, value)?;
            }

            writer.add_document(doc)
                .map_err(|e| Error::from_reason(e.to_string()))?;
        }
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Delete a document by doc_id.
    #[napi]
    pub fn delete(&self, doc_id: u32) -> Result<()> {
        let field = self.handle.field(NODE_ID_FIELD)
            .ok_or_else(|| Error::from_reason("no _node_id field in schema"))?;
        let term = ld_lucivy::schema::Term::from_field_u64(field, doc_id as u64);
        let writer = self.handle.writer.lock()
            .map_err(|_| Error::from_reason("writer lock poisoned"))?;
        writer.delete_term(term);
        self.handle.mark_uncommitted();
        Ok(())
    }

    /// Update a document (delete + re-add).
    #[napi]
    pub fn update(&self, doc_id: u32, fields: HashMap<String, serde_json::Value>) -> Result<()> {
        self.delete(doc_id)?;
        self.add(doc_id, fields)?;
        Ok(())
    }

    /// Commit pending changes (makes added/deleted docs visible to searches).
    #[napi]
    pub fn commit(&self) -> Result<()> {
        let mut writer = self.handle.writer.lock()
            .map_err(|_| Error::from_reason("writer lock poisoned"))?;
        writer.commit()
            .map_err(|e| Error::from_reason(e.to_string()))?;
        self.handle.reader.reload()
            .map_err(|e| Error::from_reason(e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    /// Rollback pending changes.
    #[napi]
    pub fn rollback(&self) -> Result<()> {
        let mut writer = self.handle.writer.lock()
            .map_err(|_| Error::from_reason("writer lock poisoned"))?;
        writer.rollback()
            .map_err(|e| Error::from_reason(e.to_string()))?;
        self.handle.mark_committed();
        Ok(())
    }

    /// Search the index.
    /// `query` can be a string (contains_split on all text fields) or an object (QueryConfig).
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

        let lucivy_query = query::build_query(
            &query_config,
            &self.handle.schema,
            &self.handle.index,
            &self.handle.raw_field_pairs,
            &self.handle.ngram_field_pairs,
            highlight_sink.clone(),
        )
        .map_err(|e| Error::from_reason(e))?;

        let searcher = self.handle.reader.searcher();
        let top_docs = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().map(|id| id as u64).collect();
                execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set)?
            }
            None => execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?,
        };

        collect_results(
            &searcher,
            &top_docs,
            &self.handle.schema,
            highlight_sink.as_deref(),
            want_fields,
        )
    }

    /// Number of documents in the index.
    #[napi(getter)]
    pub fn num_docs(&self) -> u32 {
        self.handle.reader.searcher().num_docs() as u32
    }

    /// Index path.
    #[napi(getter)]
    pub fn path(&self) -> &str {
        &self.index_path
    }

    /// Export this index as a LUCE snapshot (Buffer).
    /// Throws if there are uncommitted changes.
    #[napi]
    pub fn export_snapshot(&self) -> Result<Buffer> {
        snapshot::check_committed(&self.handle, &self.index_path)
            .map_err(|e| Error::from_reason(e))?;

        let files = snapshot::read_directory_files(std::path::Path::new(&self.index_path))
            .map_err(|e| Error::from_reason(e))?;

        let idx = snapshot::SnapshotIndex {
            path: &self.index_path,
            files,
        };
        let blob = snapshot::export_snapshot(&[idx]);
        Ok(blob.into())
    }

    /// Export this index as a LUCE snapshot to a file.
    #[napi]
    pub fn export_snapshot_to(&self, path: String) -> Result<()> {
        snapshot::check_committed(&self.handle, &self.index_path)
            .map_err(|e| Error::from_reason(e))?;

        let files = snapshot::read_directory_files(std::path::Path::new(&self.index_path))
            .map_err(|e| Error::from_reason(e))?;

        let idx = snapshot::SnapshotIndex {
            path: &self.index_path,
            files,
        };
        let blob = snapshot::export_snapshot(&[idx]);
        std::fs::write(&path, &blob)
            .map_err(|e| Error::from_reason(format!("cannot write snapshot: {e}")))?;
        Ok(())
    }

    /// Import an index from a LUCE snapshot (Buffer).
    /// The snapshot must contain exactly one index.
    #[napi(factory)]
    pub fn import_snapshot(data: Buffer, dest_path: Option<String>) -> Result<Self> {
        let indexes = snapshot::import_snapshot(&data)
            .map_err(|e| Error::from_reason(e))?;

        if indexes.len() != 1 {
            return Err(Error::from_reason(format!(
                "expected 1 index in snapshot, got {}",
                indexes.len()
            )));
        }

        let imported = &indexes[0];
        let target = dest_path.as_deref().unwrap_or(&imported.path);

        write_imported_files(target, &imported.files)?;
        Self::open(target.to_string())
    }

    /// Import an index from a LUCE snapshot file.
    #[napi(factory)]
    pub fn import_snapshot_from(path: String, dest_path: Option<String>) -> Result<Self> {
        let data = std::fs::read(&path)
            .map_err(|e| Error::from_reason(format!("cannot read snapshot: {e}")))?;
        Self::import_snapshot(data.into(), dest_path)
    }

    /// Schema as a list of field definitions.
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
                Ok(build_contains_split_multi_field(s, &self.text_fields))
            }
            serde_json::Value::Object(_) => {
                let mut config: query::QueryConfig = serde_json::from_value(query.clone())
                    .map_err(|e| Error::from_reason(format!("invalid query object: {e}")))?;
                if config.query_type == "contains_split" {
                    config = expand_contains_split(&config);
                }
                Ok(config)
            }
            _ => Err(Error::from_reason(
                "query must be a string or an object",
            )),
        }
    }
}

// ─── Contains split helpers ────────────────────────────────────────────────

fn build_contains_split_multi_field(value: &str, text_fields: &[String]) -> query::QueryConfig {
    let words: Vec<&str> = value.split_whitespace().collect();

    if text_fields.len() == 1 {
        return expand_contains_split_for_field(value, &words, &text_fields[0]);
    }

    let word_queries: Vec<query::QueryConfig> = words
        .iter()
        .map(|word| {
            if text_fields.len() == 1 {
                query::QueryConfig {
                    query_type: "contains".into(),
                    field: Some(text_fields[0].clone()),
                    value: Some(word.to_string()),
                    ..Default::default()
                }
            } else {
                let field_queries: Vec<query::QueryConfig> = text_fields
                    .iter()
                    .map(|f| query::QueryConfig {
                        query_type: "contains".into(),
                        field: Some(f.clone()),
                        value: Some(word.to_string()),
                        ..Default::default()
                    })
                    .collect();
                query::QueryConfig {
                    query_type: "boolean".into(),
                    should: Some(field_queries),
                    ..Default::default()
                }
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
    expand_contains_split_for_field(value, &words, field)
}

fn expand_contains_split_for_field(
    value: &str,
    words: &[&str],
    field: &str,
) -> query::QueryConfig {
    if words.len() <= 1 {
        return query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(value.to_string()),
            ..Default::default()
        };
    }
    let should: Vec<query::QueryConfig> = words
        .iter()
        .map(|w| query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.to_string()),
            value: Some(w.to_string()),
            ..Default::default()
        })
        .collect();
    query::QueryConfig {
        query_type: "boolean".into(),
        should: Some(should),
        ..Default::default()
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
    handle: &LucivyHandle,
    doc: &mut LucivyDocument,
    fields: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    for (key, value) in fields {
        add_field_value(handle, doc, key, value)?;
    }
    Ok(())
}

fn add_field_value(
    handle: &LucivyHandle,
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
            auto_duplicate(handle, doc, field_name, text);
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

fn auto_duplicate(handle: &LucivyHandle, doc: &mut LucivyDocument, field_name: &str, text: &str) {
    if let Some(raw_name) = handle
        .raw_field_pairs
        .iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, raw)| raw.as_str())
    {
        if let Some(raw_field) = handle.field(raw_name) {
            doc.add_text(raw_field, text);
        }
    }
    if let Some(ngram_name) = handle
        .ngram_field_pairs
        .iter()
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
) -> Result<Vec<(f32, DocAddress)>> {
    let collector = TopDocs::with_limit(limit as usize).order_by_score();
    searcher
        .search(query, &collector)
        .map_err(|e| Error::from_reason(format!("search error: {e}")))
}

fn execute_top_docs_filtered(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
    allowed_ids: HashSet<u64>,
) -> Result<Vec<(f32, DocAddress)>> {
    let inner = TopDocs::with_limit(limit as usize).order_by_score();
    let collector = FilterCollector::new(
        NODE_ID_FIELD.to_string(),
        move |value: u64| allowed_ids.contains(&value),
        inner,
    );
    searcher
        .search(query, &collector)
        .map_err(|e| Error::from_reason(format!("filtered search error: {e}")))
}

fn write_imported_files(dest_path: &str, files: &[(String, Vec<u8>)]) -> Result<()> {
    std::fs::create_dir_all(dest_path)
        .map_err(|e| Error::from_reason(format!("cannot create directory '{}': {e}", dest_path)))?;
    for (name, data) in files {
        let file_path = std::path::Path::new(dest_path).join(name);
        std::fs::write(&file_path, data)
            .map_err(|e| Error::from_reason(format!("cannot write '{}': {e}", file_path.display())))?;
    }
    Ok(())
}

fn collect_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
    include_fields: bool,
) -> Result<Vec<SearchResult>> {
    let nid_field = schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| Error::from_reason("no _node_id field in schema"))?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher
            .doc(doc_addr)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let doc_id = doc
            .get_first(nid_field)
            .and_then(|v| v.as_value().as_u64())
            .unwrap_or(0);

        let highlights = highlight_sink.and_then(|sink| {
            let seg_id = searcher
                .segment_reader(doc_addr.segment_ord)
                .segment_id();
            let by_field = sink.get(seg_id, doc_addr.doc_id)?;
            let map: HashMap<String, Vec<Vec<u32>>> = by_field
                .into_iter()
                .filter(|(name, _)| {
                    !name.ends_with(RAW_SUFFIX) && !name.ends_with(NGRAM_SUFFIX)
                })
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
                let name = schema.get_field_name(field);
                if name == NODE_ID_FIELD || name.ends_with(RAW_SUFFIX) || name.ends_with(NGRAM_SUFFIX) {
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

        results.push(SearchResult {
            doc_id: doc_id as u32,
            score: score as f64,
            highlights,
            fields,
        });
    }
    Ok(results)
}
