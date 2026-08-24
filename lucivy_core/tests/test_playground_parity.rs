//! Native reference for the browser playground: same corpus (a file list),
//! same schema and sharding as `playground/index.html`, same query panel
//! (`playground/parity_panel.json`). Writes counts and top hits to JSON so
//! the WASM run can be diffed against it (see `playground/parity_run.js`).
//!
//!   PARITY_ROOT=/tmp/linux-bench PARITY_LIST=/tmp/corpus_indexed.list \
//!   PARITY_OUT=/tmp/parity_native.json \
//!   cargo test --release -p lucivy-core --test test_playground_parity -- --ignored --nocapture

use ld_lucivy::schema::Value;
use lucivy_core::query::{QueryConfig, SchemaConfig};
use lucivy_core::sharded_handle::ShardedHandle;
use std::path::Path;
use std::time::Instant;

const COMMIT_EVERY: usize = 2000;

#[test]
#[ignore]
fn playground_parity_native() {
    let root = std::env::var("PARITY_ROOT").expect("PARITY_ROOT");
    let list = std::env::var("PARITY_LIST").expect("PARITY_LIST");
    let out = std::env::var("PARITY_OUT").unwrap_or_else(|_| "/tmp/parity_native.json".into());
    let panel_path = std::env::var("PARITY_PANEL").unwrap_or_else(|_| {
        format!("{}/../playground/parity_panel.json", env!("CARGO_MANIFEST_DIR"))
    });
    let limit: usize = std::env::var("PARITY_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);

    // Same config the playground sends to `lucivy.create`.
    let config: SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text"},
            {"name": "content", "type": "text"},
            {"name": "extension", "type": "text"}
        ],
        "shards": 4
    }))
    .unwrap();

    let dir = std::env::temp_dir().join("lucivy_parity_native");
    let _ = std::fs::remove_dir_all(&dir);
    let h = ShardedHandle::create(dir.to_str().unwrap(), &config).unwrap();

    let max_docs: usize = std::env::var("PARITY_MAX_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let paths: Vec<String> = std::fs::read_to_string(&list)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .take(max_docs)
        .map(|s| s.to_string())
        .collect();
    let t0 = Instant::now();
    for (i, rel) in paths.iter().enumerate() {
        let content = std::fs::read(Path::new(&root).join(rel)).unwrap();
        let content = String::from_utf8_lossy(&content).into_owned();
        let ext = rel.rsplit('.').next().filter(|_| rel.contains('.')).unwrap_or("");
        h.add_document_json(
            i as u64,
            &serde_json::json!({"path": rel, "content": content, "extension": ext}),
        )
        .unwrap();
        if (i + 1) % COMMIT_EVERY == 0 {
            h.commit().unwrap();
            eprintln!("[parity] {}/{} committed ({:.1}s)", i + 1, paths.len(), t0.elapsed().as_secs_f64());
        }
    }
    h.commit().unwrap();
    // The playground's drainMerges is a second commit in the binding.
    h.commit().unwrap();
    eprintln!(
        "[parity] indexed {} docs in {:.1}s ({} in the handle)",
        paths.len(),
        t0.elapsed().as_secs_f64(),
        h.num_docs()
    );

    let panel: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&panel_path).unwrap()).unwrap();
    let nid_field = h.field("_node_id").unwrap();
    let mut report = Vec::new();
    for entry in &panel {
        let name = entry["name"].as_str().unwrap();
        let query: QueryConfig = serde_json::from_value(entry["query"].clone()).unwrap();
        let warnings = h.query_warnings(&query);
        let t = Instant::now();
        let hits = match h.search_with_docs(&query, limit) {
            Ok(h) => h,
            Err(e) => {
                report.push(serde_json::json!({"name": name, "error": e}));
                continue;
            }
        };
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let top: Vec<serde_json::Value> = hits
            .iter()
            .take(10)
            .map(|hit| {
                let node_id = hit.doc.get_first(nid_field).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                let spans: usize = hit.highlights.values().map(|v| v.len()).sum();
                serde_json::json!({"node_id": node_id, "score": hit.score, "spans": spans})
            })
            .collect();
        eprintln!("[parity] {name:40} {:6} hits {ms:8.1}ms", hits.len());
        report.push(serde_json::json!({
            "name": name, "count": hits.len(), "ms": ms, "top": top, "warnings": warnings
        }));
    }
    std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("[parity] written {out}");
    h.close().unwrap();
}
