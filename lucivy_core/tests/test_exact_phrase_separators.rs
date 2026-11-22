//! **An exact `contains` of several words finds them across any separator, or none** (22 November 2026, for
//! rag3weaver's whole-query pass at distance 0: « le journal des chantiers » must find `docs/journal-des-chantiers.md`).
//! The default `contains` relaxes separators — `_`, `-`, `.` and spaces are ignored on both sides, so their absence too:
//! « journal des chantiers » finds « journaldeschantiers », as « spin lock » finds `spinlock` (the README).
//! `strict_separators` matches them byte for byte.

mod common;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

#[test]
fn an_exact_contains_of_words_matches_across_separators() {
    let dir = common::scratch("exact-phrase-separators");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 1 })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    for (i, t) in ["docs/journal-des-chantiers.md", "le journal des chantiers", "journal_des_chantiers()", "un journal de bord", "journaldeschantiers"].iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "content": t })).unwrap();
    }
    index.commit().unwrap();
    let found = |strict: bool| -> Vec<u64> {
        let mut q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "field": "content", "value": "journal des chantiers", "distance": 0})).unwrap();
        if strict {
            q = serde_json::from_value(serde_json::json!({"type": "contains", "field": "content", "value": "journal des chantiers", "distance": 0, "strict_separators": true})).unwrap();
        }
        let results = index.search(&q, 100, None).unwrap();
        let mut ids = index.node_ids_of(&results).unwrap();
        ids.sort_unstable();
        ids
    };
    assert_eq!(found(false), vec![0, 1, 2, 4], "relaxed: a space finds a hyphen, an underscore, and no separator at all");
    assert_eq!(found(true), vec![1], "strict: only the spaces");
}

/// A copied text in another case (a title in capitals) or with several spaces (an indentation) is still found, and a
/// query in capitals finds the lowercase text (asked by recherche, 22 November 2026).
#[test]
fn an_exact_contains_ignores_case_and_repeated_spaces() {
    let dir = common::scratch("exact-phrase-case-spaces");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 1 })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    for (i, t) in ["JOURNAL DES CHANTIERS", "    journal    des   chantiers", "le journal des chantiers", "un journal de bord"].iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "content": t })).unwrap();
    }
    index.commit().unwrap();
    for value in ["journal des chantiers", "Journal Des CHANTIERS", "journal   des chantiers"] {
        let q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "field": "content", "value": value, "distance": 0})).unwrap();
        let results = index.search(&q, 100, None).unwrap();
        let mut ids = index.node_ids_of(&results).unwrap();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2], "« {value} »: capitals and repeated spaces are found, nothing else");
    }
}
