//! A long fuzzy query on an index without positions reads windows, not whole values (ticket « le chemin sans
//! positions relit tout pour une requête longue », 14 November 2026).
//!
//! The query is cut into `d + 1` pieces, and every occurrence holds one of them intact (the pigeonhole). A
//! piece can be short and common: on the code index of a repository the 230-byte query of the archivist was
//! cut `ledic | …`, `ledic` occurs in a third of the documents, and every one of them ran through the edit
//! distance of a 230-byte needle, value after value — 23 MB and 6.5 s per segment, for no hit. The pieces
//! say where an occurrence can be: the distance runs only around their occurrences, and finds the same
//! occurrences with the same spans.

mod common;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

const QUERY: &str = "le dictionnaire partagé du shard replie les textes neufs au commit puis la fusion des segments garde les mots nouveaux";

fn documents() -> Vec<(u64, String)> {
    let words = ["alpha", "segment", "commit", "lecture", "texte", "index", "valeur", "champ", "requête", "fusion"];
    let head = &QUERY[..QUERY.len() * 6 / 10];
    (0..40u64).map(|i| {
        let mut t = String::new();
        for k in 0..900u64 {
            t.push_str(words[((i * 7 + k * 13) % words.len() as u64) as usize]);
            t.push(' ');
            if k % 150 == 0 { t.push_str(head); t.push_str(" autre suite "); }
        }
        if i == 7 {
            // The whole query, one edit away (« neuf » for « neufs »).
            t.push_str(&QUERY.replace("neufs", "neuf"));
        }
        (i, t)
    }).collect()
}

fn build(path: &std::path::Path, docs: &[(u64, String)], positions: bool) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "body", "type": "text" }],
        "shards": 1,
        "positions": positions,
    })).unwrap();
    let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
    for (id, text) in docs {
        index.add_document_json(*id, &serde_json::json!({ "body": text })).unwrap();
    }
    index.commit().unwrap();
    index
}

fn hits(index: &ShardedHandle, q: &QueryConfig) -> Vec<(u64, Vec<(usize, usize)>)> {
    use ld_lucivy::schema::OwnedValue;
    use ld_lucivy::Document;
    let node_field = index.shard(0).unwrap().field(lucivy_core::handle::NODE_ID_FIELD).unwrap();
    let mut out: Vec<(u64, Vec<(usize, usize)>)> = index.search_with_docs(q, 100).unwrap().into_iter().map(|hit| {
        let id = hit.doc.iter_fields_and_values()
            .find_map(|(f, v)| match v.into() { OwnedValue::U64(n) if f == node_field => Some(n), _ => None })
            .unwrap();
        let mut spans: Vec<(usize, usize)> = hit.highlights.values().flatten().map(|s| (s[0], s[1])).collect();
        spans.sort();
        (id, spans)
    }).collect();
    out.sort();
    out
}

#[test]
fn a_long_fuzzy_query_without_positions_reads_windows_and_finds_the_same() {
    // The byte count is a profiling counter: on for this test binary only.
    std::env::set_var("V3_PROFILE", "1");
    let scratch = common::scratch("no-positions-long-fuzzy");
    let docs = documents();
    let total: usize = docs.iter().map(|(_, t)| t.len()).sum();
    let with = build(&scratch.path().join("positions"), &docs, true);
    let without = build(&scratch.path().join("no-positions"), &docs, false);
    let q: QueryConfig = serde_json::from_value(serde_json::json!({
        "type": "contains", "field": "body", "value": QUERY, "distance": 1
    })).unwrap();
    let expected = hits(&with, &q);
    assert_eq!(expected.iter().map(|h| h.0).collect::<Vec<_>>(), vec![7], "with positions: the document one edit away");
    ld_lucivy::suffix_fst::briques::profile::reset();
    let found = hits(&without, &q);
    let read = ld_lucivy::suffix_fst::briques::profile::stored_dp_bytes() as usize;
    assert_eq!(found, expected, "without positions: the same documents and spans");
    println!("edit distance over {read} bytes of {total} stored");
    assert!(read * 10 < total, "the edit distance ran over {read} bytes of {total}: the whole values, not windows around the pieces");
}
