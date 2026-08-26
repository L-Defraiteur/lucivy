//! Federated search: two nodes, statistics exported, merged, injected back.
//!
//! The contract, and what this pins:
//!
//! - the **union** of what the nodes return is what a single index holding
//!   every document returns — no document lost, none invented;
//! - a document **scores the same** on its node under the merged statistics
//!   as on that single index. That is the whole point of the mode: the
//!   corpus of the federation, not of the node. It was never asserted (the
//!   older test says "scores may differ, the SET must not"), and it is what
//!   makes the hits of two nodes comparable at the coordinator;
//! - the **pre-filter** composes with it (`search_filtered_with_global_stats`):
//!   the ids decide which documents are visited, the statistics how they score.
//!
//! Since 3.0.6 this path goes through the same DAG as `search()` — shards in
//! parallel, top-k bounded per shard, batching for an index that does not fit
//! in memory. Before, it was a sequential loop that collected every matching
//! document of every shard into one `Vec` before sorting.

use std::collections::{BTreeMap, HashSet};

use lucivy_core::bm25_global::ExportableStats;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::{RamShardStorage, ShardedHandle};

/// Documents built so that both halves carry the same vocabulary: a
/// federation whose nodes hold disjoint words proves nothing about the
/// statistics.
fn corpus(n: u64) -> Vec<(u64, String)> {
    let words = [
        "kmalloc(size, GFP_KERNEL)", "spin_lock_init(&adapter->lock)",
        "return -ENOMEM;", "mutex_unlock(&dev->mutex)", "struct net_device *ndev",
        "rag3_weaver::ShardedHandle", "kfree(buf)", "wait_merges_quiet()",
    ];
    (0..n).map(|i| {
        let a = words[(i % 8) as usize];
        let b = words[((i * 3 + 1) % 8) as usize];
        let c = words[((i * 5 + 2) % 8) as usize];
        (i, format!("/* doc {i} */ {a} {b} {c} /* end {i} */"))
    }).collect()
}

fn build(docs: &[(u64, String)], shards: usize) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text", "stored": true }],
        "sfx_version": 3,
        "shards": shards,
    })).unwrap();
    let h = ShardedHandle::create_with_storage(
        Box::new(RamShardStorage::new()), &config).unwrap();
    for (id, content) in docs {
        h.add_document_json(*id, &serde_json::json!({ "content": content })).unwrap();
    }
    h.commit().unwrap();
    h.wait_merges_quiet().unwrap();
    h
}

/// (node_id → score) of a result set.
fn by_id(index: &ShardedHandle, results: &[lucivy_core::sharded_handle::ShardedSearchResult])
    -> BTreeMap<u64, f32>
{
    let ids = index.node_ids_of(results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score)).collect()
}

fn q(json: &str) -> QueryConfig { serde_json::from_str(json).unwrap() }

const QUERIES: [&str; 6] = [
    r#"{"type":"contains","field":"content","value":"kmalloc"}"#,
    r#"{"type":"contains","field":"content","value":"lock_init"}"#,
    r#"{"type":"contains","field":"content","value":"rag3weaver"}"#,
    r#"{"type":"contains","field":"content","value":"kmallok","distance":1}"#,
    r#"{"type":"contains","field":"content","value":"mutex_[a-z]+","regex":true}"#,
    r#"{"type":"parse","fields":["content"],"value":"kmalloc AND NOT kfree"}"#,
];

#[test]
fn federated_equals_one_index_holding_everything() {
    let docs = corpus(400);
    // Interleaved, so both nodes hold the same vocabulary.
    let left: Vec<_> = docs.iter().step_by(2).cloned().collect();
    let right: Vec<_> = docs.iter().skip(1).step_by(2).cloned().collect();
    let node_a = build(&left, 2);
    let node_b = build(&right, 2);
    let single = build(&docs, 4);

    for qj in QUERIES {
        let query = q(qj);

        // Coordinator: gather, JSON round-trip like a real network hop, merge.
        let sa = node_a.export_stats(&query).unwrap();
        let sb = node_b.export_stats(&query).unwrap();
        let sa: ExportableStats = serde_json::from_str(&serde_json::to_string(&sa).unwrap()).unwrap();
        let sb: ExportableStats = serde_json::from_str(&serde_json::to_string(&sb).unwrap()).unwrap();
        let global = ExportableStats::merge(&[sa, sb]);
        assert_eq!(global.total_num_docs, docs.len() as u64,
            "merged stats must cover the whole federation for {qj}");

        let ra = node_a.search_with_global_stats(&query, 1000, &global, None).unwrap();
        let rb = node_b.search_with_global_stats(&query, 1000, &global, None).unwrap();
        let mut federated = by_id(&node_a, &ra);
        federated.extend(by_id(&node_b, &rb));

        let whole = by_id(&single, &single.search(&query, 1000, None).unwrap());
        assert!(!whole.is_empty(), "query returned nothing: {qj}");

        // Same documents.
        assert_eq!(federated.keys().collect::<Vec<_>>(), whole.keys().collect::<Vec<_>>(),
            "federated search lost or invented documents for {qj}");

        // Same scores: the statistics are the federation's, so a document
        // scores as it would in one index holding everything.
        for (id, score) in &federated {
            let expected = whole[id];
            assert!((score - expected).abs() <= 1e-3 * expected.abs().max(1.0),
                "{qj}: doc {id} scores {score} federated, {expected} in one index");
        }
    }
}

#[test]
fn the_pre_filter_composes_with_federated_statistics() {
    let docs = corpus(400);
    let left: Vec<_> = docs.iter().step_by(2).cloned().collect();
    let right: Vec<_> = docs.iter().skip(1).step_by(2).cloned().collect();
    let node_a = build(&left, 2);
    let node_b = build(&right, 2);
    let allowed: HashSet<u64> = (0..400u64).step_by(7).collect();

    for qj in QUERIES {
        let query = q(qj);
        let global = ExportableStats::merge(&[
            node_a.export_stats(&query).unwrap(),
            node_b.export_stats(&query).unwrap(),
        ]);

        for node in [&node_a, &node_b] {
            let full = by_id(node, &node.search_with_global_stats(&query, 1000, &global, None).unwrap());
            let filtered = by_id(node, &node
                .search_filtered_with_global_stats(&query, 1000, &global, None, allowed.clone())
                .unwrap());
            let expected: Vec<u64> = full.keys().copied().filter(|id| allowed.contains(id)).collect();
            assert_eq!(filtered.keys().copied().collect::<Vec<_>>(), expected,
                "{qj}: a filtered federated search is the federated one intersected with the allowed ids");
        }
    }
}

/// The DAG path bounds the top-k; the old sequential one collected every
/// match before sorting. A node must return `top_k` hits, the best ones.
#[test]
fn federated_top_k_is_bounded_and_is_the_best() {
    let docs = corpus(400);
    let node = build(&docs, 4);
    let query = q(QUERIES[0]);
    let global = ExportableStats::merge(&[node.export_stats(&query).unwrap()]);

    let all = node.search_with_global_stats(&query, 1000, &global, None).unwrap();
    assert!(all.len() > 5, "the corpus must answer more than 5 hits");
    let top5 = node.search_with_global_stats(&query, 5, &global, None).unwrap();
    assert_eq!(top5.len(), 5);
    let best: Vec<f32> = all.iter().take(5).map(|r| r.score).collect();
    let got: Vec<f32> = top5.iter().map(|r| r.score).collect();
    assert_eq!(got, best, "top-5 must be the five best of the full answer");
}
