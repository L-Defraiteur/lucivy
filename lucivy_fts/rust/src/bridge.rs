//! cxx bridge: typed Rust ↔ C++ interface for lucivy_fts.
//!
//! Uses ShardedHandle (unified handle, even for single-shard).
//! - Documents: typed structs (zero JSON on hot path)
//! - Search results: typed structs (node_id + score + highlights)
//! - Query + schema: still JSON (flexible, not hot path)

use std::collections::HashSet;
use std::sync::Arc;

use ld_lucivy::query::HighlightSink;
use ld_lucivy::schema::{Field, FieldType, Value as LucivyValue};
use ld_lucivy::LucivyDocument;

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query;
use lucivy_core::sharded_handle::ShardedSearchResult;

use crate::LucivyHandle;

/// JSON-deserializable shard version from the C++ side.
#[derive(serde::Deserialize)]
struct ClientShardVersion {
    shard_id: u32,
    version: String,
    segment_ids: Vec<String>,
}

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

    // ── Tier 2 / Tier 3: delta sync + distributed search ───────────────

    struct ShardVersionInfo {
        shard_id: u32,
        version: String,
        segment_ids: Vec<String>,
    }

    // ── Rust functions exposed to C++ ───────────────────────────────────

    extern "Rust" {
        type LucivyHandle;

        // Lifecycle
        fn create_index(path: &str, schema_json: &str) -> Result<Box<LucivyHandle>>;
        fn open_index(path: &str) -> Result<Box<LucivyHandle>>;
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

        // Tier 2: delta sync
        fn shard_versions(handle: &LucivyHandle) -> Result<Vec<ShardVersionInfo>>;
        fn export_sharded_delta(handle: &LucivyHandle, base_path: &str, client_versions_json: &str) -> Result<Vec<u8>>;
        fn apply_sharded_delta(handle: &LucivyHandle, base_path: &str, data: &[u8]) -> Result<()>;

        // Tier 3: distributed search
        fn export_stats_json(handle: &LucivyHandle, query_json: &str) -> Result<String>;
        fn search_with_global_stats_json(
            handle: &LucivyHandle,
            query_json: &str,
            global_stats_json: &str,
            limit: u32,
        ) -> Result<Vec<SearchResult>>;
    }
}

// ── Lifecycle ──────────────────────────────────���───────────────────────────

fn create_index(path: &str, schema_json: &str) -> Result<Box<LucivyHandle>, String> {
    let config: query::SchemaConfig = serde_json::from_str(schema_json)
        .map_err(|e| format!("invalid schema JSON: {e}"))?;
    let handle = lucivy_core::sharded_handle::ShardedHandle::create(path, &config)?;
    Ok(Box::new(LucivyHandle(handle)))
}

fn open_index(path: &str) -> Result<Box<LucivyHandle>, String> {
    let handle = lucivy_core::sharded_handle::ShardedHandle::open(path)?;
    Ok(Box::new(LucivyHandle(handle)))
}

fn close_index(handle: &LucivyHandle) -> Result<(), String> {
    handle.close()
}

// ── Schema introspection ────���──────────────────────────────────────────────

fn get_field_ids(handle: &LucivyHandle) -> Vec<ffi::IndexFieldInfo> {
    handle
        .field_map
        .iter()
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

// ── Document operations ──────��─────────────────────────────────────────────

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
    }

    handle.add_document(doc, node_id)?;
    Ok(0)
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

    handle.add_document(doc, node_id)?;
    Ok(0)
}

fn delete_by_node_id(handle: &LucivyHandle, node_id: u64) -> Result<i64, String> {
    handle.delete_by_node_id(node_id)?;
    Ok(0)
}

// ── Transaction ────────────────────────────────────────────────────────────

fn commit(handle: &LucivyHandle) -> Result<i64, String> {
    handle.commit()?;
    Ok(0)
}

fn rollback(_handle: &LucivyHandle) {
    // ShardedHandle does not support rollback — no-op for backward compat.
}

fn reload_reader(_handle: &LucivyHandle) {
    // ShardedHandle commit() already reloads readers internally.
}

// ── Search ─────────────────────────���───────────────────────────────────────

fn search(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
) -> Result<Vec<ffi::SearchResult>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let results = handle.search(&config, limit as usize, None)?;
    collect_search_results(handle, &results)
}

fn search_with_highlights(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let sink = Arc::new(HighlightSink::new());
    let results = handle.search(&config, limit as usize, Some(sink.clone()))?;
    collect_search_results_with_highlights(handle, &results, Some(&sink))
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
    let sink = Arc::new(HighlightSink::new());
    let results = handle.search(&config, limit as usize, Some(sink.clone()))?;
    collect_search_results_with_highlights(handle, &results, Some(&sink))
}

fn search_filtered(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
    allowed_ids: &[u64],
) -> Result<Vec<ffi::SearchResult>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
    let results = handle.search_filtered(&config, limit as usize, None, id_set)?;
    collect_search_results(handle, &results)
}

fn search_filtered_with_highlights(
    handle: &LucivyHandle,
    query_json: &str,
    limit: u32,
    allowed_ids: &[u64],
) -> Result<Vec<ffi::SearchResultWithHighlights>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let sink = Arc::new(HighlightSink::new());
    let id_set: HashSet<u64> = allowed_ids.iter().copied().collect();
    let results = handle.search_filtered(&config, limit as usize, Some(sink.clone()), id_set)?;
    collect_search_results_with_highlights(handle, &results, Some(&sink))
}

// ── Typed query config builder ─���───────────────────────────────────────────

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

// ── Info ───────────────────────────────────────────────────────────────────

fn num_docs(handle: &LucivyHandle) -> u64 {
    handle.num_docs()
}

fn get_schema_json(handle: &LucivyHandle) -> String {
    serde_json::to_string(&handle.config).unwrap_or_default()
}

// ── Tier 2: delta sync ────────────────────────────────────────────────────

fn shard_versions(handle: &LucivyHandle) -> Result<Vec<ffi::ShardVersionInfo>, String> {
    let versions = handle.shard_versions()?;
    Ok(versions
        .into_iter()
        .map(|v| ffi::ShardVersionInfo {
            shard_id: v.shard_id as u32,
            version: v.version,
            segment_ids: v.segment_ids.into_iter().collect(),
        })
        .collect())
}

fn export_sharded_delta(
    handle: &LucivyHandle,
    base_path: &str,
    client_versions_json: &str,
) -> Result<Vec<u8>, String> {
    let client_versions: Vec<ClientShardVersion> =
        serde_json::from_str(client_versions_json)
            .map_err(|e| format!("invalid client_versions JSON: {e}"))?;

    let versions: Vec<lucistore::delta_sharded::ShardVersion> = client_versions
        .into_iter()
        .map(|v| lucistore::delta_sharded::ShardVersion {
            shard_id: v.shard_id as usize,
            version: v.version,
            segment_ids: v.segment_ids.into_iter().collect(),
        })
        .collect();

    handle.export_sharded_delta(base_path, &versions)
}

fn apply_sharded_delta(
    handle: &LucivyHandle,
    base_path: &str,
    data: &[u8],
) -> Result<(), String> {
    handle.apply_sharded_delta(base_path, data)
}

// ── Tier 3: distributed search ───────────────────────────────────────────

fn export_stats_json(
    handle: &LucivyHandle,
    query_json: &str,
) -> Result<String, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let stats = handle.export_stats(&config)?;
    serde_json::to_string(&stats)
        .map_err(|e| format!("failed to serialize stats: {e}"))
}

fn search_with_global_stats_json(
    handle: &LucivyHandle,
    query_json: &str,
    global_stats_json: &str,
    limit: u32,
) -> Result<Vec<ffi::SearchResult>, String> {
    let config: query::QueryConfig = serde_json::from_str(query_json)
        .map_err(|e| format!("invalid query JSON: {e}"))?;
    let global_stats: lucivy_core::bm25_global::ExportableStats =
        serde_json::from_str(global_stats_json)
            .map_err(|e| format!("invalid global_stats JSON: {e}"))?;
    let results = handle.search_with_global_stats(&config, limit as usize, &global_stats, None)?;
    collect_search_results(handle, &results)
}

// ── Internal helpers ────��──────────────────────────────────────────────────

fn collect_search_results(
    handle: &LucivyHandle,
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
        let doc: LucivyDocument = searcher.doc(r.doc_address).map_err(|e| e.to_string())?;
        let node_id = extract_node_id(&doc, nid_field);
        out.push(ffi::SearchResult { node_id, score: r.score });
    }
    Ok(out)
}

fn collect_search_results_with_highlights(
    handle: &LucivyHandle,
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
        let doc: LucivyDocument = searcher.doc(r.doc_address).map_err(|e| e.to_string())?;
        let node_id = extract_node_id(&doc, nid_field);

        let highlights = highlight_sink
            .and_then(|sink| {
                let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
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
                if entries.is_empty() { None } else { Some(entries) }
            })
            .unwrap_or_default();

        out.push(ffi::SearchResultWithHighlights {
            node_id,
            score: r.score,
            highlights,
        });
    }
    Ok(out)
}

fn extract_node_id(doc: &LucivyDocument, nid_field: ld_lucivy::schema::Field) -> u64 {
    doc.get_first(nid_field)
        .and_then(|v| v.as_value().as_u64())
        .unwrap_or(0)
}
