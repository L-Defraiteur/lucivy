//! A fuzzy query costs in proportion to its terms (ticket of 7 November 2026,
//! rag3db `docs/tickets/2026-11-07-une-requete-floue-de-lucivy-coute-en-proportion-de-ses-termes.md`).
//!
//! rag3weaver sent long user texts as `contains_split` at distance 1: one fuzzy
//! `contains` per word, OR'ed. Measured in service: 197 words held the engine
//! 33 s, against 2 ms for one word, and the cost barely depends on the size of
//! the index. This witness replays that shape on an index of two documents.
//!
//! `cargo test --release -p lucivy-core --test test_fuzzy_many_terms -- --ignored --nocapture`
//! prints the curve (terms → milliseconds, distance 0 and 1) and fails when 200
//! fuzzy terms take longer than the budget.

use std::time::Instant;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

/// 200 fuzzy terms on two documents must answer within this.
const BUDGET_MS: u128 = 2_000;

fn tmp_dir(name: &str) -> String {
    let p = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.to_str().unwrap().to_string()
}

/// Distinct words of 5 to 9 letters, deterministic, varied enough not to share
/// all their trigrams (a user's text, not `word1 word2 …`).
fn words(n: usize) -> Vec<String> {
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut out: Vec<String> = Vec::with_capacity(n);
    while out.len() < n {
        let len = 5 + (next() % 5) as usize;
        let w: String = (0..len).map(|_| (b'a' + (next() % 26) as u8) as char).collect();
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

fn index() -> (ShardedHandle, Vec<String>) {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{"name": "content", "type": "text", "stored": true}],
        "shards": 1
    }))
    .unwrap();
    let handle = ShardedHandle::create(&tmp_dir("lucivy_fuzzy_many_terms"), &config).unwrap();
    // Two documents of a few hundred words each, some of them query words.
    let vocab = words(400);
    for (i, half) in vocab.chunks(200).enumerate() {
        let text = half.iter().step_by(3).cloned().collect::<Vec<_>>().join(" ");
        handle
            .add_document_json(i as u64, &serde_json::json!({"content": text}))
            .unwrap();
    }
    handle.commit().unwrap();
    (handle, vocab)
}

fn query(terms: &[String], distance: u8) -> QueryConfig {
    serde_json::from_value(serde_json::json!({
        "type": "contains_split",
        "field": "content",
        "value": terms.join(" "),
        "distance": distance
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
fn fuzzy_query_cost_is_bounded_in_its_terms() {
    let (handle, vocab) = index();
    let mut d1_at_200 = 0;
    println!("terms  d=0 ms  d=1 ms  hits(d=1)");
    for n in [1usize, 10, 25, 50, 100, 150, 200] {
        let terms = &vocab[..n];
        let (ms0, _) = time_ms(&handle, &query(terms, 0));
        let (ms1, hits) = time_ms(&handle, &query(terms, 1));
        println!("{n:5}  {ms0:6}  {ms1:6}  {hits:5}");
        if n == 200 {
            d1_at_200 = ms1;
        }
    }
    assert!(
        d1_at_200 <= BUDGET_MS,
        "200 fuzzy terms on two documents took {d1_at_200} ms (budget {BUDGET_MS} ms)"
    );
}
