//! Jaro-Winkler as the fuzzy validation metric, end to end through the
//! JSON query config: `{"type":"fuzzy","fuzzy_metric":"jaro_winkler",
//! "min_similarity":0.9}`.
//!
//! The candidates are the trigram pigeonhole's at `distance` (2 by default
//! for this metric); Jaro-Winkler then decides. So a typo at the end of the
//! word (`kmallok`) scores above one at the start (`xmalloc`), which
//! Levenshtein cannot tell apart, and a threshold of 0.99 keeps the exact
//! word only.

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn ids(h: &ShardedHandle, q: &QueryConfig) -> Vec<(u64, f32)> {
    use ld_lucivy::schema::Value;
    let nid = h.field("_node_id").unwrap();
    h.search_with_docs(q, 100)
        .unwrap()
        .iter()
        .map(|hit| (hit.doc.get_first(nid).and_then(|v| v.as_u64()).unwrap_or(u64::MAX), hit.score))
        .collect()
}

fn q(json: &str) -> QueryConfig {
    serde_json::from_str(json).unwrap()
}

#[test]
fn jaro_winkler_fuzzy_end_to_end() {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text"}],
        "shards": 2
    }))
    .unwrap();
    let dir = std::env::temp_dir().join("lucivy_fuzzy_jw");
    let _ = std::fs::remove_dir_all(&dir);
    let h = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();

    let docs = [
        (1u64, "void *buf = kmalloc(size, GFP_KERNEL);"),   // exact
        (2, "void *buf = kmallok(size, GFP_KERNEL);"),      // typo at the end
        (3, "void *buf = xmalloc(size, GFP_KERNEL);"),      // typo at the start
        (4, "void *buf = kmaloc(size, GFP_KERNEL);"),       // one deletion
        (5, "spin_lock_init(&adapter->lock);"),             // unrelated
    ];
    for (id, text) in docs {
        h.add_document_json(id, &serde_json::json!({"content": text})).unwrap();
    }
    h.commit().unwrap();

    // Levenshtein d=1: exact, end typo, start typo, deletion — all at one edit.
    let lev = ids(&h, &q(r#"{"type":"fuzzy","field":"content","value":"kmalloc","distance":1}"#));
    let lev_ids: Vec<u64> = lev.iter().map(|(i, _)| *i).collect();
    for want in [1u64, 2, 3, 4] {
        assert!(lev_ids.contains(&want), "levenshtein d=1 misses doc {want}: {lev_ids:?}");
    }
    assert!(!lev_ids.contains(&5));

    // Jaro-Winkler at 0.9: same recall here, but the order tells the typos apart.
    let jw = ids(&h, &q(r#"{"type":"fuzzy","field":"content","value":"kmalloc","fuzzy_metric":"jaro_winkler","min_similarity":0.9}"#));
    let jw_ids: Vec<u64> = jw.iter().map(|(i, _)| *i).collect();
    assert!(!jw_ids.contains(&5), "unrelated doc kept: {jw:?}");
    assert!(jw_ids.contains(&1) && jw_ids.contains(&2) && jw_ids.contains(&4), "{jw:?}");
    let pos = |id: u64| jw_ids.iter().position(|&x| x == id).unwrap_or(usize::MAX);
    assert_eq!(pos(1), 0, "the exact word ranks first: {jw:?}");
    assert!(pos(2) < pos(3), "a typo at the end ranks above one at the start: {jw:?}");

    // At 0.99 only the exact word survives.
    let strict = ids(&h, &q(r#"{"type":"fuzzy","field":"content","value":"kmalloc","fuzzy_metric":"jaro_winkler","min_similarity":0.99}"#));
    assert_eq!(strict.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1], "{strict:?}");

    // Highlights come from the chosen window.
    let hits = h.search_with_docs(
        &q(r#"{"type":"fuzzy","field":"content","value":"kmalloc","fuzzy_metric":"jaro_winkler"}"#), 100).unwrap();
    let with_spans = hits.iter().filter(|hit| hit.highlights.values().any(|v| !v.is_empty())).count();
    assert_eq!(with_spans, hits.len(), "every hit carries a span");

    // An unknown metric is refused, not silently ignored.
    assert!(h.search_with_docs(&q(r#"{"type":"fuzzy","field":"content","value":"kmalloc","fuzzy_metric":"soundex"}"#), 10).is_err());

    h.close().unwrap();
}
