//! Reproduce the playground indexation flow natively.
//!
//! Indexes the rag3db repo files exactly like the WASM playground:
//! - One doc per file (path + content fields)
//! - COMMIT_EVERY = 200 docs
//! - No drain_merges (WASM doesn't call it)
//!
//! Then tests fuzzy d=1 for "rag3weaver" and "rak3weaver" with highlights.

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::sync::Arc;

const COMMIT_EVERY: usize = 200;
const MAX_FILE_SIZE: u64 = 100_000;

/// Collect files from the rag3db repo (same filtering as playground).
fn collect_rag3db_files() -> Vec<(String, String)> {
    let rag3db_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../");  // ld-lucivy → lucivy → rag3db

    // The playground indexes the parent rag3db repo, not ld-lucivy
    let root = if rag3db_root.join("extension").exists() {
        rag3db_root
    } else {
        // Fallback: index ld-lucivy itself
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").to_path_buf().into()
    };

    let exclude_dirs: Vec<&str> = vec![
        "target", "node_modules", "__pycache__", ".venv",
        ".pytest_cache", "pkg", ".git", "playground", "build",
    ];
    let text_extensions: Vec<&str> = vec![
        "txt", "md", "rs", "py", "js", "ts", "go", "java", "c", "cpp",
        "json", "toml", "yaml", "html", "css", "sh", "sql",
    ];
    let mut files = Vec::new();

    fn walk(dir: &std::path::Path, exclude: &[&str], text_ext: &[&str],
            files: &mut Vec<(String, String)>, root: &std::path::Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, exclude, text_ext, files, root);
                }
            } else if path.is_file() {
                // Check extension
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if !text_ext.contains(&ext) { continue; }
                // Check size
                if let Ok(meta) = path.metadata() {
                    if meta.len() > MAX_FILE_SIZE { continue; }
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if bytes.contains(&0) { continue; }
                let content = match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if content.trim().is_empty() { continue; }
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                files.push((rel, content));
            }
        }
    }

    walk(&root, &exclude_dirs, &text_extensions, &mut files, &root);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn test_playground_repro() {
    let files = collect_rag3db_files();
    eprintln!("Collected {} files", files.len());
    assert!(files.len() > 100, "Expected at least 100 files from repo");

    // ── Create index exactly like the playground ──
    let tmp_path = std::path::Path::new("/tmp/test_playground_repro");
    let _ = std::fs::remove_dir_all(tmp_path);
    std::fs::create_dir_all(tmp_path).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "path".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(false), fast: None },
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp_path).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();

    let path_field = handle.field("path").unwrap();
    let content_field = handle.field("content").unwrap();

    // ── Index in batches of COMMIT_EVERY, like the playground ──
    let t_index = std::time::Instant::now();
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_field, path);
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();

            if (i + 1) % COMMIT_EVERY == 0 {
                let t = std::time::Instant::now();
                writer.commit().unwrap();
                eprintln!("  commit after {} docs ({}ms)", i + 1, t.elapsed().as_millis());
                // NO drain_merges — playground doesn't do it
            }
        }
        // Final commit
        let t = std::time::Instant::now();
        writer.commit().unwrap();
        eprintln!("  final commit after {} docs ({}ms)", files.len(), t.elapsed().as_millis());
    }
    handle.reader.reload().unwrap();
    let index_ms = t_index.elapsed().as_millis();

    let searcher = handle.reader.searcher();
    let num_segments = searcher.segment_readers().len();
    eprintln!("\nIndex: {} docs, {} segments, indexed in {}ms", searcher.num_docs(), num_segments, index_ms);

    // Check SFX presence
    for (i, reader) in searcher.segment_readers().iter().enumerate() {
        let has_sfx = reader.sfx_file(content_field).is_some();
        let has_posmap = reader.posmap_file(content_field).is_some();
        let has_termtexts = reader.sfx_index_file("termtexts", content_field).is_some();
        let has_sepmap = reader.sfx_index_file("sepmap", content_field).is_some();
        let has_freqmap = reader.sfx_index_file("freqmap", content_field).is_some();
        eprintln!("  seg[{}]: {} docs, sfx={} posmap={} termtexts={} sepmap={} freqmap={}",
            i, reader.num_docs(), has_sfx, has_posmap, has_termtexts, has_sepmap, has_freqmap);
    }

    // ── Test queries ──
    let test_cases: Vec<(&str, u8)> = vec![
        ("rag3weaver", 0),    // exact contains
        ("rag3weaver", 1),    // fuzzy d=1
        ("rak3weaver", 1),    // fuzzy d=1 (substitution)
    ];

    for (query_text, distance) in &test_cases {
        eprintln!("\n=== contains \"{}\" d={} ===", query_text, distance);

        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(query_text.to_string()),
            distance: Some(*distance),
            ..Default::default()
        };
        let t = std::time::Instant::now();
        let q = query::build_query(&qconfig, &handle.schema, &handle.index, Some(sink.clone())).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(20).order_by_score();
        let results = searcher.search(&*q, &collector).unwrap();
        let query_ms = t.elapsed().as_millis();

        eprintln!("{} results in {}ms", results.len(), query_ms);

        // Show highlights for first 10 results
        for (score, addr) in results.iter().take(10) {
            let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
            let path: String = doc.get_first(path_field)
                .map(|v| { let o: ld_lucivy::schema::OwnedValue = v.into(); match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() } })
                .unwrap_or_default();
            let content: String = doc.get_first(content_field)
                .map(|v| { let o: ld_lucivy::schema::OwnedValue = v.into(); match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() } })
                .unwrap_or_default();

            let seg_id = searcher.segment_reader(addr.segment_ord as u32).segment_id();
            let hl_map = sink.get(seg_id, addr.doc_id);

            if let Some(fields) = hl_map {
                if let Some(offsets) = fields.get("content") {
                    for hl in offsets.iter().take(3) {
                        let hl_end = hl[1].min(content.len());
                        if hl[0] < content.len() && content.is_char_boundary(hl[0]) && content.is_char_boundary(hl_end) {
                            let matched = &content[hl[0]..hl_end];
                            // Strip non-alphanumeric and check distance
                            let stripped: String = matched.chars()
                                .filter(|c| c.is_alphanumeric())
                                .collect::<String>()
                                .to_lowercase();
                            let query_lower = query_text.to_lowercase();
                            let dist = levenshtein(&stripped, &query_lower);
                            let status = if dist <= *distance as u32 { "OK" } else { "FAIL" };
                            eprintln!("  [{}] {} score={:.4} hl=[{},{}] len={} stripped=\"{}\" dist={}  raw=\"{}\"",
                                status, path, score, hl[0], hl[1], hl[1] - hl[0],
                                stripped, dist,
                                matched.replace('\n', "\\n"));
                        }
                    }
                }
            } else {
                eprintln!("  [NO HL] {} score={:.4}", path, score);
            }
        }
    }
}

fn levenshtein(a: &str, b: &str) -> u32 {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let m = a.len();
    let n = b.len();
    let mut prev: Vec<u32> = (0..=n as u32).collect();
    let mut curr = vec![0u32; n + 1];
    for i in 1..=m {
        curr[0] = i as u32;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = std::cmp::min(
                std::cmp::min(curr[j - 1] + 1, prev[j] + 1),
                prev[j - 1] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}
