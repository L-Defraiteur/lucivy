//! **lucivy 4.4.2 — a boolean of fuzzy `contains` counts one error budget per document** (Lucie, 22 November 2026:
//! « partager un compte d'erreur global plutôt que chacun »). A fuzzy `contains` scores `−1000 × errors + BM25`, the
//! errors being its verified edits (v3) or missed trigrams (v2). `contains_split` expanded to a `boolean`, whose
//! clauses added up their penalties while a word not found cost nothing: a document that found three words with a
//! typo each ranked below a document that found a single word. Now `contains_split` with a distance scores
//! `−1000 × (errors of the words found + (distance + 1) per word missing) + the sum of their BM25`; a generic
//! boolean keeps its sum and warns; a single `contains` is unchanged.

mod common;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config() -> SchemaConfig {
    serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 1 })).unwrap()
}

fn contains(value: &str, distance: u8) -> QueryConfig {
    QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(value.into()), distance: Some(distance), ..Default::default() }
}

fn index_of(texts: &[&str]) -> (tempfile::TempDir, ShardedHandle) {
    let dir = common::scratch("fuzzy-score-coverage");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    for (i, t) in texts.iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "content": t })).unwrap();
    }
    index.commit().unwrap();
    (dir, index)
}

/// `(node id, score)`, best first.
fn ranked(index: &ShardedHandle, q: &QueryConfig) -> Vec<(u64, f32)> {
    let results = index.search(q, 100, None).unwrap();
    let ids = index.node_ids_of(&results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score)).collect()
}

/// A generic boolean keeps its sum (its clauses may be unrelated: a fuzzy title, an exact body, a filter); with two
/// fuzzy `contains` or more, it says so and points to `contains_split` (Lucie, 22 November 2026: « on met un warning
/// plutôt que modifier le boolean »).
#[test]
fn a_boolean_of_fuzzy_contains_says_to_use_contains_split() {
    let (_dir, index) = index_of(&["memary safty borowing checker"]);
    let q = QueryConfig {
        query_type: "boolean".into(),
        should: Some(["memory", "safety", "borrowing"].iter().map(|w| contains(w, 1)).collect()),
        ..Default::default()
    };
    let w = index.query_warnings(&q);
    assert!(w.iter().any(|m| m.contains("contains_split")), "the warning names contains_split: {w:?}");
    let one = QueryConfig { query_type: "boolean".into(), should: Some(vec![contains("memory", 1), contains("safety", 0)]), ..Default::default() };
    assert!(!index.query_warnings(&one).iter().any(|m| m.contains("contains_split")), "one fuzzy clause: no warning");
}

#[test]
fn contains_split_follows_the_same_budget() {
    let (_dir, index) = index_of(&["memary safty borowing checker", "memory and nothing else at all", "unrelated words only here"]);
    let q = split("memory safety borrowing", "flat");
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "contains_split: three words with a typo each come first: {r:?}");
}

#[test]
fn all_words_exact_rank_above_all_words_with_typos() {
    let (_dir, index) = index_of(&["memary safty borowing", "memory safety borrowing"]);
    let q = split("memory safety borrowing", "flat");
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(1), "exact words first: {r:?}");
}

/// A single fuzzy `contains` keeps the score of 4.4.1: exact text in the tier of no error, a typo one tier down.
#[test]
fn a_single_fuzzy_contains_keeps_its_tiers() {
    let (_dir, index) = index_of(&["the memory model", "the memary model"]);
    let r = ranked(&index, &contains("memory", 1));
    assert_eq!(r.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![0, 1], "exact first: {r:?}");
    assert!(r[0].1 > 0.0 && r[1].1 < 0.0 && r[1].1 > -1000.0, "exact without error, the typo one tier down: {r:?}");
}

fn split(value: &str, budget: &str) -> QueryConfig {
    serde_json::from_value(serde_json::json!({"type": "contains_split", "field": "content", "value": value, "distance": 1, "budget": budget})).unwrap()
}

/// A long page that holds every word somewhere, and noise documents full of « the ».
fn long_page_and_a_definition() -> (tempfile::TempDir, ShardedHandle) {
    let page = format!(
        "rebuild {} the {} vector {} index",
        "notes about the plan and the work ".repeat(40),
        "more of the same and the rest ".repeat(40),
        "and then the end of the story ".repeat(40)
    );
    let mut texts = vec!["fn rebuild_vector_index() -> Result<()>".to_string(), page];
    for i in 0..20 {
        texts.push(format!("the note {i} of the day"));
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    index_of(&refs)
}

/// (a) `idf`: a missing stop word costs almost nothing. The definition has no « the »: in `flat` the long page, at no
/// error, ranks first; in `idf` the definition does (the bench: « rebuild the vector index » lost `catalog.rs:2288`).
#[test]
fn idf_budget_lets_a_definition_without_the_stop_word_rank_first() {
    let (_dir, index) = long_page_and_a_definition();
    let r = ranked(&index, &split("rebuild the vector index", "idf"));
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "idf: the definition first: {r:?}");
}

/// (b) `window`: the words counted in the best window; a line gathering them ranks above a page scattering them.
#[test]
fn window_budget_ranks_gathered_words_above_scattered_ones() {
    let page = format!("rebuild {} vector {} index", "lorem ipsum dolor ".repeat(60), "sit amet ".repeat(60));
    // The line gathers the words with one typo; the page scatters them, exact. Without a window the page, at no error,
    // ranks first; in the window, the page gathers one word and misses two.
    let (_dir, index) = index_of(&[&page, "please rebuild the vecor index now"]);
    let r = ranked(&index, &split("rebuild vector index", "window"));
    assert_eq!(r.first().map(|(id, _)| *id), Some(1), "window: the gathered words first: {r:?}");
}

/// (c) `soft`: smaller tiers, the exact words still before the typos at the same words.
#[test]
fn soft_budget_keeps_exact_words_above_typos() {
    let (_dir, index) = index_of(&["memary safty borowing", "memory safety borrowing"]);
    let r = ranked(&index, &split("memory safety borrowing", "soft"));
    assert_eq!(r.first().map(|(id, _)| *id), Some(1), "soft: exact words first: {r:?}");
    assert!(r.iter().all(|(_, s)| *s > -100.0), "soft tiers stay small: {r:?}");
}

/// `contains` over several `fields`, one budget: a typo found in two fields is paid once, and the document with the
/// phrase in both fields ranks above the one with it in one field.
#[test]
fn a_fuzzy_contains_over_fields_pays_a_typo_once() {
    let dir = common::scratch("fuzzy-budget-fields");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "name", "type": "text" }, { "name": "content", "type": "text" }], "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    index.add_document_json(0, &serde_json::json!({ "name": "memary", "content": "the memary model" })).unwrap();
    index.add_document_json(1, &serde_json::json!({ "name": "other", "content": "the memary model" })).unwrap();
    index.commit().unwrap();
    let q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "fields": ["name", "content"], "value": "memory", "distance": 1})).unwrap();
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "the phrase in two fields first: {r:?}");
    assert!(r.iter().all(|(_, s)| *s > -1000.0), "one typo, paid once (one tier, not two): {r:?}");
}

/// 4.5.0 honoured `fields` only with a distance: an exact `contains` over several fields was refused ("this query type
/// reads 'field' (singular)"), and so was one over a single field given as `fields`. rag3weaver's exact pass sends
/// exactly that (22 November 2026, seven reds of its lib).
#[test]
fn an_exact_contains_over_fields_searches_every_field() {
    let dir = common::scratch("exact-fields");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "name", "type": "text" }, { "name": "content", "type": "text" }], "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    index.add_document_json(0, &serde_json::json!({ "name": "memory_store", "content": "the memory model" })).unwrap();
    index.add_document_json(1, &serde_json::json!({ "name": "other", "content": "the memory model" })).unwrap();
    index.add_document_json(2, &serde_json::json!({ "name": "memory", "content": "nothing here" })).unwrap();
    index.add_document_json(3, &serde_json::json!({ "name": "unrelated", "content": "nothing here" })).unwrap();
    index.commit().unwrap();
    let q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "fields": ["name", "content"], "value": "memory"})).unwrap();
    let r = ranked(&index, &q);
    let mut ids: Vec<u64> = r.iter().map(|(id, _)| *id).collect();
    ids.sort();
    assert_eq!(ids, vec![0, 1, 2], "each document with the word in one of its fields: {r:?}");
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "the word in both fields first: {r:?}");
    assert!(r.iter().all(|(_, s)| *s >= 0.0), "exact: no error tier: {r:?}");
    let one: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "fields": ["name"], "value": "memory"})).unwrap();
    let mut ids: Vec<u64> = ranked(&index, &one).iter().map(|(id, _)| *id).collect();
    ids.sort();
    assert_eq!(ids, vec![0, 2], "a single field given as `fields`");
}

/// 4.5.0: a fuzzy `contains` over several fields gave back one document of six (rag3weaver's `search_text_constraints`,
/// 22 November 2026). Every document carrying the word in one of its fields must come back, as the boolean of one
/// `contains` per field did.
#[test]
fn a_fuzzy_contains_over_fields_finds_every_document() {
    let dir = common::scratch("fuzzy-fields-all");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }, { "name": "name", "type": "text" }], "shards": 4
    })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    for (i, n) in ["alpha_lower", "alpha_title", "alpha_upper", "alpha_plain", "alpha_frob", "alpha_phrase"].iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "name": n, "content": format!("fn {n}() {{ let alpha = {i}; alpha }}") })).unwrap();
    }
    index.add_document_json(6, &serde_json::json!({ "name": "beta", "content": "fn beta() {}" })).unwrap();
    index.commit().unwrap();
    // rag3weaver searches with a highlight sink and, under a filter, with admitted ids: every way must agree.
    let all_ids: std::collections::HashSet<u64> = (0..7).collect();
    for distance in [0, 1, 2] {
        let q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "fields": ["content", "name"], "value": "alpha", "distance": distance})).unwrap();
        for way in ["plain", "highlights", "filtered", "filtered+highlights"] {
            let sink = way.contains("highlights").then(|| std::sync::Arc::new(ld_lucivy::query::HighlightSink::new()));
            let results = if way.starts_with("filtered") {
                index.search_filtered(&q, 100, sink, all_ids.clone()).unwrap()
            } else {
                index.search(&q, 100, sink).unwrap()
            };
            let mut ids = index.node_ids_of(&results).unwrap();
            ids.sort();
            assert_eq!(ids, vec![0, 1, 2, 3, 4, 5], "distance {distance}, {way}: every document with the word");
        }
    }
}

/// 4.5.0 read `fields` on a `contains` but still warned "'fields' is not read by query type \"contains\"". The warning
/// stays where it is true: a `contains` that also names `field` (the singular wins), and the types that ignore `fields`.
#[test]
fn a_contains_over_fields_is_not_warned_as_unread() {
    let (_dir, index) = index_of(&["memory safety"]);
    let over: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "fields": ["content", "name"], "value": "memory", "distance": 1})).unwrap();
    let w = index.query_warnings(&over);
    assert!(!w.iter().any(|m| m.contains("is not read")), "contains reads `fields`: {w:?}");
    let both: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains", "field": "content", "fields": ["content", "name"], "value": "memory"})).unwrap();
    assert!(index.query_warnings(&both).iter().any(|m| m.contains("is not read")), "`field` wins over `fields`: still warned");
    let regex: QueryConfig = serde_json::from_value(serde_json::json!({"type": "regex", "fields": ["content"], "value": "mem.*"})).unwrap();
    assert!(index.query_warnings(&regex).iter().any(|m| m.contains("is not read")), "regex ignores `fields`: still warned");
}

/// 4.5.0: `ErrorBudgetQuery` and `BestFieldQuery` did not hand the prescan to their words, as `BooleanQuery` does —
/// over several shards, the words only saw shard 0. A `contains_split` under a budget must find every document.
#[test]
fn contains_split_under_a_budget_finds_every_document_over_shards() {
    let dir = common::scratch("budget-shards");
    let cfg: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }], "shards": 4
    })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &cfg).unwrap();
    for i in 0..8u64 {
        index.add_document_json(i, &serde_json::json!({ "content": format!("note {i}: memory safety matters") })).unwrap();
    }
    index.add_document_json(8, &serde_json::json!({ "content": "nothing related" })).unwrap();
    index.commit().unwrap();
    for budget in ["sum", "flat", "idf", "window", "soft"] {
        let q: QueryConfig = serde_json::from_value(serde_json::json!({"type": "contains_split", "field": "content", "value": "memory safety", "distance": 1, "budget": budget})).unwrap();
        let mut ids: Vec<u64> = ranked(&index, &q).iter().map(|(id, _)| *id).collect();
        ids.sort();
        assert_eq!(ids, (0..8).collect::<Vec<u64>>(), "budget {budget}: every document over four shards");
    }
}
