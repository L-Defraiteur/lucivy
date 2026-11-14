//! `maintain`: purge a set of `_node_id`, compact, and say the index before
//! and after — on a fixture whose numbers are known: 4 shards, 400 documents
//! over four commits (several segments per shard), 100 of them orphans, plus
//! 5 ids the index never held.

mod common;

use std::collections::BTreeSet;

use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn fixture(path: &str) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 4
    })).unwrap();
    let index = ShardedHandle::create(path, &config).unwrap();
    for batch in 0..4u64 {
        for i in batch * 100..(batch + 1) * 100 {
            let text = format!("common word number{i} and some text {}", "filler ".repeat((i % 7) as usize + 1));
            index.add_document_json(i, &serde_json::json!({ "content": text })).unwrap();
        }
        index.commit().unwrap();
    }
    index.wait_merges_quiet().unwrap();
    index
}

fn hits(index: &ShardedHandle, word: &str) -> BTreeSet<u64> {
    let query = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(word.into()), ..Default::default() };
    let results = index.search(&query, 10_000, None).unwrap();
    index.node_ids_of(&results).unwrap().into_iter().collect()
}

fn orphans() -> Vec<u64> {
    // Every fourth document, and five ids the index never held.
    (0..400u64).filter(|i| i % 4 == 1).chain(1_000_000..1_000_005).collect()
}

#[test]
fn maintain_purges_the_given_ids_and_compacts() {
    let scratch = common::scratch("maintain");
    let path = scratch.path().to_str().unwrap().to_string();
    let index = fixture(&path);
    let all = hits(&index, "common");
    assert_eq!(all.len(), 400, "the fixture");

    assert_eq!(index.live_node_ids().unwrap(), (0..400).collect::<Vec<u64>>(), "[check: live-ids-before]");
    let request = MaintenanceRequest { purge_node_ids: orphans(), compact: true, ..Default::default() };
    let report = index.maintain(&request).unwrap();
    eprintln!("{}", report.summary());

    assert_eq!(report.requested, 105, "[check: requested-distinct-ids]");
    assert_eq!(report.purged, 100, "[check: purged-live-documents]");
    assert_eq!(report.absent, 5, "[check: absent-ids]");
    let (written, deleted) = report.before.documents();
    assert_eq!(written - deleted, 400, "[check: report-before]");
    let (written, deleted) = report.after.documents();
    assert_eq!((written, deleted), (300, 0), "[check: compaction-gives-back-the-deleted]");
    assert!(report.after.segments.len() <= report.before.segments.len(), "[check: segments-do-not-grow]");

    let expected: BTreeSet<u64> = all.iter().copied().filter(|i| i % 4 != 1).collect();
    assert_eq!(hits(&index, "common"), expected, "[check: same-hits-minus-the-orphans]");
    assert_eq!(index.live_node_ids().unwrap(), expected.iter().copied().collect::<Vec<u64>>(), "[check: live-ids-after]");
    // `contains` is a substring search: "number5" also finds number50..59.
    let survivors_with = |word: &str| -> BTreeSet<u64> {
        (0..400u64).filter(|i| i % 4 != 1 && format!("number{i}").contains(word)).collect()
    };
    assert_eq!(hits(&index, "number5"), survivors_with("number5"), "[check: an-orphan-is-not-found]");
    assert!(!hits(&index, "number5").contains(&5), "[check: the-orphan-5-is-gone]");
    assert!(hits(&index, "number6").contains(&6), "[check: a-survivor-is-found]");
    index.close().unwrap();
    drop(index);
    let reopened = ShardedHandle::open(&path).unwrap();
    assert_eq!(reopened.num_docs(), 300, "[check: the-purge-survives-reopening]");
    assert_eq!(hits(&reopened, "common"), expected, "[check: same-hits-after-reopening]");
    reopened.close().unwrap();
}

#[test]
fn maintain_without_ids_nor_compaction_only_reports() {
    let scratch = common::scratch("maintain-noop");
    let index = fixture(scratch.path().to_str().unwrap());
    let report = index.maintain(&MaintenanceRequest::default()).unwrap();
    assert_eq!((report.requested, report.purged, report.absent, report.merge_rounds), (0, 0, 0, 0));
    assert_eq!(report.before, report.after, "[check: nothing-done-nothing-changed]");
    index.close().unwrap();
}

#[test]
fn maintain_purges_without_compacting() {
    let scratch = common::scratch("maintain-purge-only");
    let index = fixture(scratch.path().to_str().unwrap());
    let report = index.maintain(&MaintenanceRequest { purge_node_ids: orphans(), ..Default::default() }).unwrap();
    assert_eq!(report.purged, 100, "[check: purged-live-documents]");
    assert_eq!(report.merge_rounds, 0, "[check: no-compaction-asked]");
    let (written, deleted) = report.after.documents();
    assert_eq!(written - deleted, 300, "[check: report-after]");
    assert_eq!(hits(&index, "common").len(), 300, "[check: orphans-gone]");
    index.close().unwrap();
}

/// On a COPY of a real table (opening writes): the live ids to a file, or the
/// purge of a list then the compaction, with the reports. What the maintenance
/// window of rag3weaver calls, replayed by hand:
///
///     LUCIVY_MAINTAIN_INDEX=<copy>/x.rag3db.fts/Lucivy_Scope \
///     [LUCIVY_MAINTAIN_LIVE_OUT=<file>]            # write the live _node_id, one per line, and stop
///     [LUCIVY_MAINTAIN_IDS=<file, one _node_id per line>] [LUCIVY_MAINTAIN_COMPACT=1] \
///     cargo test --release -p lucivy-core --test test_maintain maintain_an_index -- --ignored --nocapture
#[test]
#[ignore]
fn maintain_an_index() {
    let path = std::env::var("LUCIVY_MAINTAIN_INDEX").expect("LUCIVY_MAINTAIN_INDEX");
    let index = ShardedHandle::open(&path).unwrap();
    if let Ok(out) = std::env::var("LUCIVY_MAINTAIN_LIVE_OUT") {
        let ids = index.live_node_ids().unwrap();
        let text: String = ids.iter().map(|id| format!("{id}\n")).collect();
        std::fs::write(&out, text).unwrap();
        eprintln!("LIVE {path}: {} documents → {out}", ids.len());
        index.close().unwrap();
        return;
    }
    let purge_node_ids: Vec<u64> = std::env::var("LUCIVY_MAINTAIN_IDS").map(|file| {
        std::fs::read_to_string(file).unwrap().lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().parse().expect("one _node_id per line"))
            .collect()
    }).unwrap_or_default();
    let compact = std::env::var("LUCIVY_MAINTAIN_COMPACT").is_ok_and(|v| v == "1");
    let report = index.maintain(&MaintenanceRequest { purge_node_ids, compact, ..Default::default() }).unwrap();
    eprintln!("BEFORE\n{}\nAFTER\n{}\n{}", report.before.summary(), report.after.summary(), report.summary());
    index.close().unwrap();
    drop(index);
    let reopened = ShardedHandle::open(&path).unwrap();
    let (written, deleted) = report.after.documents();
    assert_eq!(reopened.num_docs(), written - deleted, "[check: the-maintenance-survives-reopening]");
    reopened.close().unwrap();
}
