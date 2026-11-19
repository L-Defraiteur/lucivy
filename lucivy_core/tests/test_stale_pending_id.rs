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
