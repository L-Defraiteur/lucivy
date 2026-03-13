//! cxx bridge: typed Rust ↔ C++ interface for lucivy_fts.
//!
//! Replaces the extern "C" + JSON approach with typed structs and automatic ownership.
//! - Documents: typed structs (zero JSON on hot path)
//! - Search results: typed structs (node_id + score + highlights)
//! - Query + schema: still JSON (flexible, not hot path)
//! - Ownership: automatic via Box<LucivyHandle> + String + Vec

use std::collections::HashSet;
use std::sync::Arc;

use ld_lucivy::collector::{FilterCollector, TopDocs};
use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{Field, FieldType, Value as LucivyValue};
use ld_lucivy::{DocAddress, Searcher, LucivyDocument};

use lucivy_core::handle::{NGRAM_SUFFIX, NODE_ID_FIELD, RAW_SUFFIX};
use lucivy_core::query;

use crate::LucivyHandle;

#[cxx::bridge]
mod ffi {
    // ── Shared structs (visible from both Rust and C++) ──────────────────

    struct DocFieldText {
        field_id: u32,
        value: String,
    }

    struct DocFieldU64 {
        field_id: u32,
        value: u64,
    }

    struct DocFieldI64 {
        field_id: u32,
        value: i64,
    }

    struct DocFieldF64 {
        field_id: u32,
        value: f64,
    }

    struct SearchResult {
        node_id: u64,
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
        node_id: u64,
        score: f32,
        highlights: Vec<FieldHighlights>,
    }

    struct IndexFieldInfo {
        field_id: u32,
        name: String,
        field_type: String,
    }

    // ── Rust functions exposed to C++ ────────────────────────────────────

    extern "Rust" {
        type LucivyHandle;

        // Lifecycle
        fn create_index(path: &str, schema_json: &str) -> Result<Box<LucivyHandle>>;
        fn open_index(path: &str) -> Result<Box<LucivyHandle>>;
        // close = drop of Box<LucivyHandle> (automatic)
        fn close_index(handle: &LucivyHandle) -> Result<()>;

        // Schema introspection
        fn get_field_ids(handle: &LucivyHandle) -> Vec<IndexFieldInfo>;

        // Document operations (hot path — typed, zero JSON)
        fn add_document_texts(
            handle: &LucivyHandle,
            node_id: u64,
            fields: &[DocFieldText],
        ) -> Result<i64>;

        fn add_document_mixed(
            handle: &LucivyHandle,
            node_id: u64,
            text_fields: &[DocFieldText],
            u64_fields: &[DocFieldU64],
            i64_fields: &[DocFieldI64],
            f64_fields: &[DocFieldF64],
        ) -> Result<i64>;

        fn delete_by_node_id(handle: &LucivyHandle, node_id: u64) -> Result<i64>;

        // Transaction
        fn commit(handle: &LucivyHandle) -> Result<i64>;
        fn rollback(handle: &LucivyHandle);
        fn reload_reader(handle: &LucivyHandle);

        // Search (query stays JSON — flexible, not a hot path)
        fn search(
            handle: &LucivyHandle,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;

        fn search_with_highlights(
            handle: &LucivyHandle,
            query_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResultWithHighlights>>;

        /// Typed search — bypasses JSON serialization/deserialization.
        /// Modes: "contains", "contains_split", "fuzzy", "regex", "parse".
        fn search_typed_with_highlights(
            handle: &LucivyHandle,
            field: &str,
            value: &str,
            mode: &str,
            distance: u8,
            limit: u32,
        ) -> Result<Vec<SearchResultWithHighlights>>;

        fn search_filtered(
            handle: &LucivyHandle,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResult>>;

        fn search_filtered_with_highlights(
            handle: &LucivyHandle,
            query_json: &str,
            limit: u32,
            allowed_ids: &[u64],
        ) -> Result<Vec<SearchResultWithHighlights>>;

        // Info
        fn num_docs(handle: &LucivyHandle) -> u64;
        fn get_schema_json(handle: &LucivyHandle) -> String;
    }
}

// ── Lifecycle ──────────────────────────────────────────────────────────────

fn create_index(path: &str, schema_json: &str) -> Result<Box<LucivyHandle>, String> {
    let config: query::SchemaConfig = serde_json::from_str(schema_json)
        .map_err(|e| format!("invalid schema JSON: {e}"))?;
    let directory = lucivy_core::directory::StdFsDirectory::open(path)
        .map_err(|e| format!("cannot open directory: {e}"))?;
    let inner = lucivy_core::handle::LucivyHandle::create(directory, &config)?;
    Ok(Box::new(LucivyHandle(inner)))
}

fn open_index(path: &str) -> Result<Box<LucivyHandle>, String> {
    let directory = lucivy_core::directory::StdFsDirectory::open(path)
        .map_err(|e| format!("cannot open directory: {e}"))?;
    let inner = lucivy_core::handle::LucivyHandle::open(directory)?;
    Ok(Box::new(LucivyHandle(inner)))
}

fn close_index(handle: &LucivyHandle) -> Result<(), String> {
    handle.0.close()
}

// ── Schema introspection ───────────────────────────────────────────────────

fn get_field_ids(handle: &LucivyHandle) -> Vec<ffi::IndexFieldInfo> {
    handle
        .field_map
        .iter()
        .filter(|(name, _)| !name.ends_with(RAW_SUFFIX) && !name.ends_with(NGRAM_SUFFIX))
        .map(|(name, field)| {
            let ft = match handle.schema.get_field_entry(*field).field_type() {
                FieldType::Str(_) => "text",
                FieldType::U64(_) => "u64",
                FieldType::I64(_) => "i64",
                FieldType::F64(_) => "f64",
                _ => "unknown",
            };
            ffi::IndexFieldInfo {
                field_id: field.field_id(),
                name: name.clone(),
                field_type: ft.to_string(),
            }
        })
        .collect()
}

// ── Document operations ────────────────────────────────────────────────────

fn add_document_texts(
    handle: &LucivyHandle,
    node_id: u64,
    fields: &[ffi::DocFieldText],
) -> Result<i64, String> {
    let mut doc = LucivyDocument::new();

    let nid_field = handle
        .field(NODE_ID_FIELD)
        .ok_or("no _node_id field in schema")?;
    doc.add_u64(nid_field, node_id);

    for f in fields {
        let field = Field::from_field_id(f.field_id);
        doc.add_text(field, &f.value);
        let field_name = handle.schema.get_field_entry(field).name().to_owned();
        auto_duplicate_field(&mut doc, handle, &field_name, &f.value);
    }

    let mut guard = handle
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;
    let writer = guard.as_mut().ok_or("index is closed")?;
    writer
        .add_document(doc)
        .map(|o| o as i64)
        .map_err(|e| e.to_string())
}

fn add_document_mixed(
    handle: &LucivyHandle,
    node_id: u64,
    text_fields: &[ffi::DocFieldText],
    u64_fields: &[ffi::DocFieldU64],
    i64_fields: &[ffi::DocFieldI64],
    f64_fields: &[ffi::DocFieldF64],
) -> Result<i64, String> {
    let mut doc = LucivyDocument::new();

    let nid_field = handle
        .field(NODE_ID_FIELD)
        .ok_or("no _node_id field in schema")?;
    doc.add_u64(nid_field, node_id);

    for f in text_fields {
        let field = Field::from_field_id(f.field_id);
        doc.add_text(field, &f.value);
        let field_name = handle.schema.get_field_entry(field).name().to_owned();
        auto_duplicate_field(&mut doc, handle, &field_name, &f.value);
    }

    for f in u64_fields {
        doc.add_u64(Field::from_field_id(f.field_id), f.value);
    }
    for f in i64_fields {
        doc.add_i64(Field::from_field_id(f.field_id), f.value);
    }
    for f in f64_fields {
        doc.add_f64(Field::from_field_id(f.field_id), f.value);
    }

    let mut guard = handle
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;
    let writer = guard.as_mut().ok_or("index is closed")?;
    writer
        .add_document(doc)
        .map(|o| o as i64)
        .map_err(|e| e.to_string())
}

/// Auto-duplicate a text value into ._raw and ._ngram counterparts if they exist.
fn auto_duplicate_field(
    doc: &mut LucivyDocument,
    handle: &LucivyHandle,
    field_name: &str,
    value: &str,
) {
    if let Some(raw_name) = handle
        .raw_field_pairs
        .iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, raw)| raw.as_str())
    {
        if let Some(raw_field) = handle.field(raw_name) {
            doc.add_text(raw_field, value);
        }
    }

    if let Some(ngram_name) = handle
        .ngram_field_pairs
        .iter()
        .find(|(user, _)| user == field_name)
        .map(|(_, ngram)| ngram.as_str())
    {
        if let Some(ngram_field) = handle.field(ngram_name) {
            doc.add_text(ngram_field, value);
        }
    }
}

fn delete_by_node_id(handle: &LucivyHandle, node_id: u64) -> Result<i64, String> {
    let field = handle
        .field(NODE_ID_FIELD)
        .ok_or("no _node_id field in schema")?;
    let term = ld_lucivy::schema::Term::from_field_u64(field, node_id);
    let mut guard = handle
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;
    let writer = guard.as_mut().ok_or("index is closed")?;
    Ok(writer.delete_term(term) as i64)
}

// ── Transaction ────────────────────────────────────────────────────────────

fn commit(handle: &LucivyHandle) -> Result<i64, String> {
    let mut guard = handle
        .writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;
    let writer = guard.as_mut().ok_or("index is closed")?;
    writer
        .commit()
        .map(|o| o as i64)
        .map_err(|e| e.to_string())
}

fn rollback(handle: &LucivyHandle) {
    if let Ok(mut guard) = handle.writer.lock() {
        if let Some(writer) = guard.as_mut() {
            let _ = writer.rollback();
        }
    }
}

fn reload_reader(handle: &LucivyHandle) {
    if let Err(e) = handle.reader.reload() {
        eprintln!("reload_reader: {e}");
    }
}

// ── Search ─────────────────────────────────────────────────────────────────

fn search(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
) -> Result<Vec<ffi::SearchResult>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;

    let lucivy_query = query::build_query(
        &config,
        &handle.schema,
        &handle.index,
        &handle.raw_field_pairs,
        &handle.ngram_field_pairs,
        None,
    )?;

    let searcher = handle.reader.searcher();
    let top_docs = execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?;
    collect_search_results(&searcher, &top_docs, &handle.schema)
}

fn search_with_highlights(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;

    let highlight_sink = Arc::new(HighlightSink::new());

    let lucivy_query = query::build_query(
        &config,
        &handle.schema,
        &handle.index,
        &handle.raw_field_pairs,
        &handle.ngram_field_pairs,
        Some(highlight_sink.clone()),
    )?;

    let searcher = handle.reader.searcher();
    let top_docs = execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?;
    collect_search_results_with_highlights(
        &searcher,
        &top_docs,
        &handle.schema,
        Some(&highlight_sink),
    )
}

// ── Typed search (no JSON) ───────────────────────────────────────────────

fn build_typed_query_config(
    field: &str,
    value: &str,
    mode: &str,
    distance: u8,
) -> Result<query::QueryConfig, String> {
    match mode {
        "contains" => Ok(query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            ..Default::default()
        }),
        "contains_split" => Ok(query::QueryConfig {
            query_type: "contains_split".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            distance: if distance > 0 { Some(distance) } else { None },
            ..Default::default()
        }),
        "startsWith" => Ok(query::QueryConfig {
            query_type: "startsWith".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            distance: if distance > 0 { Some(distance) } else { None },
            ..Default::default()
        }),
        "startsWith_split" => Ok(query::QueryConfig {
            query_type: "startsWith_split".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            distance: if distance > 0 { Some(distance) } else { None },
            ..Default::default()
        }),
        "fuzzy" => Ok(query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            distance: Some(distance),
            ..Default::default()
        }),
        "regex" => Ok(query::QueryConfig {
            query_type: "contains".into(),
            field: Some(field.into()),
            value: Some(value.into()),
            regex: Some(true),
            ..Default::default()
        }),
        "parse" => Ok(query::QueryConfig {
            query_type: "parse".into(),
            fields: Some(vec![field.into()]),
            value: Some(value.into()),
            ..Default::default()
        }),
        other => Err(format!(
            "unknown search mode: {other}. Valid: contains, contains_split, startsWith, startsWith_split, fuzzy, regex, parse"
        )),
    }
}

fn search_typed_with_highlights(
    handle: &LucivyHandle,
    field: &str,
    value: &str,
    mode: &str,
    distance: u8,
    limit: u32,
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let config = build_typed_query_config(field, value, mode, distance)?;

    let highlight_sink = Arc::new(HighlightSink::new());

    let lucivy_query = query::build_query(
        &config,
        &handle.schema,
        &handle.index,
        &handle.raw_field_pairs,
        &handle.ngram_field_pairs,
        Some(highlight_sink.clone()),
    )?;

    let searcher = handle.reader.searcher();
    let top_docs = execute_top_docs(&searcher, lucivy_query.as_ref(), limit)?;
    collect_search_results_with_highlights(
        &searcher,
        &top_docs,
        &handle.schema,
        Some(&highlight_sink),
    )
}

// ── Filtered search ─────────────────────────────────────────────────────

fn search_filtered(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
    allowed_ids: &[u64],
) -> Result<Vec<ffi::SearchResult>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;

    let lucivy_query = query::build_query(
        &config,
        &handle.schema,
        &handle.index,
        &handle.raw_field_pairs,
        &handle.ngram_field_pairs,
        None,
    )?;

    let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
    let searcher = handle.reader.searcher();
    let top_docs = execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set)?;
    collect_search_results(&searcher, &top_docs, &handle.schema)
}

fn search_filtered_with_highlights(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
    allowed_ids: &[u64],
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;

    let highlight_sink = Arc::new(HighlightSink::new());

    let lucivy_query = query::build_query(
        &config,
        &handle.schema,
        &handle.index,
        &handle.raw_field_pairs,
        &handle.ngram_field_pairs,
        Some(highlight_sink.clone()),
    )?;

    let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
    let searcher = handle.reader.searcher();
    let top_docs = execute_top_docs_filtered(&searcher, lucivy_query.as_ref(), limit, id_set)?;
    collect_search_results_with_highlights(
        &searcher,
        &top_docs,
        &handle.schema,
        Some(&highlight_sink),
    )
}

// ── Info ───────────────────────────────────────────────────────────────────

fn num_docs(handle: &LucivyHandle) -> u64 {
    handle.reader.searcher().num_docs()
}

fn get_schema_json(handle: &LucivyHandle) -> String {
    serde_json::to_string(&handle.schema).unwrap_or_default()
}

// ── Internal helpers ───────────────────────────────────────────────────────

fn execute_top_docs(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let collector = TopDocs::with_limit(limit as usize).order_by_score();
    searcher
        .search(query, &collector)
        .map_err(|e| format!("search error: {e}"))
}

fn execute_top_docs_filtered(
    searcher: &Searcher,
    query: &dyn ld_lucivy::query::Query,
    limit: u32,
    allowed_ids: HashSet<u64>,
) -> Result<Vec<(f32, DocAddress)>, String> {
    let inner = TopDocs::with_limit(limit as usize).order_by_score();
    let collector = FilterCollector::new(
        NODE_ID_FIELD.to_string(),
        move |value: u64| allowed_ids.contains(&value),
        inner,
    );
    searcher
        .search(query, &collector)
        .map_err(|e| format!("filtered search error: {e}"))
}

fn collect_search_results(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
) -> Result<Vec<ffi::SearchResult>, String> {
    let nid_field = schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| "no _node_id field in schema")?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher.doc(doc_addr).map_err(|e| e.to_string())?;
        let node_id = extract_node_id(&doc, nid_field);
        results.push(ffi::SearchResult { node_id, score });
    }
    Ok(results)
}

fn collect_search_results_with_highlights(
    searcher: &Searcher,
    top_docs: &[(f32, DocAddress)],
    schema: &ld_lucivy::schema::Schema,
    highlight_sink: Option<&HighlightSink>,
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let nid_field = schema
        .get_field(NODE_ID_FIELD)
        .map_err(|_| "no _node_id field in schema")?;

    let mut results = Vec::with_capacity(top_docs.len());
    for &(score, doc_addr) in top_docs {
        let doc: LucivyDocument = searcher.doc(doc_addr).map_err(|e| e.to_string())?;
        let node_id = extract_node_id(&doc, nid_field);

        let highlights = highlight_sink
            .and_then(|sink| {
                let seg_id = searcher.segment_reader(doc_addr.segment_ord).segment_id();
                let by_field = sink.get(seg_id, doc_addr.doc_id)?;
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
                if entries.is_empty() { None } else { Some(entries) }
            })
            .unwrap_or_default();

        results.push(ffi::SearchResultWithHighlights {
            node_id,
            score,
            highlights,
        });
    }
    Ok(results)
}

fn extract_node_id(doc: &LucivyDocument, nid_field: Field) -> u64 {
    doc.get_first(nid_field)
        .and_then(|v| v.as_value().as_u64())
        .unwrap_or(0)
}
