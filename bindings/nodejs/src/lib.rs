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
    handle: ShardedHandle,
    index_path: String,
    user_fields: Vec<(String, String)>,
    text_fields: Vec<String>,
}

#[napi]
impl Index {
    /// Create a new index at the given path.
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
            handle,
            index_path: path,
            user_fields,
            text_fields,
        })
    }

    /// Open an existing index at the given path.
    #[napi(factory)]
    pub fn open(path: String) -> Result<Self> {
        let handle = ShardedHandle::open(&path)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

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

        self.handle.add_document(doc, doc_id as u64)
            .map_err(|e| Error::from_reason(e))
    }

    /// Add multiple documents at once.
    /// Each element must have a `docId` key and field values.
    #[napi]
    pub fn add_many(&self, docs: Vec<HashMap<String, serde_json::Value>>) -> Result<()> {
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

            self.handle.add_document(doc, doc_id)
                .map_err(|e| Error::from_reason(e))?;
        }
        Ok(())
    }

    /// Delete a document by doc_id.
    #[napi]
    pub fn delete(&self, doc_id: u32) -> Result<()> {
        self.handle.delete_by_node_id(doc_id as u64)
            .map_err(|e| Error::from_reason(e))
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
        self.handle.commit()
            .map_err(|e| Error::from_reason(e))
    }

    /// Close the index: commit pending writes and release the writer lock.
    #[napi]
    pub fn close(&self) -> Result<()> {
        self.handle.close()
            .map_err(|e| Error::from_reason(e))
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

        let results = match allowed_ids {
            Some(ids) => {
                let id_set: HashSet<u64> = ids.into_iter().map(|id| id as u64).collect();
                self.handle.search_filtered(&query_config, limit as usize, highlight_sink.clone(), id_set)
                    .map_err(|e| Error::from_reason(e))?
            }
            None => self.handle.search(&query_config, limit as usize, highlight_sink.clone())
                .map_err(|e| Error::from_reason(e))?,
        };

        collect_sharded_results(
            &self.handle,
            &results,
            highlight_sink.as_deref(),
            want_fields,
        )
    }

    /// Number of documents in the index.
    #[napi(getter)]
    pub fn num_docs(&self) -> u32 {
        self.handle.num_docs() as u32
    }

    /// Number of shards.
    #[napi(getter)]
    pub fn num_shards(&self) -> u32 {
        self.handle.num_shards() as u32
    }

    /// Index path.
    #[napi(getter)]
    pub fn path(&self) -> &str {
        &self.index_path
    }

    /// Export this index as a LUCE snapshot (Buffer).
    #[napi]
    pub fn export_snapshot(&self) -> Result<Buffer> {
        let blob = snapshot::export_to_snapshot(
            &self.handle,
            std::path::Path::new(&self.index_path),
        ).map_err(|e| Error::from_reason(e))?;
        Ok(blob.into())
    }

    /// Export this index as a LUCE snapshot to a file.
    #[napi]
    pub fn export_snapshot_to(&self, path: String) -> Result<()> {
        let blob = snapshot::export_to_snapshot(
            &self.handle,
            std::path::Path::new(&self.index_path),
        ).map_err(|e| Error::from_reason(e))?;
        std::fs::write(&path, &blob)
            .map_err(|e| Error::from_reason(format!("cannot write snapshot: {e}")))?;
        Ok(())
    }

    /// Import an index from a LUCE snapshot (Buffer).
    /// The snapshot must contain exactly one index.
    #[napi(factory)]
    pub fn import_snapshot(data: Buffer, dest_path: Option<String>) -> Result<Self> {
        let dest = dest_path.as_deref().unwrap_or("/tmp/lucivy_import");
        let dest_p = std::path::Path::new(dest);
        let handle = snapshot::import_from_snapshot(&data, dest_p)
            .map_err(|e| Error::from_reason(e))?;

        let (user_fields, text_fields) = extract_user_fields(&handle.config);

        Ok(Self {
            handle,
            index_path: dest.to_string(),
            user_fields,
            text_fields,
        })
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
