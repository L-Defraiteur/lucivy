//! A search filtered by `allowed_ids` scores as if the index held only the
//! allowed documents (`AggregatedBm25StatsOwned::with_subset_docs`): N is the
//! subset's size and every document frequency must be counted in the subset
//! too. The contains family counts it in its prescan, which only sees the
//! subset; this checks every query type against the same documents indexed
//! alone. All documents have the same length, so the average field length —
//! the corpus's on purpose — does not enter the comparison.

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;
use std::collections::{HashMap, HashSet};

const ALLOWED: u64 = 10;

fn build(path: &std::path::Path, docs: &[(u64, String)]) -> ShardedHandle {
    let _ = std::fs::remove_dir_all(path);
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "body", "type": "text" }],
        "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
    for (id, text) in docs {
        index.add_document_json(*id, &serde_json::json!({ "body": text })).unwrap();
    }
    index.commit().unwrap();
    index
}

/// 100 documents of four tokens; `alpha` in the first 50.
fn corpus() -> Vec<(u64, String)> {
    (0..100u64).map(|i| {
        let head = if i < 50 { "alpha" } else { "beta" };
        (i, format!("{head} item{i} gamma delta"))
    }).collect()
}

fn scores(index: &ShardedHandle, query: &QueryConfig, filter: Option<HashSet<u64>>) -> HashMap<u64, f32> {
    let results = match filter {
        Some(ids) => index.search_filtered(query, 1000, None, ids).unwrap(),
        None => index.search(query, 1000, None).unwrap(),
    };
    let ids = index.node_ids_of(&results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score)).collect()
}

fn query(query_type: &str, value: &str) -> QueryConfig {
    QueryConfig {
        query_type: query_type.into(),
        field: Some("body".into()),
        value: Some(value.into()),
        ..Default::default()
    }
}

#[test]
fn a_filtered_search_scores_like_the_subset_indexed_alone() {
    let base = std::env::temp_dir().join(format!("lucivy-filtered-scores-{}", std::process::id()));
    let docs = corpus();
    let full = build(&base.join("full"), &docs);
    let subset_docs: Vec<_> = docs.iter().filter(|(id, _)| *id < ALLOWED).cloned().collect();
    let subset = build(&base.join("subset"), &subset_docs);
    let allowed: HashSet<u64> = (0..ALLOWED).collect();

    let mut differences = Vec::new();
    for (kind, value) in [("term", "alpha"), ("contains", "alpha"), ("phrase", "alpha item3"), ("parse", "alpha")] {
        let q = query(kind, value);
        let filtered = scores(&full, &q, Some(allowed.clone()));
        let alone = scores(&subset, &q, None);
        assert!(!alone.is_empty(), "{kind} {value:?} finds nothing on the subset");
        let ids: std::collections::BTreeSet<u64> = filtered.keys().chain(alone.keys()).copied().collect();
        for id in ids {
            let (a, b) = (filtered.get(&id).copied(), alone.get(&id).copied());
            let same = matches!((a, b), (Some(x), Some(y)) if (x - y).abs() <= 1e-4 * y.abs().max(1.0));
            if !same {
                differences.push(format!("{kind} {value:?} doc {id}: filtered {a:?}, subset alone {b:?}"));
            }
        }
    }
    full.close().unwrap();
    subset.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
    assert!(differences.is_empty(), "a filtered search does not score like its subset:\n{}", differences.join("\n"));
}
