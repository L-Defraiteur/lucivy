//! Test: ShardedHandle search on a cold scheduler (first index in process).
//!
//! This test runs as a separate binary — the global scheduler is cold.
//! Reproduces the bug where RR-4sh returns 0 hits when it's the first index.

use lucivy_core::handle::NODE_ID_FIELD;
use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

fn tmp_dir(name: &str) -> String {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.to_str().unwrap().to_string()
}

#[test]
fn test_cold_rr_search() {
    let dir = tmp_dir("lucivy_cold_rr");
    let config: lucivy_core::query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "content", "type": "text", "stored": true}
        ],
        "shards": 4,
        "balance_weight": 1.0
    })).unwrap();

    let handle = ShardedHandle::create(&dir, &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid = handle.field(NODE_ID_FIELD).unwrap();

    for i in 0u64..100 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(content_f, &format!("function test_{i}() {{ println!(\"hello\"); }}"));
        handle.add_document(doc, i).unwrap();
    }

    handle.commit().unwrap();
    assert_eq!(handle.num_docs(), 100);

    let query: QueryConfig = serde_json::from_str(
        r#"{"type": "contains", "field": "content", "value": "function"}"#
    ).unwrap();
    let results = handle.search(&query, 20, None).unwrap();
    eprintln!("cold_rr: {} results for 'function'", results.len());
    assert!(results.len() > 0, "cold scheduler RR should find docs with 'function'");
}

#[test]
fn test_cold_ta_search() {
    let dir = tmp_dir("lucivy_cold_ta");
    let config: lucivy_core::query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "content", "type": "text", "stored": true}
        ],
        "shards": 4,
        "balance_weight": 0.2
    })).unwrap();

    let handle = ShardedHandle::create(&dir, &config).unwrap();
    let content_f = handle.field("content").unwrap();
    let nid = handle.field(NODE_ID_FIELD).unwrap();

    for i in 0u64..100 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(content_f, &format!("function test_{i}() {{ println!(\"hello\"); }}"));
        handle.add_document(doc, i).unwrap();
    }

    handle.commit().unwrap();
    assert_eq!(handle.num_docs(), 100);

    let query: QueryConfig = serde_json::from_str(
        r#"{"type": "contains", "field": "content", "value": "function"}"#
    ).unwrap();
    let results = handle.search(&query, 20, None).unwrap();
    eprintln!("cold_ta: {} results for 'function'", results.len());
    assert!(results.len() > 0, "cold scheduler TA should find docs with 'function'");
}

#[test]
fn test_cold_rr_two_fields() {
    let dir = tmp_dir("lucivy_cold_rr_2f");
    let config: lucivy_core::query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ],
        "shards": 4,
        "balance_weight": 1.0
    })).unwrap();

    let handle = ShardedHandle::create(&dir, &config).unwrap();
    let path_f = handle.field("path").unwrap();
    let content_f = handle.field("content").unwrap();
    let nid = handle.field(NODE_ID_FIELD).unwrap();

    for i in 0u64..100 {
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_u64(nid, i);
        doc.add_text(path_f, &format!("src/file_{i}.cpp"));
        doc.add_text(content_f, &format!("void function_{}() {{ return; }}", i));
        handle.add_document(doc, i).unwrap();
    }

    handle.commit().unwrap();
    assert_eq!(handle.num_docs(), 100);

    let query: QueryConfig = serde_json::from_str(
        r#"{"type": "contains", "field": "content", "value": "function"}"#
    ).unwrap();
    let results = handle.search(&query, 20, None).unwrap();
    eprintln!("cold_rr_2f: {} results", results.len());
    assert!(results.len() > 0, "should find docs with 'function' in content");
}
