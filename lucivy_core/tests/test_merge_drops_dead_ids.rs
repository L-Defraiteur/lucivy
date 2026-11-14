//! A merge keeps only the global ids that still have a live document: after
//! deleting documents and compacting, the merged segment's `.gmap` names as
//! many ids as a fresh index of the surviving documents — and searches give
//! the same hits and the same scores as that fresh index.
//!
//! Until 14 November 2026 the merged `.gmap` was the union of its inputs':
//! the id of a term whose every document was deleted stayed, with empty
//! postings, and the shard dictionary could never drop its text.

mod common;

use std::collections::BTreeMap;

use ld_lucivy::suffix_fst::gmap::GmapReader;
use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config() -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    })).unwrap()
}

fn text(i: u64) -> String {
    // Words every document shares, and words only this one has.
    format!("shared words here uniq{i}x{} tail{}", i * 7919 % 10007, i % 13)
}

/// Ids named by the `.gmap` of every segment, per text field.
fn gmap_ids(index: &ShardedHandle) -> usize {
    let handle = index.shard(0).unwrap();
    let searcher = handle.reader.searcher();
    let mut ids = 0;
    for seg in searcher.segment_readers() {
        for field in seg.sfx_fields().collect::<Vec<_>>() {
            let bytes = seg.sfx_index_file("gmap", field).expect(".gmap").read_bytes().unwrap();
            ids += GmapReader::open(bytes.as_slice()).expect("gmap").len() as usize;
        }
    }
    ids
}

fn answers(index: &ShardedHandle, word: &str) -> BTreeMap<u64, u32> {
    let query = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(word.into()), ..Default::default() };
    let results = index.search(&query, 10_000, None).unwrap();
    let ids = index.node_ids_of(&results).unwrap();
    ids.into_iter().zip(results.iter().map(|r| r.score.to_bits())).collect()
}

#[test]
fn a_merge_drops_the_ids_of_deleted_documents() {
    let purged_dir = common::scratch("dead-ids-purged");
    let fresh_dir = common::scratch("dead-ids-fresh");
    let purged = ShardedHandle::create(purged_dir.path().to_str().unwrap(), &config()).unwrap();
    for batch in 0..4u64 {
        for i in batch * 100..(batch + 1) * 100 {
            purged.add_document_json(i, &serde_json::json!({ "content": text(i) })).unwrap();
        }
        purged.commit().unwrap();
    }
    purged.wait_merges_quiet().unwrap();
    let orphans: Vec<u64> = (0..400).filter(|i| i % 2 == 1).collect();
    eprintln!("ids before the purge: {}", gmap_ids(&purged));
    let report = purged.maintain(&MaintenanceRequest { purge_node_ids: orphans, compact: true, ..Default::default() }).unwrap();
    eprintln!("{}", report.summary());
    assert_eq!(report.purged, 200);
    assert_eq!(report.after.documents(), (200, 0), "[check: compaction-purges-a-lone-segment]");

    let fresh = ShardedHandle::create(fresh_dir.path().to_str().unwrap(), &config()).unwrap();
    for i in (0..400u64).filter(|i| i % 2 == 0) {
        fresh.add_document_json(i, &serde_json::json!({ "content": text(i) })).unwrap();
    }
    fresh.commit().unwrap();
    fresh.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();

    let (dead, live) = (gmap_ids(&purged), gmap_ids(&fresh));
    eprintln!("ids in the merged .gmap: purged {dead}, fresh {live}");
    assert_eq!(dead, live, "[check: the-merged-gmap-names-only-live-ids]");

    for word in ["shared", "uniq2x", "uniq3x", "tail1", "tail4", "words here", "x1"] {
        assert_eq!(answers(&purged, word), answers(&fresh, word), "[check: same-hits-and-scores] {word}");
    }
    purged.close().unwrap();
    fresh.close().unwrap();
}
