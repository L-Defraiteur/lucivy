//! **The dictionary compaction keeps the words a document not yet committed reuses** (19 November 2026). The
//! code index of rag3weaver's own base had lost words: `check_dictionary` named 4 748 ids of `content` that live
//! segments reference and no dictionary generation holds, all inside the range of the compacted generation, and
//! cited mostly by one recent segment of ~550 documents without deletes (the re-index of changed files). The
//! compaction (`maintain` with `compact`) keeps the ids the committed segments' `.gmap`s name; a document still in
//! the indexer, which reused the id of a word only deleted documents held (the old version of its file), is not
//! among them. Its segment then names an id whose text is gone: the document drops out of the searches for that
//! word, fuzzy ones included.

mod common;

use lucivy_core::maintenance::MaintenanceRequest;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn config() -> SchemaConfig {
    serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }],
        "shards": 1
    }))
    .unwrap()
}

fn found(index: &ShardedHandle, word: &str) -> Vec<u64> {
    let query = QueryConfig { query_type: "contains".into(), field: Some("content".into()), value: Some(word.into()), ..Default::default() };
    let results = index.search(&query, 1_000, None).unwrap();
    let mut ids = index.node_ids_of(&results).unwrap();
    ids.sort_unstable();
    ids
}

#[test]
fn a_compaction_keeps_the_words_a_pending_document_reuses() {
    let dir = common::scratch("compaction-in-flight");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    // The first version of 50 files, committed: their words are minted.
    for i in 0..50u64 {
        index.add_document_json(i, &serde_json::json!({ "content": format!("fn version{i} reusedword{i} common") })).unwrap();
    }
    index.commit().unwrap();
    // The files change: the old versions are deleted (committed), the new ones are added, not yet committed;
    // they reuse the words `reusedword<i>` that only the deleted documents held.
    for i in 0..50u64 {
        index.delete_by_node_id(i).unwrap();
    }
    index.commit().unwrap();
    for i in 0..50u64 {
        index.add_document_json(100 + i, &serde_json::json!({ "content": format!("fn changed{i} reusedword{i} common") })).unwrap();
    }
    // The nightly maintenance compacts meanwhile, then the indexer commits.
    index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }).unwrap();
    index.commit().unwrap();

    let check = index.check_dictionary().unwrap();
    let lost: u64 = check.fields.iter().map(|f| f.unknown).sum();
    assert_eq!(lost, 0, "no word lost: {check:?}");
    for i in [0u64, 17, 49] {
        assert_eq!(found(&index, &format!("reusedword{i}")), vec![100 + i], "the new version is found by its reused word");
    }
    index.close().unwrap();
}

/// **The way rag3weaver writes, under stress** (19 November 2026): files re-indexed one at a time (the old
/// version deleted, the new one added, a commit each), merges and dictionary folds in the background, and a
/// nightly maintenance (`maintain` with `compact`) running from another thread while the indexing goes on. At
/// the end no word is lost. The lost words of the code base came from a path the single-threaded witness above
/// does not take.
#[test]
fn rewriting_files_while_the_maintenance_compacts_loses_no_word() {
    let dir = common::scratch("compaction-stress");
    let index = std::sync::Arc::new(ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap());
    let files = 60u64;
    // A deterministic stream: file f, version v.
    let text = |f: u64, v: u64| {
        format!(
            "fn file{f} keep{f} version{v}x{f} shared words {} {} tail{}",
            (f * 31 + v * 17) % 97,
            format!("ident_{}_{}", f % 7, v % 5),
            (f + v) % 11
        )
    };
    let node = |f: u64, v: u64| f * 1_000 + v;
    for f in 0..files {
        index.add_document_json(node(f, 0), &serde_json::json!({ "content": text(f, 0) })).unwrap();
        index.commit().unwrap();
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let maintenance = {
        let index = index.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut runs = 0u32;
            let mut refused = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match index.maintain(&MaintenanceRequest { compact: true, ..Default::default() }) {
                    Ok(_) => runs += 1,
                    Err(_) => refused += 1,
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (runs, refused)
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
    let (runs, refused) = maintenance.join().unwrap();
    index.wait_merges_quiet().unwrap();
    let check = index.check_dictionary().unwrap();
    let lost: u64 = check.fields.iter().map(|f| f.unknown).sum();
    eprintln!("maintenance: {runs} run(s), {refused} refused; check: {check:?}");
    assert!(runs > 0, "the maintenance ran at least once while the files were rewritten");
    assert_eq!(lost, 0, "no word lost: {check:?}");
    for f in [0u64, 13, 59] {
        let v = version[f as usize];
        assert_eq!(found(&index, &format!("version{v}x{f}")), vec![node(f, v)], "the last version of file {f} is found");
    }
}
