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
use ld_lucivy::{Directory, HasLen};
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

/// Bytes of the shard dictionary's files by extension.
fn dictionary_files(index: &ShardedHandle) -> Vec<(String, u64)> {
    let dir = index.shard(0).unwrap().index.directory().clone();
    let mut by_ext: BTreeMap<String, u64> = BTreeMap::new();
    for path in dir.list_managed_files() {
        let name = path.to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("dict-") else { continue };
        let ext = rest.splitn(3, '.').nth(2).unwrap_or("").to_string();
        let bytes = dir.open_read(&path).map(|f| f.len() as u64).unwrap_or(0);
        *by_ext.entry(ext).or_default() += bytes;
    }
    by_ext.into_iter().collect()
}

fn dictionary_bytes(index: &ShardedHandle) -> u64 {
    *index.index_report().unwrap().bytes_by_kind.get("dictionary").unwrap_or(&0)
}

/// After `maintain` (purge, then compaction), the shard dictionary holds
/// only the texts of the live documents: its bytes are those of a fresh
/// index of the same documents: the same texts, and the bytes within the
/// margin of the kept ids' table (no id is renumbered).
#[test]
fn maintain_gives_back_the_dictionary_of_deleted_documents() {
    let purged_dir = common::scratch("dead-texts-purged");
    let fresh_dir = common::scratch("dead-texts-fresh");
    let purged = ShardedHandle::create(purged_dir.path().to_str().unwrap(), &config()).unwrap();
    for batch in 0..4u64 {
        for i in batch * 100..(batch + 1) * 100 {
            purged.add_document_json(i, &serde_json::json!({ "content": text(i) })).unwrap();
        }
        purged.commit().unwrap();
    }
    purged.wait_merges_quiet().unwrap();
    let full = dictionary_bytes(&purged);
    let before: Vec<_> = ["shared", "uniq2x", "tail4", "words here"].iter().map(|w| answers(&purged, w)).collect();
    let orphans: Vec<u64> = (0..400).filter(|i| i % 2 == 1).collect();
    let report = purged.maintain(&MaintenanceRequest { purge_node_ids: orphans.clone(), compact: true, ..Default::default() }).unwrap();
    // The first compaction keeps the dead texts one cycle in reprieve (19 November 2026); the second lets them go.
    purged.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();

    let fresh = ShardedHandle::create(fresh_dir.path().to_str().unwrap(), &config()).unwrap();
    for i in (0..400u64).filter(|i| i % 2 == 0) {
        fresh.add_document_json(i, &serde_json::json!({ "content": text(i) })).unwrap();
    }
    fresh.commit().unwrap();
    let fresh_report = fresh.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    eprintln!("dictionary texts: purged {:?}, fresh {:?}", report.dictionary_texts, fresh_report.dictionary_texts);
    assert_eq!(report.dictionary_texts.1, fresh_report.dictionary_texts.1, "[check: the-texts-of-a-fresh-index]");
    for (name, purged_part, fresh_part) in dictionary_files(&purged).into_iter().zip(dictionary_files(&fresh)).map(|((n, a), (_, b))| (n, a, b)) {
        eprintln!("  {name}: purged {purged_part}, fresh {fresh_part}");
    }

    let (dead, live) = (dictionary_bytes(&purged), dictionary_bytes(&fresh));
    let margin = (dead as f64 - live as f64).abs() / live as f64;
    eprintln!("dictionary bytes: all 400 {full}, purged {dead}, fresh {live} (margin {:.2} %)", margin * 100.0);
    assert!(dead < full, "[check: the-dictionary-shrinks]");
    // The texts are the fresh index's exactly (checked above). The bytes differ by the table of ids of the
    // `.termtexts`, which names the kept ids by runs of 8 bytes (`ids_section`: a u32 start and a u32
    // length): no id is renumbered, so the kept ids can be cut into as many runs as there are texts, the
    // order the writer threads minted them in deciding how many. A margin in percent did not hold: 2.25 %
    // and 3.65 % on luciepc, 6.2 % on a four-core CI runner (16 November 2026, 64 048 against 60 290). The
    // bound is the structure's: the fresh index, 2 % for the FST, plus one run per kept text.
    let kept_texts = report.dictionary_texts.1;
    let bound = live as f64 * 1.02 + 8.0 * kept_texts as f64;
    assert!((dead as f64) <= bound, "[check: the-dictionary-of-a-fresh-index] {dead} vs {live} (bound {bound:.0}, {kept_texts} texts)");

    // Same hits and scores as before the purge, minus the orphans; and as the fresh index.
    for (word, was) in ["shared", "uniq2x", "tail4", "words here"].iter().zip(before) {
        let now = answers(&purged, word);
        let expected: BTreeMap<u64, u32> = was.into_iter().filter(|(id, _)| id % 2 == 0).map(|(id, _)| (id, 0)).collect();
        assert_eq!(now.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>(), "[check: same-hits-minus-the-orphans] {word}");
        assert_eq!(now, answers(&fresh, word), "[check: same-hits-and-scores-as-fresh] {word}");
    }
    // The dictionary on disk is the compacted one: reopened, same answers.
    purged.close().unwrap();
    let reopened = ShardedHandle::open(purged_dir.path().to_str().unwrap()).unwrap();
    assert_eq!(dictionary_bytes(&reopened), dead, "[check: the-compacted-dictionary-survives-reopening]");
    for word in ["shared", "uniq2x", "tail4"] {
        assert_eq!(answers(&reopened, word), answers(&fresh, word), "[check: same-answers-after-reopening] {word}");
    }
    reopened.close().unwrap();
    fresh.close().unwrap();
}
