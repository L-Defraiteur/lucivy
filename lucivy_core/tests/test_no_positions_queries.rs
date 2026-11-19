//! Measurement, ignored: the queries of an index without positions against the same corpus with positions —
//! the time of each query, its hits, and (under `V3_DIAG_STORED=1`) the candidates each segment read back on
//! the stored text. Ticket « le chemin sans positions relit tout pour une requête longue » (14 November 2026).
//!
//!     LUCIVY_NOPOS_CORPUS=<a source tree> LUCIVY_NOPOS_QUERIES=<json [{"label", "query"}]> \
//!     [LUCIVY_NOPOS_MAX_FILES=20000] [V3_DIAG_STORED=1] \
//!     cargo test --release -p lucivy-core --test test_no_positions_queries -- --ignored --nocapture

mod common;

use std::time::Instant;

use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;

fn build(path: &std::path::Path, files: &[(String, String)], positions: bool) -> ShardedHandle {
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [{ "name": "name", "type": "text" }, { "name": "content", "type": "text" }],
        "shards": 4,
        "positions": positions,
    })).unwrap();
    let index = ShardedHandle::create(path.to_str().unwrap(), &config).unwrap();
    let started = Instant::now();
    for (i, (name, content)) in files.iter().enumerate() {
        index.add_document_json(i as u64, &serde_json::json!({ "name": name, "content": content })).unwrap();
    }
    index.commit().unwrap();
    index.wait_merges_quiet().unwrap();
    index.wait_dictionary_folds();
    println!("positions {positions}: {} documents indexed in {:.1} s", files.len(), started.elapsed().as_secs_f64());
    index
}

#[test]
#[ignore]
fn no_positions_queries_against_positions() {
    let root = std::env::var("LUCIVY_NOPOS_CORPUS").expect("LUCIVY_NOPOS_CORPUS");
    let queries: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(std::env::var("LUCIVY_NOPOS_QUERIES").expect("LUCIVY_NOPOS_QUERIES")).unwrap()).unwrap();
    let max_files = std::env::var("LUCIVY_NOPOS_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let walked = common::corpus::walk(std::path::Path::new(&root), &common::corpus::Rules {
        exclude_dirs: &["target", "node_modules", ".git"],
        extensions: None,
        max_size: 200_000,
        skip_zero_size: true,
        skip_binary: true,
        skip_blank: true,
        max_files,
        relative: true,
    });
    println!("{}", walked.report(std::path::Path::new(&root)));
    let scratch = common::scratch("no-positions-queries");
    let with = build(&scratch.path().join("positions"), &walked.files, true);
    let without = build(&scratch.path().join("no-positions"), &walked.files, false);
    println!("query\tpositions ms\thits\tno positions ms\thits");
    for entry in &queries {
        let label = entry["label"].as_str().unwrap();
        let q: QueryConfig = serde_json::from_value(entry["query"].clone()).unwrap();
        let time = |index: &ShardedHandle| {
            let t = Instant::now();
            let n = index.search(&q, 20, None).unwrap().len();
            (t.elapsed().as_secs_f64() * 1e3, n)
        };
        let (a, na) = time(&with);
        eprintln!("---- {label}, no positions:");
        let (b, nb) = time(&without);
        println!("{label}\t{a:.1}\t{na}\t{b:.1}\t{nb}");
    }
}
