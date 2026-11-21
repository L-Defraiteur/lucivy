//! **The fuzzy `contains` scores the same alone and inside a `boolean`** (21 November 2026, Lucie's hypothesis that
//! the boolean path of several fuzzy `contains` — rag3weaver's `ContainsSplit` — had stayed on an older version of the
//! miss-count scoring). It has not: a `boolean` builds each clause through the same `build_query`, hence the same
//! `FuzzyQueryV3` and `SfxScorer` (`−1000 × missed trigrams + BM25`), and only sums its clauses. This witness fixes it.

mod common;

use std::collections::BTreeMap;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config() -> SchemaConfig {
    serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 4 })).unwrap()
}

fn scores(index: &ShardedHandle, q: &QueryConfig) -> BTreeMap<u64, u32> {
    let results = index.search(q, 10_000, None).unwrap();
    let ids = index.node_ids_of(&results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score.to_bits())).collect()
}

fn contains(value: &str, distance: u8) -> QueryConfig {
    QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(value.into()), distance: Some(distance), ..Default::default() }
}

#[test]
fn a_fuzzy_contains_scores_the_same_alone_and_in_a_boolean_of_one_clause() {
    let dir = common::scratch("fuzzy-score-paths");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    let texts = ["memory safety in rust", "memroy safety everywhere", "the safety of memorys", "nothing to see here", "memory memory memory"];
    for (i, t) in texts.iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "content": t })).unwrap();
    }
    index.commit().unwrap();
    for (word, distance) in [("memory", 1u8), ("memroy", 1), ("safety", 1), ("memory", 0)] {
        let alone = scores(&index, &contains(word, distance));
        let boolean = QueryConfig { query_type: "boolean".into(), should: Some(vec![contains(word, distance)]), ..Default::default() };
        let in_boolean = scores(&index, &boolean);
        assert!(!alone.is_empty(), "{word} d{distance} finds something");
        assert_eq!(alone, in_boolean, "{word} d{distance}: the same documents and the same scores, alone or in a boolean of one clause");
    }
}
