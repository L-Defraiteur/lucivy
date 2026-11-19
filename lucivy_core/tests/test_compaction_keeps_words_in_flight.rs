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
    // Diagnostic branch: the engine says, per compaction, the segments it sees; five rounds, for the CI runners
    // where the race shows (19 November 2026).
    std::env::set_var("LUCIVY_DEBUG_COMPACTION", "1");
    for round in 0..10 {
        eprintln!("[debug-compaction] ===== round {round}");
        rewriting_files_once();
    }
    panic!("diagnostic branch: fails on purpose, so that the CI shows the compaction log");
}

fn rewriting_files_once() {
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

/// **A word minted by a document deleted before its commit is not handed to the next one** (19 November 2026).
/// A new word enters the shard dictionary's pending table when a document mints it; the table forgets it once a
/// commit names the segment's texts as a pending part. A document deleted before the commit leaves a segment
/// with no live document, so no texts are named, and the commit returned before forgetting: the next document
/// with the same word took the id from the pending table, unverified, and its segment cites an id no generation
/// holds — the re-index of a file, added, deleted and added again within a commit.
#[test]
fn a_word_minted_by_a_document_deleted_before_its_commit_is_not_lost() {
    let dir = common::scratch("minted-then-deleted");
    let index = ShardedHandle::create(dir.path().to_str().unwrap(), &config()).unwrap();
    index.add_document_json(1, &serde_json::json!({ "content": "base words" })).unwrap();
    index.commit().unwrap();
    // A first version, deleted before any commit.
    index.add_document_json(10, &serde_json::json!({ "content": "fn freshword alpha" })).unwrap();
    index.delete_by_node_id(10).unwrap();
    index.commit().unwrap();
    // The file again, with the same new word.
    index.add_document_json(11, &serde_json::json!({ "content": "fn freshword beta" })).unwrap();
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();
    let check = index.check_dictionary().unwrap();
    let lost: u64 = check.fields.iter().map(|f| f.unknown).sum();
    assert_eq!(lost, 0, "no word lost: {check:?}");
    assert_eq!(found(&index, "freshword"), vec![11], "the second version is found by its word");
}

/// **The host's pattern, harder** (19 November 2026): four shards; one thread rewrites files (delete the old
/// version, add the new one, a commit every few files) for 20 seconds; another runs the nightly maintenance in a
/// loop — purge of a few orphans, then `compact` — as often as it can. No word may be lost.
#[test]
#[ignore]
fn the_host_pattern_under_repeated_maintenance_loses_no_word() {
    let dir = common::scratch("compaction-host-pattern");
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "content", "type": "text" }, { "name": "name", "type": "text" }],
        "shards": 4
    }))
    .unwrap();
    let index = std::sync::Arc::new(ShardedHandle::create(dir.path().to_str().unwrap(), &config).unwrap());
    let files = 400u64;
    let text = |f: u64, v: u64| {
        format!(
            "pub fn file{f}_v{v}() {{ let word{}x{} = call_{}(arg{}); }} // note{} shared",
            f % 53, v % 7, (f * 13 + v) % 211, v, (f + v * 3) % 389
        )
    };
    let node = |f: u64, v: u64| f * 100_000 + v;
    for f in 0..files {
        index.add_document_json(node(f, 0), &serde_json::json!({ "content": text(f, 0), "name": format!("file{f}") })).unwrap();
    }
    index.commit().unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let orphans = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let maintenance = {
        let (index, stop, orphans) = (index.clone(), stop.clone(), orphans.clone());
        std::thread::spawn(move || {
            let (mut runs, mut refused) = (0u32, 0u32);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let purge: Vec<u64> = std::mem::take(&mut *orphans.lock().unwrap());
                match index.maintain(&MaintenanceRequest { purge_node_ids: purge, compact: true, max_purge_ratio: 1.0, ..Default::default() }) {
                    Ok(_) => runs += 1,
                    Err(_) => refused += 1,
                }
            }
            (runs, refused)
        })
    };
    let mut version = vec![0u64; files as usize];
    let started = std::time::Instant::now();
    let mut round = 0u64;
    while started.elapsed() < std::time::Duration::from_secs(20) {
        round += 1;
        let f = (round * 7919) % files;
        let v = version[f as usize];
        index.delete_by_node_id(node(f, v)).unwrap();
        index.add_document_json(node(f, v + 1), &serde_json::json!({ "content": text(f, v + 1), "name": format!("file{f}") })).unwrap();
        version[f as usize] = v + 1;
        if round % 5 == 0 {
            // An orphan: a document added then left for the maintenance to purge.
            let o = 90_000_000 + round;
            index.add_document_json(o, &serde_json::json!({ "content": format!("orphan{round} {}", text(f, v)) })).unwrap();
            orphans.lock().unwrap().push(o);
        }
        if round % 7 == 0 {
            index.commit().unwrap();
        }
    }
    index.commit().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (runs, refused) = maintenance.join().unwrap();
    index.wait_merges_quiet().unwrap();
    let check = index.check_dictionary().unwrap();
    let lost: u64 = check.fields.iter().map(|f| f.unknown).sum();
    eprintln!("rounds {round}; maintenance: {runs} run(s), {refused} refused; check: {check:?}");
    assert!(runs >= 5, "the maintenance ran several times while the files were rewritten");
    assert_eq!(lost, 0, "no word lost: {check:?}");
}
