//! **The engine's test doors, through `lucivy_core`** (feature `failpoints`, run by hand:
//! `cargo test -p lucivy-core --features failpoints --test test_failpoints`).
#![cfg(feature = "failpoints")]

mod common;

use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::SchemaConfig;
use lucivy_core::sharded_handle::ShardedHandle;

/// **`maintain` names the segment of a text cited again after its reprieve** (19 November 2026). A
/// compaction that does not see a segment (the door hides one) puts its texts in reprieve; the next
/// `maintain` takes them back and says so in its report: shard, field, id and the segment that cites
/// them. That name is what rag3weaver keeps in its maintenance note, to find why a compaction missed a
/// segment on the host.
#[test]
fn maintain_names_the_segment_of_a_text_back_from_its_reprieve() {
    let _scenario = fail::FailScenario::setup();
    let dir = common::scratch("revived");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    }))
    .unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config).unwrap();
    index.add_document_json(1, &serde_json::json!({ "content": "fn seenword alpha" })).unwrap();
    index.commit().unwrap();
    index.add_document_json(2, &serde_json::json!({ "content": "fn hiddenword beta" })).unwrap();
    index.commit().unwrap();

    fail::cfg("compact_dictionary::hide_segments", "1*return").unwrap();
    let first = index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    fail::remove("compact_dictionary::hide_segments");
    assert!(first.dictionary_revived.is_empty(), "{:?}", first.dictionary_revived);

    let second = index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    assert!(!second.dictionary_revived.is_empty(), "the second maintain names what came back");
    assert!(
        second.dictionary_revived.iter().all(|r| r.starts_with("shard_0: field ") && r.contains(", segment ")),
        "{:?}",
        second.dictionary_revived
    );
    assert!(second.summary().contains("cited again after their reprieve"), "{}", second.summary());
    let check = index.check_dictionary().unwrap();
    assert_eq!(check.fields.iter().map(|f| f.unknown).sum::<u64>(), 0, "{check:?}");
    index.close().unwrap();
}

/// **A revert is all or nothing** (lucivy 4.4.0): a failure injected on the third shard leaves the four at their
/// former generation.
#[test]
fn revert_to_is_all_or_nothing() {
    let _scenario = fail::FailScenario::setup();
    let dir = common::scratch("tagged-atomic");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({ "fields": [{ "name": "content", "type": "text" }], "shards": 4 })).unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config).unwrap();
    for (g, n) in [(1u64, 40u64), (2, 80)] {
        for i in 0..n {
            index.add_document_json(g * 1_000 + i, &serde_json::json!({ "content": format!("doc {g} {i}") })).unwrap();
        }
        index.commit_tagged(g).unwrap();
    }
    let docs = index.num_docs();
    fail::cfg("tagged::revert_shard", "2*off->1*return").unwrap();
    assert!(index.revert_to(1).is_err(), "the third shard fails");
    fail::remove("tagged::revert_shard");
    assert_eq!(index.num_docs(), docs, "the four shards stay at generation 2");
    assert_eq!(index.tagged_generations().unwrap(), (vec![1, 2], 2));
}
