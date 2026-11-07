//! An exact passage of a document is found at any length (ticket of
//! 7 November 2026, `docs/tickets/2026-11-07-un-passage-exact-long-n-est-plus-trouve.md`
//! on branch `tickets-2026-11-07`).
//!
//! A `contains` of a passage copied from a document stopped finding it past
//! 42 to 54 bytes, whatever its text, on one shard as on four, strict or
//! relaxed: the chain of chunks that spells the query was cut at eight
//! positions (`MAX_CHAIN_DEPTH`, `src/suffix_fst/briques/fst_walk.rs`).
//! Pasting a sentence did not find its document.

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const DOCUMENTS: [&str; 2] = [
    include_str!("../../docs/13-09-2026/07-indexation-profil.md"),
    include_str!("../../docs/13-09-2026/09-architecture.md"),
];

fn tmp_dir(name: &str) -> String {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.to_str().unwrap().to_string()
}

fn index(shards: usize) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": shards
    }))
    .unwrap();
    let handle = ShardedHandle::create(&tmp_dir(&format!("lucivy_long_exact_{shards}")), &config).unwrap();
    for (i, text) in DOCUMENTS.iter().enumerate() {
        handle.add_document_json(i as u64, &serde_json::json!({"content": text})).unwrap();
    }
    handle.commit().unwrap();
    handle
}

/// Every prefix (on char boundaries, by steps of 4 bytes) of a 300-byte
/// passage of the first document, strict and relaxed, on `shards` shards:
/// the lengths at which the document was not found.
fn missed_lengths(shards: usize) -> Vec<String> {
    let handle = index(shards);
    let text = DOCUMENTS[0];
    let start = text.find("Le profileur").expect("the passage");
    let mut missed = Vec::new();
    for len in (4..=300).step_by(4) {
        let Some(end) = (start + len..text.len()).find(|&i| text.is_char_boundary(i)) else { break };
        let passage = &text[start..end];
        for strict in [false, true] {
            let q: QueryConfig = serde_json::from_value(serde_json::json!({
                "type": "contains", "field": "content", "value": passage, "strict_separators": strict
            }))
            .unwrap();
            let ids = handle.node_ids_of(&handle.search(&q, 10, None).unwrap()).unwrap();
            if !ids.contains(&0) {
                missed.push(format!("{}{}", end - start, if strict { "s" } else { "r" }));
            }
        }
    }
    missed
}

#[test]
fn an_exact_passage_is_found_at_every_length_on_one_shard() {
    let missed = missed_lengths(1);
    assert!(missed.is_empty(), "lengths not found (bytes, r relaxed / s strict): {missed:?}");
}

#[test]
fn an_exact_passage_is_found_at_every_length_on_four_shards() {
    let missed = missed_lengths(4);
    assert!(missed.is_empty(), "lengths not found (bytes, r relaxed / s strict): {missed:?}");
}
