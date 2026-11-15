//! A filtered search scores with the average field length of the documents it admits, not of the whole
//! index: the hidden documents (closed versions kept in the index, the generations of rag3weaver) must not
//! move the ranks. Index A holds the visible documents V alone; index B holds V and longer hidden versions
//! H. `search_filtered(B, V)` must rank like `search(A)`, for term, contains, phrase and parse, and score
//! within a tolerance this test measures and prints.
//!
//! Until 16 November 2026 the filtered statistics kept N and the document frequencies of the subset (4.3.7)
//! but scaled the corpus's token count: the average length was V ∪ H's. The test of 4.3.7 could not see it,
//! its documents all had four tokens.

mod common;

use std::collections::{HashMap, HashSet};

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const VISIBLE: u64 = 60;

fn build(path: &std::path::Path, docs: &[(u64, String)], shards: usize) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "body", "type": "text" }],
        "shards": shards
    })).unwrap();
    let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
    for (id, text) in docs {
        index.add_document_json(*id, &serde_json::json!({ "body": text })).unwrap();
    }
    index.commit().unwrap();
    index
}

/// V: 60 visible documents of 50 to 110 tokens (past the 40 that field norms keep exact), a third with
/// `alpha`, some with `alpha beta`. H: for each, a hidden longer version (×4) with the same words.
fn corpus() -> (Vec<(u64, String)>, Vec<(u64, String)>) {
    let words = |i: u64, n: u64| -> String {
        let mut w: Vec<String> = (0..n).map(|k| format!("w{}", (i * 7 + k) % 37)).collect();
        if i % 3 == 0 { w[1] = "alpha".into(); w[2] = "beta".into(); }
        if i % 5 == 0 { w[n as usize - 1] = "alpha".into(); }
        w.join(" ")
    };
    let visible: Vec<(u64, String)> = (0..VISIBLE).map(|i| (i, words(i, 50 + (i * 13) % 61))).collect();
    let hidden: Vec<(u64, String)> = (0..VISIBLE).map(|i| (1000 + i, words(i, 4 * (50 + (i * 13) % 61)))).collect();
    (visible, hidden)
}

fn ranked(index: &ShardedHandle, query: &QueryConfig, filter: Option<HashSet<u64>>) -> Vec<(u64, f32)> {
    let results = match filter {
        Some(ids) => index.search_filtered(query, 1000, None, ids).unwrap(),
        None => index.search(query, 1000, None).unwrap(),
    };
    let ids = index.node_ids_of(&results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score)).collect()
}

fn query(kind: &str, value: &str) -> QueryConfig {
    QueryConfig { query_type: kind.into(), field: Some("body".into()), value: Some(value.into()), ..Default::default() }
}

fn run(shards: usize) {
    let scratch = common::scratch(&format!("filtered-hidden-lengths-{shards}"));
    let (visible, hidden) = corpus();
    let alone = build(&scratch.path().join("alone"), &visible, shards);
    let all: Vec<_> = visible.iter().chain(&hidden).cloned().collect();
    let with_history = build(&scratch.path().join("with-history"), &all, shards);
    let admitted: HashSet<u64> = visible.iter().map(|(id, _)| *id).collect();
    let mut worst = 0f64;
    let mut rank_differences = Vec::new();
    for (kind, value) in [("term", "alpha"), ("contains", "alpha"), ("contains", "w12"), ("phrase", "alpha beta"), ("parse", "alpha w3")] {
        let q = query(kind, value);
        let a = ranked(&alone, &q, None);
        let b = ranked(&with_history, &q, Some(admitted.clone()));
        assert!(!a.is_empty(), "{kind} {value:?} finds nothing");
        // Same ranks: the same ids in the same order, ties broken by id on both sides.
        let order = |v: &[(u64, f32)]| { let mut v = v.to_vec(); v.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap().then(x.0.cmp(&y.0))); v.into_iter().map(|x| x.0).collect::<Vec<_>>() };
        if order(&a) != order(&b) {
            rank_differences.push(format!("{kind} {value:?}: alone {:?}… with history {:?}…", &order(&a)[..a.len().min(6)], &order(&b)[..b.len().min(6)]));
        }
        let by_id: HashMap<u64, f32> = a.iter().copied().collect();
        for (id, score) in &b {
            if let Some(expected) = by_id.get(id) {
                worst = worst.max(((*score - *expected) as f64).abs() / (*expected as f64).abs().max(1e-9));
            }
        }
    }
    println!("{shards} shard(s): largest relative score gap {worst:.2e}");
    alone.close().unwrap();
    with_history.close().unwrap();
    assert!(rank_differences.is_empty(), "[check: same-ranks-with-and-without-history] {shards} shard(s):\n{}", rank_differences.join("\n"));
    // The ranks are exact; the scores carry what the view cannot know: the index of V alone counts its tokens
    // exactly, the view estimates them from field norms (a length is kept exact up to 40 tokens, then by steps
    // of about an eighth, read at the middle of its step). Measured on 16 November 2026: 3.97e-4 with one
    // shard, 1.89e-3 with two (against 3.98e-1 before the fix).
    assert!(worst <= 5e-3, "[check: same-scores-within-tolerance] {shards} shard(s): largest relative gap {worst:.2e}");
}

#[test]
fn one_shard_hidden_documents_do_not_move_the_ranks() { run(1) }

#[test]
fn two_shards_hidden_documents_do_not_move_the_ranks() { run(2) }
