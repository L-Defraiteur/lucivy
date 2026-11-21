//! **lucivy 4.4.2 — the score of a fuzzy `contains` is `BM25 × coverage`** (Lucie, 21 November 2026: « le score
//! négatif, il faut l'interpréter correctement »; the page `docs/lucivy/21-11-2026/03-le-sens-du-score-negatif.md` in
//! rag3db). It was `−1000 × missed trigrams + BM25`: inside a `boolean`, every fuzzy clause added its penalty, and a
//! document that found three words with a typo each ranked below a document that found a single word. Coverage is the
//! share of the query's trigrams found, in ]0, 1], 1 for an exact match.

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
    let (_dir, index) = index_of(&["memroy saftey borowing checker", "memory and nothing else at all", "unrelated words only here"]);
    let q = QueryConfig {
        query_type: "boolean".into(),
        should: Some(["memory", "safety", "borrowing"].iter().map(|w| contains(w, 1)).collect()),
        ..Default::default()
    };
    let r = ranked(&index, &q);
    assert_eq!(r.first().map(|(id, _)| *id), Some(0), "the document with all three words, each with a typo, comes first: {r:?}");
}

#[test]
fn a_fuzzy_contains_never_scores_below_zero() {
    let (_dir, index) = index_of(&["memroy leak", "memory leak", "memoyr leak", "mmeory leak"]);
    for (word, d) in [("memory", 1u8), ("memory", 2), ("leak", 1)] {
        for (id, score) in ranked(&index, &contains(word, d)) {
            assert!(score >= 0.0, "{word} d{d}: document {id} scores {score}");
        }
    }
}

#[test]
fn an_exact_match_ranks_above_the_same_text_with_a_typo() {
    let (_dir, index) = index_of(&["the memory model", "the memroy model"]);
    let r = ranked(&index, &contains("memory", 1));
    assert_eq!(r.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![0, 1], "exact first: {r:?}");
    assert!(r[0].1 > r[1].1, "strictly: {r:?}");
}
