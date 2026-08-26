//! The fuzzy score tier is the verified edit distance, not a trigram count.
//!
//! A one-edit match on a four-token query used to come out as "16 misses"
//! (`-15991` on the playground): under `pieces` mode the chain held the
//! resolved pieces, not the query's n-grams. The tier is now what the
//! verification measured — 0 for the exact text, 1 for one edit — and does
//! not depend on how many token boundaries the query crosses.

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn tier(score: f32) -> i32 { (score / 1000.0).round() as i32 }

#[test]
fn levenshtein_tier_is_the_verified_edit_distance() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text"}], "shards": 2
    })).unwrap();
    let dir = std::env::temp_dir().join("lucivy_fuzzy_tiers");
    let _ = std::fs::remove_dir_all(&dir);
    let h = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();
    let docs = [
        (1u64, "-- Test for various ALTER statements"),   // exact, 3 boundaries
        (2, "-- Test for barious ALTER statements"),      // one edit, 3 boundaries
        (3, "-- testforvariousalter statements"),         // exact, no boundary
        (4, "-- testforbariousalter statements"),         // one edit, no boundary
        (5, "spin_lock_init(&adapter->lock);"),           // unrelated
    ];
    for (id, text) in docs {
        h.add_document_json(id, &serde_json::json!({"content": text})).unwrap();
    }
    h.commit().unwrap();

    let q: QueryConfig = serde_json::from_str(
        r#"{"type":"fuzzy","field":"content","value":"test for various alter","distance":1}"#).unwrap();
    use ld_lucivy::schema::Value;
    let nid = h.field("_node_id").unwrap();
    let hits: Vec<(u64, f32)> = h.search_with_docs(&q, 10).unwrap().iter()
        .map(|hit| (hit.doc.get_first(nid).and_then(|v| v.as_u64()).unwrap(), hit.score))
        .collect();
    let of = |id: u64| hits.iter().find(|(i, _)| *i == id).map(|(_, s)| *s)
        .unwrap_or_else(|| panic!("doc {id} missing: {hits:?}"));
    assert!(!hits.iter().any(|(i, _)| *i == 5), "{hits:?}");
    assert_eq!(tier(of(1)), 0, "exact text is tier 0: {hits:?}");
    assert_eq!(tier(of(3)), 0, "exact text without boundaries is tier 0: {hits:?}");
    assert_eq!(tier(of(2)), -1, "one edit is tier -1 whatever the boundaries: {hits:?}");
    assert_eq!(tier(of(4)), -1, "one edit without boundaries is tier -1: {hits:?}");
    assert!(of(1) > of(2) && of(3) > of(4), "exact ranks above one edit: {hits:?}");
    h.close().unwrap();
}
