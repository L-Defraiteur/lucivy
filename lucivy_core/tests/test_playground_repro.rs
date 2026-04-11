//! Reproduce the playground indexation flow natively.
//!
//! Indexes the rag3db repo files exactly like the WASM playground:
//! - Same TEXT_EXTENSIONS filter
//! - Same isBinaryContent check (null byte in first 512 bytes)
//! - Same MAX_FILE_SIZE = 100KB
//! - Same COMMIT_EVERY = 200 docs
//! - No drain_merges (WASM doesn't call it)
//! - Fields: path (text, stored, not indexed) + content (text, stored, indexed)
//!
//! The repo root defaults to the rag3db parent directory (packages/rag3db/).
//! Set RAG3DB_ROOT env var to override.

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::sync::Arc;

const COMMIT_EVERY: usize = 200;
const MAX_FILE_SIZE: u64 = 100_000;

/// Same extensions as the playground's TEXT_EXTENSIONS set.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "rs", "py", "js", "ts", "jsx", "tsx", "json", "toml",
    "yaml", "yml", "html", "htm", "css", "scss", "less", "go", "java",
    "c", "cpp", "cc", "h", "hpp", "rb", "sh", "bash", "zsh", "fish",
    "sql", "xml", "csv", "tsv", "r", "swift", "kt", "scala",
    "lua", "vim", "el", "ex", "exs", "erl", "hs", "ml", "mli",
    "clj", "lisp", "php", "pl", "pm", "tcl", "awk", "sed",
    "makefile", "cmake", "dockerfile", "gitignore", "env",
    "cfg", "ini", "conf", "properties", "lock",
];

/// Same as playground's isTextFilename
fn is_text_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    if let Some(dot_pos) = lower.rfind('.') {
        let ext = &lower[dot_pos + 1..];
        if TEXT_EXTENSIONS.contains(&ext) { return true; }
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(base, "makefile" | "dockerfile" | "readme" | "license" | "changelog" | "authors" | "cargo.lock")
}

/// Same as playground's isBinaryContent
fn is_binary_content(content: &[u8]) -> bool {
    content[..content.len().min(512)].contains(&0)
}

/// Collect files from the rag3db repo, matching the playground's exact filtering.
/// Uses RAG3DB_ROOT env var, or clones from GitHub to /tmp/test_rag3db_clone.
fn collect_repo_files() -> Vec<(String, String)> {
    let root = if let Ok(path) = std::env::var("RAG3DB_ROOT") {
        std::path::PathBuf::from(path)
    } else {
        let clone_path = std::path::Path::new("/tmp/test_rag3db_clone");
        if !clone_path.exists() {
            eprintln!("Cloning https://github.com/L-Defraiteur/rag3db.git ...");
            let status = std::process::Command::new("git")
                .args(&["clone", "--depth", "1", "https://github.com/L-Defraiteur/rag3db.git", clone_path.to_str().unwrap()])
                .status()
                .expect("git clone failed");
            assert!(status.success(), "git clone failed");
        }
        clone_path.to_path_buf()
    };

    eprintln!("Indexing from: {}", root.display());

    // No exclude dirs — the playground indexes everything from the tarball
    // (it doesn't know about .git, build dirs etc. — tarball excludes .git already)
    let exclude_dirs: Vec<&str> = vec![
        ".git",
    ];
    let mut files = Vec::new();

    fn walk(dir: &std::path::Path, exclude: &[&str],
            files: &mut Vec<(String, String)>, root: &std::path::Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip symlinks (tarball doesn't follow them)
            if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                continue;
            }
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, exclude, files, root);
                }
            } else if path.is_file() {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
                // Same extension check as playground
                if !is_text_filename(&rel) { continue; }
                // Same size check
                if let Ok(meta) = path.metadata() {
                    if meta.len() > MAX_FILE_SIZE { continue; }
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Same binary check as playground
                if is_binary_content(&bytes) { continue; }
                let content = match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if content.is_empty() { continue; }
                files.push((rel, content));
            }
        }
    }

    walk(&root, &exclude_dirs, &mut files, &root);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn test_playground_repro() {
    let files = collect_repo_files();
    eprintln!("Collected {} files", files.len());

    // Verify we got the rag3db repo (not just ld-lucivy)
    let has_cmake = files.iter().any(|(p, _)| p.contains("CMakeLists"));
    let has_extension = files.iter().any(|(p, _)| p.starts_with("extension/"));
    eprintln!("Has CMakeLists: {}, Has extension/: {}", has_cmake, has_extension);

    // Check if "librag3weaver" is in any file (the bug trigger)
    let has_librag3weaver = files.iter().any(|(_, c)| c.contains("librag3weaver"));
    eprintln!("Has 'librag3weaver' in content: {}", has_librag3weaver);

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
            }
        }
        let t = std::time::Instant::now();
        writer.commit().unwrap();
        eprintln!("  final commit after {} docs ({}ms)", files.len(), t.elapsed().as_millis());
    }
    handle.reader.reload().unwrap();
    let index_ms = t_index.elapsed().as_millis();

    let searcher = handle.reader.searcher();
    eprintln!("\nIndex: {} docs, {} segments, indexed in {}ms",
        searcher.num_docs(), searcher.segment_readers().len(), index_ms);

    // ── Test queries ──
    let test_cases: Vec<(&str, u8)> = vec![
        ("rag3weaver", 0),
        ("rag3weaver", 1),
        ("rak3weaver", 1),
        ("rag3db", 1),
        // Multi-token: d=0 works but d=1 doesn't — BUG
        ("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 0),
        ("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 1),
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
        let results = handle.search(&qconfig, 20, Some(sink.clone())).unwrap();
        let query_ms = t.elapsed().as_millis();

        eprintln!("{} results in {}ms", results.len(), query_ms);

        for (score, addr) in results.iter().take(20) {
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
                            let stripped: String = matched.chars()
                                .filter(|c| c.is_alphanumeric())
                                .collect::<String>()
                                .to_lowercase();
                            let query_lower = query_text.to_lowercase();
                            let dist = levenshtein(&stripped, &query_lower);
                            let status = if dist <= *distance as u32 { "OK" } else { "FAIL" };

                            // Show more context for FAIL
                            let ctx_start = hl[0].saturating_sub(15);
                            let ctx_end = (hl_end + 15).min(content.len());
                            let ctx_s = if ctx_start > 0 { let mut s = ctx_start; while s > 0 && !content.is_char_boundary(s) { s -= 1; } s } else { 0 };
                            let ctx_e = { let mut e = ctx_end; while e < content.len() && !content.is_char_boundary(e) { e += 1; } e };

                            eprintln!("  [{}] {} score={:.4} hl=[{},{}] len={} stripped=\"{}\" dist={} raw=\"{}\" ctx=\"{}\"",
                                status, path, score, hl[0], hl[1], hl[1] - hl[0],
                                stripped, dist,
                                matched.replace('\n', "\\n"),
                                content[ctx_s..ctx_e].replace('\n', "\\n"));
                        }
                    }
                }
            } else {
                eprintln!("  [NO HL] {} score={:.4}", path, score);
            }
        }
    }

    // === Monotonicity check: d=1 must be a superset of d=0 ===
    eprintln!("\n=== MONOTONICITY CHECK: d=0 ⊆ d=1 ===");
    for query_text in &["rag3weaver", "rak3weaver"] {
        // Collect ALL doc addresses for d=0
        let qconfig_d0 = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(query_text.to_string()),
            distance: Some(0),
            ..Default::default()
        };
        let results_d0 = handle.search(&qconfig_d0, 100_000, None).unwrap();
        let docs_d0: std::collections::HashSet<u32> = results_d0.iter()
            .map(|(_, addr)| addr.segment_ord as u32 * 100_000 + addr.doc_id)
            .collect();

        // Collect ALL doc addresses for d=1
        let qconfig_d1 = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(query_text.to_string()),
            distance: Some(1),
            ..Default::default()
        };
        let results_d1 = handle.search(&qconfig_d1, 100_000, None).unwrap();
        let docs_d1: std::collections::HashSet<u32> = results_d1.iter()
            .map(|(_, addr)| addr.segment_ord as u32 * 100_000 + addr.doc_id)
            .collect();

        let missing: Vec<_> = docs_d0.difference(&docs_d1).collect();
        eprintln!("  \"{}\": d=0 found {} docs, d=1 found {} docs, missing from d=1: {}",
            query_text, docs_d0.len(), docs_d1.len(), missing.len());
        if !missing.is_empty() {
            for &addr in missing.iter().take(5) {
                let seg = (addr / 100_000) as u32;
                let doc = addr % 100_000;
                eprintln!("    MISSING: seg={} doc={}", seg, doc);
            }
        }
        assert!(missing.is_empty(), "d=1 must be a superset of d=0 for \"{}\"", query_text);
    }
    eprintln!("  MONOTONICITY OK");
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
