//! **lucivy 4.4.2 — a boolean of fuzzy `contains` counts one error budget per document** (Lucie, 22 November 2026:
//! « partager un compte d'erreur global plutôt que chacun »). A fuzzy `contains` scores `−1000 × errors + BM25`, the
//! errors being its verified edits (v3) or missed trigrams (v2). Inside a `boolean` — and `contains_split`, which
//! expands to one — the clauses used to add up their penalties while a word not found cost nothing: a document that
//! found three words with a typo each ranked below a document that found a single word. Now a boolean with fuzzy
//! clauses scores `−1000 × (errors of the words found + (distance + 1) per word missing) + the sum of their BM25`; a
//! single `contains` is unchanged.

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

#[test]
fn three_words_found_with_a_typo_each_rank_above_a_single_exact_word() {
    // One edit each (a substitution, two deletions): a swap of two letters is two edits, out of distance 1.
    let (_dir, index) = index_of(&["memary safty borowing checker", "memory and nothing else at all", "unrelated words only here"]);
    let q = QueryConfig {
        query_type: "boolean".into(),
        should: Some(["memory", "safety", "borrowing"].iter().map(|w| contains(w, 1)).collect()),
        ..Default::default()
    };
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "the document with all three words, each with a typo, comes first: {r:?}");
}

#[test]
fn contains_split_follows_the_same_budget() {
    let (_dir, index) = index_of(&["memary safty borowing checker", "memory and nothing else at all", "unrelated words only here"]);
    let q = QueryConfig { query_type: "contains_split".into(), field: Some("content".into()), value: Some("memory safety borrowing".into()), distance: Some(1), ..Default::default() };
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "contains_split: three words with a typo each come first: {r:?}");
}

#[test]
fn all_words_exact_rank_above_all_words_with_typos() {
    let (_dir, index) = index_of(&["memary safty borowing", "memory safety borrowing"]);
    let q = QueryConfig {
        query_type: "boolean".into(),
        should: Some(["memory", "safety", "borrowing"].iter().map(|w| contains(w, 1)).collect()),
        ..Default::default()
    };
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
