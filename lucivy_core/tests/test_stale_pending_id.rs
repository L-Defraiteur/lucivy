//! **A word removed by the dictionary compaction does not come back with its old id** (19 November 2026, the lost
//! words of rag3weaver's code index). A minted word enters the shard dictionary's pending table, which forgets it
//! once a commit has named its texts — by the commit epoch, which only `IndexWriter::prepare_commit` turned. The
//! shards' commit (`drain_workers`, `flush_workers`, `finalize_flush_and_prepare`) never did: the table kept every
//! word for good, and answered without checking that its text still exists. Once a compaction had removed the
//! text of a word no live document cited any more, the next document holding that word took the old id back, with
//! no text in any generation: it no longer came out for that word.

mod common;

use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn found(index: &ShardedHandle, word: &str) -> Vec<u64> {
    let query = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(word.into()), ..Default::default() };
    let results = index.search(&query, 100, None).unwrap();
    let mut ids = index.node_ids_of(&results).unwrap();
    ids.sort_unstable();
    ids
}

#[test]
fn a_word_removed_by_the_compaction_comes_back_with_a_text() {
    let dir = common::scratch("stale-pending-id");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    }))
    .unwrap();
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config).unwrap();
    index.add_document_json(1, &serde_json::json!({ "content": "fn zzzmot alpha" })).unwrap();
    index.commit().unwrap();
    index.delete_by_node_id(1).unwrap();
    index.add_document_json(2, &serde_json::json!({ "content": "fn other beta" })).unwrap();
    index.commit().unwrap();
    // The first compaction puts the dead texts in reprieve, the second lets them go.
    for _ in 0..2 {
        index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    }
    index.add_document_json(3, &serde_json::json!({ "content": "fn zzzmot gamma" })).unwrap();
    index.commit().unwrap();
    let check = index.check_dictionary().unwrap();
    assert_eq!(check.fields.iter().map(|f| f.unknown).sum::<u64>(), 0, "no word lost: {check:?}");
    assert_eq!(found(&index, "zzzmot"), vec![3], "the new document is found by the word");
    index.close().unwrap();
}

/// **Files rewritten while the dictionary compaction runs in a tight loop lose no word** (19 November 2026).
/// One thread rewrites 60 files 600 times (delete the old version, add the new one, a commit every 3), with words
/// that come back from version to version; another runs `compact_dictionaries` as often as it is let through. On
/// luciepc this lost 383 words in 201 compactions before the fix, with no segment missed: the reused words took
/// their old ids back from the pending table, after the compaction had removed their texts.
#[test]
fn files_rewritten_under_a_tight_compaction_loop_lose_no_word() {
    let dir = common::scratch("tight-compaction");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    }))
    .unwrap();
    let index = std::sync::Arc::new(ShardedHandle::create(dir.path().to_str().unwrap(), &config).unwrap());
    let files = 60u64;
    let text = |f: u64, v: u64| format!("fn file{f} keep{f} version{v}x{f} shared words {} ident_{}_{} tail{}", (f * 31 + v * 17) % 97, f % 7, v % 5, (f + v) % 11);
    let node = |f: u64, v: u64| f * 1_000 + v;
    for f in 0..files {
        index.add_document_json(node(f, 0), &serde_json::json!({ "content": text(f, 0) })).unwrap();
        index.commit().unwrap();
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let compactor = {
        let (index, stop) = (index.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut runs = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if index.compact_dictionaries().is_ok() {
                    runs += 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            runs
        })
    };
    let mut version = vec![0u64; files as usize];
    for round in 1..=600u64 {
        let f = (round * 37) % files;
        let v = version[f as usize];
        index.delete_by_node_id(node(f, v)).unwrap();
        index.add_document_json(node(f, v + 1), &serde_json::json!({ "content": text(f, v + 1) })).unwrap();
        version[f as usize] = v + 1;
        if round % 3 == 0 {
            index.commit().unwrap();
        }
    }
    index.commit().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let runs = compactor.join().unwrap();
    index.wait_merges_quiet().unwrap();
    // Two more compactions: whatever is still in reprieve goes, as it would on the host the nights after.
    for _ in 0..2 {
        index.compact_dictionaries().unwrap();
    }
    let check = index.check_dictionary().unwrap();
    eprintln!("{runs} compactions during the rewrites; {check:?}");
    assert!(runs >= 10, "the compaction ran often during the rewrites: {runs}");
    assert_eq!(check.fields.iter().map(|f| f.unknown).sum::<u64>(), 0, "no word lost: {check:?}");
    for f in [0u64, 13, 59] {
        let v = version[f as usize];
        assert_eq!(found(&index, &format!("version{v}x{f}")), vec![node(f, v)], "the last version of file {f} is found");
    }
}
