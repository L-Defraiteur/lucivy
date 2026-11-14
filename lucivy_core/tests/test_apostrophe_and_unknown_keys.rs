//! Two silent misses of a query (tickets of 7 November 2026, branch
//! `tickets-2026-11-07`): the typographic apostrophe, and the keys a query
//! accepts without doing anything with them.

mod common;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

/// The index and the scratch directory it lives in (the guard outlives the handle).
fn index(name: &str) -> (tempfile::TempDir, ShardedHandle) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": 2
    }))
    .unwrap();
    let scratch = common::scratch(name);
    let handle = ShardedHandle::create(scratch.path().to_str().unwrap(), &config).unwrap();
    for (id, text) in [
        (1u64, "## Vue d'ensemble\nLe module en bref."),
        (2, "## Vue d’ensemble\nApostrophe typographique."),
        (3, "Une vue d'un ensemble de fichiers"),
    ] {
        handle.add_document_json(id, &serde_json::json!({"content": text})).unwrap();
    }
    handle.commit().unwrap();
    (scratch, handle)
}

fn ids(handle: &ShardedHandle, q: serde_json::Value) -> Vec<u64> {
    let q: QueryConfig = serde_json::from_value(q).unwrap();
    let mut ids = handle.node_ids_of(&handle.search(&q, 10, None).unwrap()).unwrap();
    ids.sort_unstable();
    ids
}

/// « Vue d'ensemble » and « Vue d’ensemble » find each other: the straight
/// apostrophe is a separator, the typographic one content, and a text written
/// with one was not found by the other, even at distance 1.
#[test]
fn both_apostrophes_find_each_other() {
    let (_scratch, handle) = index("lucivy_apostrophes");
    for value in ["Vue d'ensemble", "Vue d’ensemble", "vue d‘ensemble", "VUE Dʼensemble"] {
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"strict_separators": true}),
            serde_json::json!({"distance": 1}),
        ] {
            let mut q = serde_json::json!({"type": "contains", "field": "content", "value": value});
            q.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let found = ids(&handle, q.clone());
            assert_eq!(found, vec![1, 2], "{q}");
        }
    }
    // The words of a `parse` and a `contains_split` too.
    let found = ids(&handle, serde_json::json!({"type": "parse", "field": "content", "value": "\"Vue d’ensemble\""}));
    assert_eq!(found, vec![1, 2]);
}

/// `boost`, `offset`, `slop` or a typo: the search ran without them, and
/// `query_warnings` says so instead of letting the caller believe they applied.
#[test]
fn unknown_query_keys_are_named_by_the_warnings() {
    let (_scratch, handle) = index("lucivy_unknown_keys");
    let q: QueryConfig = serde_json::from_value(serde_json::json!({
        "type": "contains", "field": "content", "value": "ensemble",
        "boost": 2.0, "offset": 10, "slop": 1, "feild": "content", "terms": ["a"]
    }))
    .unwrap();
    let warnings = handle.query_warnings(&q);
    for key in ["`boost`", "`offset`", "`slop`", "`feild`", "`terms`"] {
        assert!(warnings.iter().any(|w| w.contains(key)), "no warning names {key}: {warnings:?}");
    }
    // A clean query has none of them.
    let clean: QueryConfig = serde_json::from_value(serde_json::json!({
        "type": "contains", "field": "content", "value": "ensemble"
    }))
    .unwrap();
    assert!(handle.query_warnings(&clean).iter().all(|w| !w.contains("ignored")), "{:?}", handle.query_warnings(&clean));
}
