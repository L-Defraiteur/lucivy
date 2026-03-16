//! Benchmark: index rag3db clone and time contains_split queries.
//! Run with: cargo test -p lucivy-core --test bench_contains -- --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use lucivy_core::directory::StdFsDirectory;
use lucivy_core::handle::{LucivyHandle, NODE_ID_FIELD};
use lucivy_core::query::{self, QueryConfig};
use lucivy_core::snapshot;
use ld_lucivy::collector::TopDocs;
use ld_lucivy::query::HighlightSink;

const RAG3DB_CLONE: &str = "/tmp/rag3db_bench";
const INDEX_DIR: &str = "/tmp/lucivy_bench_index";
const LUCE_FILE: &str = "/tmp/lucivy_bench_rag3db.luce";
const MAX_FILE_SIZE: u64 = 100_000;

fn is_text(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else { return false };
    let chunk = &data[..data.len().min(8192)];
    if chunk.contains(&0u8) { return false; }
    std::str::from_utf8(chunk).is_ok()
}

fn collect_files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let exclude = ["target", "node_modules", ".git", "build", "build_wasm", "pkg",
                   "__pycache__", ".venv", ".pytest_cache"];

    fn walk(dir: &Path, root: &Path, exclude: &[&str], files: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, root, exclude, files);
                }
            } else if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                if size > MAX_FILE_SIZE || size == 0 { continue; }
                if !is_text(&path) { continue; }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.trim().is_empty() {
                        files.push((rel, content));
                    }
                }
            }
        }
    }

    walk(root, root, &exclude, &mut files);
    files
}

fn create_index(files: &[(String, String)]) -> LucivyHandle {
    let _ = std::fs::remove_dir_all(INDEX_DIR);
    std::fs::create_dir_all(INDEX_DIR).unwrap();

    let config: query::SchemaConfig = serde_json::from_value(serde_json::json!({
        "fields": [
            {"name": "path", "type": "text", "stored": true},
            {"name": "content", "type": "text", "stored": true}
        ]
    })).unwrap();

    let dir = StdFsDirectory::open(INDEX_DIR).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();

    let path_field = handle.field("path").unwrap();
    let content_field = handle.field("content").unwrap();
    let nid_field = handle.field(NODE_ID_FIELD).unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_u64(nid_field, i as u64);
            doc.add_text(path_field, path);
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();

            if (i + 1) % 1000 == 0 {
                writer.commit().unwrap();
                eprintln!("  committed {} docs", i + 1);
            }
        }
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();
    handle
}

fn time_query(handle: &LucivyHandle, config: &QueryConfig, label: &str, with_highlights: bool) -> (usize, f64) {
    let sink = if with_highlights {
        Some(Arc::new(HighlightSink::new()))
    } else {
        None
    };

    let t0 = Instant::now();
    let query = query::build_query(
        config,
        &handle.schema,
        &handle.index,
        sink,
    ).unwrap();

    let searcher = handle.reader.searcher();
    let collector = TopDocs::with_limit(20).order_by_score();
    let results = searcher.search(&*query, &collector).unwrap();
    let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
    (results.len(), elapsed)
}

#[test]
fn bench_rag3db_contains() {
    // --- Index ---
    eprintln!("\n=== Collecting files from {} ===", RAG3DB_CLONE);
    let files = collect_files(Path::new(RAG3DB_CLONE));
    eprintln!("Found {} text files", files.len());

    eprintln!("=== Indexing ===");
    let t0 = Instant::now();
    let handle = create_index(&files);
    let ndocs = handle.reader.searcher().num_docs();
    eprintln!("Indexed {} docs in {:.1}s", ndocs, t0.elapsed().as_secs_f64());

    // --- Export .luce ---
    let snap_files = snapshot::read_directory_files(Path::new(INDEX_DIR)).unwrap();
    let snap = snapshot::SnapshotIndex { path: INDEX_DIR, files: snap_files };
    let blob = snapshot::export_snapshot(&[snap]);
    std::fs::write(LUCE_FILE, &blob).unwrap();
    eprintln!("Exported {} ({:.1} MB)\n", LUCE_FILE, blob.len() as f64 / 1024.0 / 1024.0);

    // --- Bench ---
    let queries: Vec<(&str, QueryConfig)> = vec![
        ("contains 'rag3db'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("rag3db".into()),
            ..Default::default()
        }),
        ("contains 'main'", QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some("main".into()),
            ..Default::default()
        }),
        ("contains_split 'rag3db main'", QueryConfig {
            query_type: "contains_split".into(),
            field: Some("content".into()),
            value: Some("rag3db main".into()),
            ..Default::default()
        }),
        ("contains_split 'rag3db main' d=1", QueryConfig {
            query_type: "contains_split".into(),
            field: Some("content".into()),
            value: Some("rag3db main".into()),
            distance: Some(1),
            ..Default::default()
        }),
        ("startsWith 'rag3db'", QueryConfig {
            query_type: "startsWith".into(),
            field: Some("content".into()),
            value: Some("rag3db".into()),
            ..Default::default()
        }),
        ("startsWith_split 'rag3db main'", QueryConfig {
            query_type: "startsWith_split".into(),
            field: Some("content".into()),
            value: Some("rag3db main".into()),
            ..Default::default()
        }),
        ("startsWith_split 'rag3db main' d=1", QueryConfig {
            query_type: "startsWith_split".into(),
            field: Some("content".into()),
            value: Some("rag3db main".into()),
            distance: Some(1),
            ..Default::default()
        }),
    ];

    eprintln!("{:<45} {:>6} {:>10} {:>10}", "Query", "Hits", "No HL", "With HL");
    eprintln!("{}", "-".repeat(75));

    for (label, config) in &queries {
        // Warm up
        let _ = time_query(&handle, config, label, false);

        // 3-run average
        let mut total_no_hl = 0.0;
        let mut total_hl = 0.0;
        let mut hits = 0;
        for _ in 0..3 {
            let (h, ms) = time_query(&handle, config, label, false);
            total_no_hl += ms;
            hits = h;
            let (_, ms) = time_query(&handle, config, label, true);
            total_hl += ms;
        }
        eprintln!("{:<45} {:>6} {:>8.1}ms {:>8.1}ms",
            label, hits, total_no_hl / 3.0, total_hl / 3.0);
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(INDEX_DIR);
    eprintln!("\n.luce saved at {} for reuse", LUCE_FILE);
}
