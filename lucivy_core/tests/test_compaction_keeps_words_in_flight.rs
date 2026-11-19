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
