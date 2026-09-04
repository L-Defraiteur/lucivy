//! A filtered search returns exactly the unfiltered hits whose id is allowed,
//! with the same highlights — whatever the query type and however early the
//! engine prunes. This is the contract that lets the id filter move from the
//! collector down into the v3 resolvers without changing an answer.
//!
//! Scores are not compared: the document frequency a prescan reports may
//! legitimately differ once the walk is pruned.

use std::collections::{BTreeMap, HashSet};

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const DOCS: u64 = 400;

/// `sfx_version` 3 (an FST per segment) or 4 (a dictionary per shard).
fn build(path: &str, sfx_version: u8) -> ShardedHandle {
    let _ = std::fs::remove_dir_all(path);
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            { "name": "path", "type": "text" },
            { "name": "content", "type": "text" }
        ],
        "sfx_version": sfx_version,
        "shards": 3
    })).unwrap();
    let index = ShardedHandle::create(path, &config).unwrap();
    let words = ["kmalloc", "kfree", "spin_lock_init", "mutex_unlock", "net_device", "ENOMEM", "kmallok", "rag3_weaver"];
    for i in 0..DOCS {
        let w1 = words[(i % 8) as usize];
        let w2 = words[((i * 3 + 1) % 8) as usize];
        let w3 = words[((i * 5 + 2) % 8) as usize];
        index.add_document_json(i, &serde_json::json!({
            "path": format!("drivers/net/file_{i}.c"),
            "content": format!("void *buf = {w1}(size, GFP_KERNEL); {w2}(&lock); return -{w3}; /* doc {i} {w1} */"),
        })).unwrap();
    }
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();
    index
}

/// (node_id → highlights by field) for a search, filtered or not.
fn hits(index: &ShardedHandle, q: &QueryConfig, allowed: Option<&HashSet<u64>>)
    -> BTreeMap<u64, BTreeMap<String, Vec<[usize; 2]>>>
{
    use ld_lucivy::schema::Value;
    let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
    let results = match allowed {
        None => index.search(q, DOCS as usize, Some(sink.clone())).unwrap(),
        Some(ids) => index.search_filtered(q, DOCS as usize, Some(sink.clone()), ids.clone()).unwrap(),
    };
    let nid = index.field("_node_id").unwrap();
    results.iter().map(|r| {
        let shard = index.shard(r.shard_id).unwrap();
        let searcher = shard.reader.searcher();
        let seg = searcher.segment_reader(r.doc_address.segment_ord);
        let doc: ld_lucivy::LucivyDocument = searcher.doc(r.doc_address).unwrap();
        let id = doc.get_first(nid).and_then(|v| v.as_u64()).unwrap();
        let mut hl: BTreeMap<String, Vec<[usize; 2]>> = sink.get(seg.segment_id(), r.doc_address.doc_id)
            .unwrap_or_default().into_iter().collect();
        for spans in hl.values_mut() { spans.sort(); spans.dedup(); }
        (id, hl)
    }).collect()
}

fn q(json: &str) -> QueryConfig { serde_json::from_str(json).unwrap() }

#[test]
fn filtered_equals_unfiltered_intersected_with_allowed() {
    filtered_equals_unfiltered("/tmp/lucivy_test_filtered_truth", 3);
}

/// The same contract on a shard dictionary (`sfx_version` 4): the filter
/// reaches the prescan through the same channel, the plan and the prefix
/// alternatives do not see it.
#[test]
fn filtered_equals_unfiltered_intersected_with_allowed_on_dictionary() {
    filtered_equals_unfiltered("/tmp/lucivy_test_filtered_truth_dict", 4);
}

fn filtered_equals_unfiltered(path: &str, sfx_version: u8) {
    let index = build(path, sfx_version);
    let queries = [
        r#"{"type":"contains","field":"content","value":"kmalloc"}"#,
        r#"{"type":"contains","field":"content","value":"lock_init","strict_separators":true}"#,
        r#"{"type":"contains","field":"content","value":"spin lock init"}"#,
        r#"{"type":"fuzzy","field":"content","value":"kmaloc","distance":1}"#,
        r#"{"type":"fuzzy","field":"content","value":"kmalloc","fuzzy_metric":"jaro_winkler","min_similarity":0.9}"#,
        r#"{"type":"regex","field":"content","value":"mutex_[a-z]+"}"#,
        r#"{"type":"phrase","field":"content","value":"return -ENOMEM"}"#,
        r#"{"type":"startsWith","field":"content","value":"net_dev"}"#,
        r#"{"type":"term","field":"content","value":"kfree"}"#,
        r#"{"type":"parse","fields":["content","path"],"value":"kmalloc AND NOT kfree"}"#,
        r#"{"type":"contains","field":"path","value":"file_1"}"#,
    ];
    // Allowed sets: small, medium, everything, and one with ids the index never saw.
    let sets: Vec<(String, HashSet<u64>)> = vec![
        ("10 ids".into(), (0..DOCS).step_by(40).collect()),
        ("50 ids".into(), (0..DOCS).filter(|i| i % 8 == 3).collect()),
        ("all".into(), (0..DOCS).collect()),
        ("some unknown".into(), [1u64, 2, 7, 9_999, 12_345].into_iter().collect()),
    ];
    let mut checked = 0;
    for qj in queries {
        let query = q(qj);
        let full = hits(&index, &query, None);
        assert!(!full.is_empty() || qj.contains("file_1"), "query returned nothing: {qj}");
        for (label, allowed) in &sets {
            let filtered = hits(&index, &query, Some(allowed));
            let expected: BTreeMap<_, _> = full.iter()
                .filter(|(id, _)| allowed.contains(id))
                .map(|(id, hl)| (*id, hl.clone()))
                .collect();
            assert_eq!(filtered, expected, "{qj} with {label}");
            checked += 1;
        }
    }
    assert_eq!(checked, queries.len() * sets.len());

    // Deletions: the filter is a separate channel from the alive bitset, so
    // an unfiltered search keeps its answers and its scores, and a filtered
    // one still never returns a deleted document, allowed or not.
    // Ties are ordered by physical address, which a commit reshuffles: compare
    // by id, the scores being what is asserted.
    let mut before: Vec<(u64, f32)> = scored(&index, &q(queries[0]));
    before.sort_by_key(|(id, _)| *id);
    for id in (0..DOCS).filter(|i| i % 8 == 0 && i % 5 == 0) {
        index.delete_by_node_id(id).unwrap();
    }
    index.commit().unwrap();
    let deleted: HashSet<u64> = (0..DOCS).filter(|i| i % 8 == 0 && i % 5 == 0).collect();
    let mut after: Vec<(u64, f32)> = scored(&index, &q(queries[0]));
    after.sort_by_key(|(id, _)| *id);
    assert!(after.iter().all(|(id, _)| !deleted.contains(id)), "deleted doc returned: {after:?}");
    let kept: Vec<(u64, f32)> = before.into_iter().filter(|(id, _)| !deleted.contains(id)).collect();
    assert_eq!(after, kept, "an unfiltered search on a segment with deletions keeps its scores");
    for qj in queries {
        let query = q(qj);
        let full = hits(&index, &query, None);
        assert!(full.keys().all(|id| !deleted.contains(id)));
        for (label, allowed) in &sets {
            let filtered = hits(&index, &query, Some(allowed));
            let expected: BTreeMap<_, _> = full.iter()
                .filter(|(id, _)| allowed.contains(id))
                .map(|(id, hl)| (*id, hl.clone()))
                .collect();
            assert_eq!(filtered, expected, "{qj} with {label}, after deletions");
        }
    }
    index.close().unwrap();
}

/// (node_id, score) in result order.
fn scored(index: &ShardedHandle, q: &QueryConfig) -> Vec<(u64, f32)> {
    use ld_lucivy::schema::Value;
    let nid = index.field("_node_id").unwrap();
    index.search_with_docs(q, DOCS as usize).unwrap().iter()
        .map(|h| (h.doc.get_first(nid).and_then(|v| v.as_u64()).unwrap(), h.score))
        .collect()
}

/// Cost of a filtered search against the unfiltered one, on the 10 000-file
/// kernel index `test_playground_parity` leaves in `/tmp/lucivy_parity_native`.
/// `cargo test --release -p lucivy-core --test test_filtered_search_truth bench_filtered -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_filtered_search_cost() {
    let dir = std::env::var("PARITY_DIR").unwrap_or_else(|_| "/tmp/lucivy_parity_native".into());
    if !std::path::Path::new(&dir).join("shard_0").exists() && !std::path::Path::new(&dir).join("router.json").exists() {
        eprintln!("no index at {dir}, skipping"); return;
    }
    let index = ShardedHandle::open(&dir).unwrap();
    let n = index.num_docs() as u64;
    let queries = [
        ("contains kmalloc", r#"{"type":"contains","field":"content","value":"kmalloc"}"#),
        ("split spin lock init", r#"{"type":"contains_split","field":"content","value":"spin lock init"}"#),
        ("fuzzy d1 kmallc", r#"{"type":"fuzzy","field":"content","value":"kmallc","distance":1}"#),
        ("regex spin_lock_[a-z]+", r#"{"type":"regex","field":"content","value":"spin_lock_[a-z]+"}"#),
        ("phrase return -ENOMEM", r#"{"type":"phrase","field":"content","value":"return -ENOMEM"}"#),
    ];
    let sets: Vec<(&str, Option<HashSet<u64>>)> = vec![
        ("unfiltered", None),
        ("10 ids", Some((0..n).step_by((n / 10).max(1) as usize).take(10).collect())),
        ("100 ids", Some((0..n).step_by((n / 100).max(1) as usize).take(100).collect())),
        ("1000 ids", Some((0..n).step_by((n / 1000).max(1) as usize).take(1000).collect())),
    ];
    for (name, qj) in queries {
        let query = q(qj);
        let mut line = format!("{name:28}");
        for (label, allowed) in &sets {
            // warm once, then the median of 5
            let run = || {
                let sink = std::sync::Arc::new(ld_lucivy::query::HighlightSink::new());
                let t = std::time::Instant::now();
                let r = match allowed {
                    None => index.search(&query, 100, Some(sink)).unwrap(),
                    Some(ids) => index.search_filtered(&query, 100, Some(sink), ids.clone()).unwrap(),
                };
                (t.elapsed().as_secs_f64() * 1e3, r.len())
            };
            run();
            let mut ts: Vec<(f64, usize)> = (0..5).map(|_| run()).collect();
            ts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            line += &format!(" | {label}: {:6.1} ms ({} hits)", ts[2].0, ts[2].1);
        }
        eprintln!("{line}");
    }
}
