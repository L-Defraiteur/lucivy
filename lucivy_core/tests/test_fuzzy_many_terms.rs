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

use std::time::Instant;

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

/// Prose: the words of lucivy's README, in order (a user's text, not
/// random letters — the fuzzy path looks its candidates up by trigrams).
fn prose() -> Vec<String> {
    include_str!("../../README.md")
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .map(|w| w.to_string())
        .collect()
}

fn index() -> (ShardedHandle, Vec<String>) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": 1
    }))
    .unwrap();
    let handle = ShardedHandle::create(&tmp_dir("lucivy_fuzzy_many_terms"), &config).unwrap();
    // Two documents of prose, the second half of the README: the query (the
    // first 200 words) shares its vocabulary with them, not its text.
    let words = prose();
    let rest = &words[200..];
    for (i, half) in rest.chunks(rest.len() / 2 + 1).take(2).enumerate() {
        handle
            .add_document_json(i as u64, &serde_json::json!({"content": half.join(" ")}))
            .unwrap();
    }
    handle.commit().unwrap();
    (handle, words)
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

fn time_ms(handle: &ShardedHandle, q: &QueryConfig) -> (u128, usize) {
    let t0 = Instant::now();
    let hits = handle.search(q, 20, None).unwrap();
    (t0.elapsed().as_millis(), hits.len())
}

#[test]
#[ignore = "timing witness: run with --release -- --ignored --nocapture"]
fn fuzzy_query_cost_is_bounded_in_its_length() {
    let (handle, words) = index();
    let mut whole_d1_at_200 = 0;
    println!("words  chars  whole d=0 ms  whole d=1 ms  by word d=1 ms");
    for n in [1usize, 4, 8, 16, 32, 64, 128, 200] {
        let terms = &words[..n];
        let chars = terms.join(" ").chars().count();
        let (w0, _) = time_ms(&handle, &whole(terms, 0));
        let (w1, _) = time_ms(&handle, &whole(terms, 1));
        let (s1, _) = time_ms(&handle, &word_by_word(terms, 1));
        println!("{n:5}  {chars:5}  {w0:12}  {w1:12}  {s1:14}");
        if n == 200 {
            whole_d1_at_200 = w1;
        }
    }
    assert!(
        whole_d1_at_200 <= BUDGET_MS,
        "a 200-word text searched whole at distance 1 on two documents took {whole_d1_at_200} ms (budget {BUDGET_MS} ms)"
    );
}
