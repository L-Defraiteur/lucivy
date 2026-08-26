//! `LUCIVY_MAX_MATCHES_PER_SEGMENT` bounds what one segment resolves for one
//! query; past it the segment's answer is truncated and the handle says so
//! through `last_search_truncated()`. The cap is read once per process, so
//! this binary sets it before anything else runs.

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

#[test]
fn a_capped_search_reports_it_and_a_normal_one_does_not() {
    std::env::set_var("LUCIVY_MAX_MATCHES_PER_SEGMENT", "40");
    let dir = std::env::temp_dir().join("lucivy_truncation_flag");
    let _ = std::fs::remove_dir_all(&dir);
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text"}], "shards": 1
    })).unwrap();
    let index = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();
    for i in 0..200u64 {
        // "e" everywhere, "zebra_gate" once.
        let text = if i == 7 { "the zebra_gate opens here".to_string() }
            else { format!("free tree bee see me {i} between the eels") };
        index.add_document_json(i, &serde_json::json!({"content": text})).unwrap();
    }
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();

    let q = |v: &str| QueryConfig {
        query_type: "contains".into(), field: Some("content".into()), value: Some(v.into()),
        ..Default::default()
    };
    // A one-letter query resolves thousands of matches: capped, and said so.
    let hits = index.search(&q("e"), 10, None).unwrap();
    assert!(!hits.is_empty());
    assert!(index.last_search_truncated(), "a one-letter query over 200 documents must hit a 40-match cap");

    // A selective query stays under the cap: the flag is cleared.
    let hits = index.search(&q("zebra_gate"), 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(!index.last_search_truncated(), "a selective query is not truncated");
    index.close().unwrap();
}
