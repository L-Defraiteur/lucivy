//! **The witnesses of lucivy 4.4.0, tagged generations** (20 November 2026; `docs/lucivy/20-11-2026/02-…` in rag3db).
//! Red first, on an API that answers « not implemented yet ».

mod common;

use std::collections::BTreeMap;

use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config() -> SchemaConfig {
    serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 4 })).unwrap()
}

fn text(i: u64, v: u64) -> String {
    format!("fn item{i} version{v} shared words tail{} uniq{i}x{v}", (i + v) % 7)
}

/// What a set of queries answers: per query, the documents and their score bits.
fn answers(index: &ShardedHandle) -> BTreeMap<String, Vec<(u64, u32)>> {
    let mut out = BTreeMap::new();
    for (kind, value, distance) in [("contains", "shared", 0u8), ("contains", "version2", 0), ("contains", "verzion1", 1), ("parse", "tail3 shared", 0)] {
        let q = QueryConfig { query_type: kind.into(), field: Some("content".into()), value: Some(value.into()), distance: Some(distance), ..Default::default() };
        let results = index.search(&q, 10_000, None).unwrap();
        let ids = index.node_ids_of(&results).unwrap();
        let mut pairs: Vec<(u64, u32)> = ids.into_iter().zip(results.iter().map(|r| r.score.to_bits())).collect();
        pairs.sort_unstable();
        out.insert(format!("{kind} {value} d{distance}"), pairs);
    }
    out
}

/// Every file of the directory, with its size and modification time.
fn fingerprint(dir: &std::path::Path) -> BTreeMap<String, (u64, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let m = e.metadata().unwrap();
                out.insert(p.strip_prefix(dir).unwrap().display().to_string(), (m.len(), m.modified().unwrap()));
            }
        }
    }
    out
}

/// Generation 1: 120 documents. Generation 2: 40 deleted, 80 new versions, then a merge and a dictionary compaction.
/// Generation 3: 60 more. Returns the answers of generation 1, taken right after its commit.
fn three_generations(index: &ShardedHandle) -> BTreeMap<String, Vec<(u64, u32)>> {
    for i in 0..120 {
        index.add_document_json(i, &serde_json::json!({ "content": text(i, 1) })).unwrap();
    }
    index.commit_tagged(1).expect("commit_tagged(1)");
    let first = answers(index);
    for i in 0..40 {
        index.delete_by_node_id(i).unwrap();
    }
    for i in 0..80 {
        index.add_document_json(1_000 + i, &serde_json::json!({ "content": text(i, 2) })).unwrap();
    }
    index.commit_tagged(2).expect("commit_tagged(2)");
    index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    for i in 0..60 {
        index.add_document_json(2_000 + i, &serde_json::json!({ "content": text(i, 3) })).unwrap();
    }
    index.commit_tagged(3).expect("commit_tagged(3)");
    first
}

#[test]
fn revert_to_brings_back_the_answers_of_g() {
    let dir = common::scratch("tagged-revert");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    let first = three_generations(&index);
    assert_ne!(answers(&index), first, "generation 3 answers otherwise");
    index.revert_to(1).expect("revert_to(1)");
    assert_eq!(answers(&index), first, "the same documents and scores as generation 1");
    assert_eq!(index.num_docs(), 120);
    assert_eq!(index.tagged_generations().unwrap(), (vec![1], 1), "the generations above 1 are forgotten");
}

#[test]
fn the_garbage_collector_keeps_every_kept_generation() {
    let dir = common::scratch("tagged-gc");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    three_generations(&index);
    assert_eq!(index.tagged_generations().unwrap(), (vec![1, 2, 3], 3));
    // Reopened from disk, generation 1 still answers: its files survived the merge and the compaction.
    drop(index);
    let read = ShardedHandle::open_read_only_at(dir.path().to_str().unwrap(), 1).expect("generation 1 is on disk");
    assert_eq!(read.num_docs(), 120);
    drop(read);
    let index = ShardedHandle::open(dir.path().to_str().unwrap()).unwrap();
    index.release_below(3).unwrap();
    index.add_document_json(9_999, &serde_json::json!({ "content": "after release" })).unwrap();
    index.commit_tagged(4).unwrap();
    assert_eq!(index.tagged_generations().unwrap(), (vec![3, 4], 4), "1 and 2 are released");
    assert!(ShardedHandle::open_read_only_at(dir.path().to_str().unwrap(), 1).is_err(), "generation 1 is gone");
}

#[test]
fn a_crash_in_the_middle_of_a_revert_is_said() {
    let dir = common::scratch("tagged-crash");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    three_generations(&index);
    drop(index);
    // A revert killed after the first shard: shard_0 is back at 1, the others are at 3.
    let shard0 = dir.path().join("shard_0");
    std::fs::copy(shard0.join("meta.1.json"), shard0.join("meta.json")).expect("meta.1.json is kept by shard_0");
    match ShardedHandle::open(dir.path().to_str().unwrap()) {
        Err(e) => assert!(e.contains("generation"), "the refusal says the shards disagree on the generation: {e}"),
        Ok(index) => {
            let said = index.tagged_generations().err().expect("the disagreement is said, not hidden");
            assert!(said.contains("shard_0"), "{said}");
        }
    }
}

#[test]
fn open_read_only_at_serves_g_without_writing() {
    let dir = common::scratch("tagged-read");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    let first = three_generations(&index);
    drop(index);
    let before = fingerprint(dir.path());
    let read = ShardedHandle::open_read_only_at(dir.path().to_str().unwrap(), 1).expect("open generation 1");
    assert_eq!(answers(&read), first, "the answers of generation 1");
    assert!(read.add_document_json(5, &serde_json::json!({ "content": "x" })).is_err(), "a reader does not write");
    drop(read);
    assert_eq!(fingerprint(dir.path()), before, "nothing written, modified or deleted");
}

#[test]
fn the_dictionary_after_a_revert() {
    let dir = common::scratch("tagged-dictionary");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    index.add_document_json(1, &serde_json::json!({ "content": "fn base alpha" })).unwrap();
    index.commit_tagged(1).unwrap();
    index.add_document_json(2, &serde_json::json!({ "content": "fn freshword beta" })).unwrap();
    index.commit_tagged(2).unwrap();
    index.revert_to(1).unwrap();
    // The ids minted in 2 are gone with its segments; the same word minted again gets a text.
    index.add_document_json(3, &serde_json::json!({ "content": "fn freshword gamma" })).unwrap();
    index.commit_tagged(2).unwrap();
    let check = index.check_dictionary().unwrap();
    assert_eq!(check.fields.iter().map(|f| f.unknown).sum::<u64>(), 0, "{check:?}");
    let q = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some("freshword".into()), ..Default::default() };
    let ids = index.node_ids_of(&index.search(&q, 10, None).unwrap()).unwrap();
    assert_eq!(ids, vec![3]);
}

#[test]
fn an_untagged_index_opens_and_its_first_commit_tagged_tags_it() {
    let dir = common::scratch("tagged-migration");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    index.add_document_json(1, &serde_json::json!({ "content": "written untagged" })).unwrap();
    index.commit().unwrap();
    drop(index);
    let index = ShardedHandle::open(dir.path().to_str().unwrap()).expect("an untagged index opens as before");
    assert!(index.tagged_generations().is_err() || index.tagged_generations().unwrap().0.is_empty(), "untagged");
    index.commit_tagged(7).expect("the first commit_tagged tags it");
    assert_eq!(index.tagged_generations().unwrap(), (vec![7], 7));
    for s in 0..4 {
        let meta = std::fs::read_to_string(dir.path().join(format!("shard_{s}/meta.json"))).unwrap();
        assert!(meta.contains("\"min_lucivy\""), "a tagged index tells an older lucivy to keep out: shard_{s}");
    }
    assert!(index.commit().is_err(), "an untagged commit on a tagged index is refused");
}
