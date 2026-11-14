//! An exact passage of a document is found at any length (ticket of
//! 7 November 2026, `docs/tickets/2026-11-07-un-passage-exact-long-n-est-plus-trouve.md`
//! on branch `tickets-2026-11-07`).
//!
//! A `contains` of a passage copied from a document stopped finding it past
//! 42 to 54 bytes, whatever its text, on one shard as on four, strict or
//! relaxed: the chain of chunks that spells the query was cut at eight
//! positions (`MAX_CHAIN_DEPTH`, `src/suffix_fst/briques/fst_walk.rs`).
//! Pasting a sentence did not find its document.

mod common;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const DOCUMENTS: [&str; 2] = [
    include_str!("../../docs/13-09-2026/07-indexation-profil.md"),
    include_str!("../../docs/13-09-2026/09-architecture.md"),
];

/// The index and the scratch directory it lives in (the guard outlives the handle).
fn index(name: &str, shards: usize) -> (tempfile::TempDir, ShardedHandle) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": shards
    }))
    .unwrap();
    let scratch = common::scratch(&format!("{name}_{shards}"));
    let handle = ShardedHandle::create(scratch.path().to_str().unwrap(), &config).unwrap();
    for (i, text) in DOCUMENTS.iter().enumerate() {
        handle.add_document_json(i as u64, &serde_json::json!({"content": text})).unwrap();
    }
    handle.commit().unwrap();
    (scratch, handle)
}

/// Every prefix (on char boundaries, by steps of 4 bytes) of a 300-byte
/// passage of the first document, strict and relaxed, on `shards` shards:
/// the lengths at which the document was not found.
fn missed_lengths(shards: usize) -> Vec<String> {
    let (_scratch, handle) = index("lucivy_long_exact", shards);
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

/// **Recall of the windowed cut at distance 2** (the piece partition of a
/// query longer than 64 bytes searched near the even split only, from
/// `composite::piece_cut_positions`). Passages of the first document from 24
/// to 300 bytes, two letters changed at a third and at two thirds of the
/// passage, searched whole at distance 2 on four shards, from several starts:
/// each must come back. Prints the found counts and the time of each length.
#[test]
fn a_long_passage_with_two_typos_is_found_at_distance_two() {
    let (_scratch, handle) = index("lucivy_two_typos", 4);
    let text = DOCUMENTS[0];
    let starts: Vec<usize> = ["Le profileur", "La compaction", "Le plafond", "Les vallées"]
        .iter()
        .filter_map(|s| text.find(s))
        .collect();
    assert!(starts.len() >= 2, "passages: {starts:?}");
    let mut missed = Vec::new();
    for len in [24usize, 48, 64, 96, 128, 200, 300] {
        let mut found = 0;
        let t0 = std::time::Instant::now();
        for &start in &starts {
            let Some(end) = (start + len..text.len()).find(|&i| text.is_char_boundary(i)) else { continue };
            let mut bytes = text[start..end].as_bytes().to_vec();
            for at in [bytes.len() / 3, 2 * bytes.len() / 3] {
                if let Some(i) = (at..bytes.len()).find(|&i| bytes[i].is_ascii_lowercase()) {
                    bytes[i] = if bytes[i] == b'x' { b'y' } else { b'x' };
                }
            }
            let typo = String::from_utf8(bytes).unwrap();
            let q: QueryConfig = serde_json::from_value(serde_json::json!({
                "type": "contains", "field": "content", "value": typo, "distance": 2
            }))
            .unwrap();
            let ids = handle.node_ids_of(&handle.search(&q, 10, None).unwrap()).unwrap();
            if ids.contains(&0) {
                found += 1;
            } else {
                missed.push(format!("{len}@{start}"));
            }
        }
        eprintln!("d=2 two typos, {len:3} bytes: {found}/{} found, {} ms", starts.len(), t0.elapsed().as_millis());
    }
    assert!(missed.is_empty(), "passages with two typos not found at distance 2 (length@start): {missed:?}");
}
