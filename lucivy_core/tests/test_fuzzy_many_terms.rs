//! A fuzzy query costs in proportion to its length (ticket of 7 November 2026,
//! rag3db `docs/tickets/2026-11-07-une-requete-floue-de-lucivy-coute-en-proportion-de-ses-termes.md`).
//!
//! rag3weaver searched a user's whole text as one relaxed `contains` at
//! distance 1. Measured by its session « recherche » (sonde_requete_longue,
//! closed product ticket « le plein texte explose sur une requête longue »):
//! 197 words, 1 397 characters, took 29 s on a base of two documents and 112 s
//! on 65 946, against 0 to 132 ms at distance 0 — the cost follows the fuzzy
//! needle, not the index. One fuzzy `contains` per word (`contains_split`)
//! stays cheap. This witness replays the whole-text shape on two documents of
//! prose.
//!
//! `cargo test --release -p lucivy-core --test test_fuzzy_many_terms -- --ignored --nocapture`
//! prints the curve (words, characters → milliseconds, whole text at distance
//! 0 and 1, word by word at distance 1) and fails when the 200-word fuzzy
//! whole text takes longer than the budget.

use std::sync::Arc;
use std::time::Instant;

use ld_lucivy::query::HighlightSink;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

/// A 200-word text searched whole at distance 1, on two documents, must answer within this.
const BUDGET_MS: u128 = 2_000;

fn tmp_dir(name: &str) -> String {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.to_str().unwrap().to_string()
}

/// The query: the distinct words of a page of French prose, three letters or
/// more, lowercased, in order of appearance — what « recherche »'s probe sent
/// (a user's prompt, not identifiers).
fn query_words() -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    include_str!("../../docs/13-09-2026/10-knowledge-dump.md")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// The two documents: two other pages of the same prose (same vocabulary,
/// different text), as the two notes of the base the probe measured.
const DOCUMENTS: [&str; 2] = [
    include_str!("../../docs/13-09-2026/07-indexation-profil.md"),
    include_str!("../../docs/13-09-2026/09-architecture.md"),
];

fn index() -> (ShardedHandle, Vec<String>) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": 4
    }))
    .unwrap();
    let handle = ShardedHandle::create(&tmp_dir("lucivy_fuzzy_many_terms"), &config).unwrap();
    for (i, text) in DOCUMENTS.iter().enumerate() {
        handle
            .add_document_json(i as u64, &serde_json::json!({"content": text}))
            .unwrap();
    }
    handle.commit().unwrap();
    (handle, query_words())
}

fn whole(terms: &[String], distance: u8) -> QueryConfig {
    serde_json::from_value(serde_json::json!({
        "type": "contains", "field": "content", "value": terms.join(" "), "distance": distance
    }))
    .unwrap()
}

fn word_by_word(terms: &[String], distance: u8) -> QueryConfig {
    serde_json::from_value(serde_json::json!({
        "type": "contains_split", "field": "content", "value": terms.join(" "), "distance": distance
    }))
    .unwrap()
}

/// One search, timed; `highlights` passes a sink, as rag3weaver always does
/// (`fts_handle::search_hits_with_text`).
fn time_ms(handle: &ShardedHandle, q: &QueryConfig, highlights: bool) -> (u128, usize) {
    let sink = highlights.then(|| Arc::new(HighlightSink::new()));
    let t0 = Instant::now();
    let hits = handle.search(q, 30, sink).unwrap();
    (t0.elapsed().as_millis(), hits.len())
}

#[test]
#[ignore = "timing witness: run with --release -- --ignored --nocapture"]
fn fuzzy_query_cost_is_bounded_in_its_length() {
    let (handle, words) = index();
    assert!(words.len() >= 200, "the page has {} distinct words, 200 are needed", words.len());
    let mut whole_d1_at_200 = 0;
    println!("words  chars  whole d=0  whole d=1  whole d=1+hl  by word d=1+hl   (ms)");
    for n in [1usize, 4, 8, 16, 32, 64, 128, 200] {
        let terms = &words[..n];
        let chars = terms.join(" ").chars().count();
        let (w0, _) = time_ms(&handle, &whole(terms, 0), true);
        let (w1, _) = time_ms(&handle, &whole(terms, 1), false);
        let (w1h, _) = time_ms(&handle, &whole(terms, 1), true);
        let (s1h, _) = time_ms(&handle, &word_by_word(terms, 1), true);
        println!("{n:5}  {chars:5}  {w0:9}  {w1:9}  {w1h:12}  {s1h:14}");
        if n == 200 {
            whole_d1_at_200 = w1h;
        }
    }
    assert!(
        whole_d1_at_200 <= BUDGET_MS,
        "a 200-word text searched whole at distance 1 on two documents took {whole_d1_at_200} ms (budget {BUDGET_MS} ms)"
    );
}

/// **A long fuzzy query still finds what it should.** Above 64 bytes the
/// piece partition is searched near the even split only; any partition
/// keeps the pigeonhole sound, so recall must not move. A passage of the
/// first document, 300 bytes or more, with one letter changed, searched whole
/// at distance 1 on four shards: the first document comes back, with a span.
#[test]
fn long_fuzzy_query_finds_its_passage() {
    let (handle, _) = index();
    let text = DOCUMENTS[0];
    // A passage of plain prose (ASCII letters and spaces), long enough.
    let start = text.find("Le ").unwrap_or(0);
    let mut end = start;
    let mut taken = 0;
    for (i, c) in text[start..].char_indices() {
        end = start + i + c.len_utf8();
        taken += 1;
        if taken >= 300 {
            break;
        }
    }
    let passage = &text[start..end];
    // One substitution in the middle of the passage, on an ASCII letter.
    let mut bytes = passage.as_bytes().to_vec();
    let mid = (bytes.len() / 2..bytes.len()).find(|&i| bytes[i].is_ascii_lowercase()).unwrap();
    bytes[mid] = if bytes[mid] == b'x' { b'y' } else { b'x' };
    let typo = String::from_utf8(bytes).unwrap();
    let q = whole(&[typo], 1);
    let sink = Arc::new(HighlightSink::new());
    let hits = handle.search(&q, 10, Some(sink.clone())).unwrap();
    let ids = handle.node_ids_of(&hits).unwrap();
    assert!(ids.contains(&0), "the passage at distance 1 was not found: {ids:?}");
}

