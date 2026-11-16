//! The same documents give the same scores, to the bit, whatever the segments they were written in. A `parse`
//! of plain words is an OR of `contains`, one clause per word: the union sums the scores of the clauses that
//! hit a document. Until 16 November 2026 it summed them in f32 in the order of its live clauses, and that
//! order moved: a clause that ran out was taken out by `swap_remove`, which put the last clause in its
//! place. Whether a clause runs out before a document depends on what else its segment holds, so the same
//! document summed `w4 + w2 + w3` in one index and `w2 + w3 + w4` in another, an ulp apart (ticket « Un ulp
//! d'écart de score entre profils en parse », measured on 89 894 documents).
//!
//! Here: `D`, a document with `wone` alone, then documents `X` with `wtwo`, `wthree` and `wfour` at varied
//! frequencies. One index writes them in one commit (`wone` runs out at `D`, before every `X`); the other
//! writes `D` in a segment of its own (the segment of the `X` has no `wone` clause at all). Same N, same
//! document frequencies, same lengths: only the order of the sum differs.

mod common;

use std::collections::HashMap;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const XS: u64 = 40;

fn documents() -> Vec<(u64, String)> {
    let mut docs = vec![(0u64, "wone filler filler".to_string())];
    for i in 1..=XS {
        let mut words = Vec::new();
        for _ in 0..(1 + i % 5) { words.push("wtwo") }
        for _ in 0..(1 + (i * 3) % 7) { words.push("wthree") }
        for _ in 0..(1 + (i * 5) % 4) { words.push("wfour") }
        for _ in 0..(i % 9) { words.push("filler") }
        docs.push((i, words.join(" ")));
    }
    docs
}

fn build(path: &std::path::Path, commits: &[&[(u64, String)]]) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "body", "type": "text" }],
        "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
    for batch in commits {
        for (id, text) in batch.iter() {
            index.add_document_json(*id, &serde_json::json!({ "body": text })).unwrap();
        }
        index.commit().unwrap();
    }
    index
}

fn scores(index: &ShardedHandle, value: &str) -> HashMap<u64, f32> {
    let query = QueryConfig { query_type: "parse".into(), field: Some("body".into()), value: Some(value.into()), ..Default::default() };
    let results = index.search(&query, 1000, None).unwrap();
    index.node_ids_of(&results).unwrap().into_iter().zip(results.iter().map(|r| r.score)).collect()
}

fn segments(index: &ShardedHandle) -> usize {
    (0..index.num_shards()).map(|s| index.shard(s).unwrap().reader.searcher().segment_readers().len()).sum()
}

#[test]
fn parse_scores_are_identical_across_segment_layouts() {
    let scratch = common::scratch("parse-scores-layouts");
    let docs = documents();
    let one_commit = build(&scratch.path().join("one-commit"), &[&docs]);
    let apart = build(&scratch.path().join("apart"), &[&docs[..1], &docs[1..]]);
    println!("segments: one commit {}, apart {}", segments(&one_commit), segments(&apart));
    let mut differ = Vec::new();
    for value in ["wone wtwo wthree wfour", "wfour wthree wtwo wone"] {
        let (a, b) = (scores(&one_commit, value), scores(&apart, value));
        assert_eq!(a.len(), docs.len(), "{value:?}: every document is found");
        assert_eq!(b.len(), docs.len(), "{value:?}: every document is found");
        for (id, score) in &a {
            let other = b[id];
            if score.to_bits() != other.to_bits() {
                differ.push(format!("{value:?} doc {id}: {score:?} against {other:?}"));
            }
        }
    }
    assert!(differ.is_empty(), "{} scores differ between the two layouts:\n{}", differ.len(), differ.join("\n"));
}
