//! Test: index real repo files natively, trigger merge, verify contains search.
//! This bypasses the Python binding to diagnose merge SFX issues directly.

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::sync::Arc;

fn collect_repo_files() -> Vec<(String, String)> {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let exclude_dirs: Vec<&str> = vec!["target", "node_modules", "__pycache__", ".venv",
        ".pytest_cache", "pkg", ".git", "playground"];
    let mut files = Vec::new();

    fn walk(dir: &std::path::Path, exclude: &[&str], files: &mut Vec<(String, String)>, root: &std::path::Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !exclude.contains(&name.as_str()) {
                    walk(&path, exclude, files, root);
                }
            } else if path.is_file() {
                if let Ok(meta) = path.metadata() {
                    if meta.len() > 100_000 { continue; }
                }
                // Check if text file
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

    walk(&repo_root, &exclude_dirs, &mut files, &repo_root);
    files
}

#[test]
fn test_merge_contains_correctness() {
    let files = collect_repo_files();
    let rag3_count = files.iter().filter(|(_, c)| c.to_lowercase().contains("rag3weaver")).count();
    eprintln!("Collected {} files, {} contain 'rag3weaver'", files.len(), rag3_count);

    // Create index in temp dir
    let tmp_path = std::path::Path::new("/tmp/test_merge_contains_with");
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

    // Add all docs
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let path_field = handle.field("path").unwrap();
        let content_field = handle.field("content").unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(path_field, path);
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
        }
        eprintln!("Added {} docs, committing...", files.len());
        writer.commit().unwrap();
        eprintln!("Committed. Draining merges...");
        writer.drain_merges().unwrap();
        eprintln!("Merges drained.");
    }
    handle.reader.reload().unwrap();

    // Count segments
    let searcher = handle.reader.searcher();
    let num_segments = searcher.segment_readers().len();
    let num_docs = searcher.num_docs();
    eprintln!("Index: {} docs, {} segments", num_docs, num_segments);

    // Check SFX presence per segment
    let content_field = handle.field("content").unwrap();
    for (i, reader) in searcher.segment_readers().iter().enumerate() {
        let has_sfx = reader.sfx_file(content_field).is_some();
        let has_sfxpost = reader.sfxpost_file(content_field).is_some();
        let has_posmap = reader.posmap_file(content_field).is_some();
        eprintln!("  segment[{}]: {} docs, sfx={} sfxpost={} posmap={}",
            i, reader.num_docs(), has_sfx, has_sfxpost, has_posmap);
    }

    // Per-segment diagnostic for "rag3weaver"
    eprintln!("\n=== Per-segment diagnostic for 'rag3weaver' ===");
    for (i, reader) in searcher.segment_readers().iter().enumerate() {
        // Check if the SFX FST can find the suffix "rag3weaver"
        if let Some(sfx_slice) = reader.sfx_file(content_field) {
            if let Ok(sfx_bytes) = sfx_slice.read_bytes() {
                if let Ok(sfx_reader) = ld_lucivy::suffix_fst::file::SfxFileReader::open(sfx_bytes.as_ref()) {
                    let fst = sfx_reader.fst();
                    // Try to find "rag3weaver" as a key in the FST
                    eprintln!("  seg[{}]: {} docs, sfx OK", i, reader.num_docs());
                }
            }
        }
    }
    eprintln!();

    // Search
    let test_queries = ["rag3weaver", "weaver", "rag3db", "rag3", "search"];
    for q in &test_queries {
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            ..Default::default()
        };
        let query = query::build_query(&qconfig, &handle.schema, &handle.index, None).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(1000).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        let expected = if *q == "rag3weaver" { rag3_count } else { 0 };
        eprintln!("contains \"{}\": {} results (expected >= {})", q, results.len(), expected);
    }

    // Now test WITHOUT merge: create a second index, don't drain_merges
    let tmp2_path = std::path::Path::new("/tmp/test_merge_contains_without");
    let _ = std::fs::remove_dir_all(tmp2_path);
    std::fs::create_dir_all(tmp2_path).unwrap();
    let dir2 = StdFsDirectory::open(tmp2_path).unwrap();
    let handle2 = LucivyHandle::create(dir2, &config).unwrap();
    {
        let mut guard = handle2.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let content_field = handle2.field("content").unwrap();
        for (_path, content) in files.iter() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        // NO drain_merges!
    }
    handle2.reader.reload().unwrap();
    let searcher2 = handle2.reader.searcher();
    eprintln!("\nWithout drain_merges: {} docs, {} segments",
        searcher2.num_docs(), searcher2.segment_readers().len());

    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some("rag3weaver".to_string()),
        ..Default::default()
    };
    let query = query::build_query(&qconfig, &handle2.schema, &handle2.index, None).unwrap();
    let collector = ld_lucivy::collector::TopDocs::with_limit(1000).order_by_score();
    let results = searcher2.search(&*query, &collector).unwrap();
    eprintln!("contains \"rag3weaver\" (no merge): {} results", results.len());

    // === Multi-token d=0 tests: WITH merge vs WITHOUT merge ===
    eprintln!("\n=== Multi-token contains d=0: WITH merge ===");
    let multi_queries = [
        "use rag3weaver",
        "rag3weaver for",
        "weaver for search",
        "3weaver for search",
        "3weaver for",
        "use rag3weaver for search",
        "rag3weaver for search",
    ];
    for q in &multi_queries {
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            ..Default::default()
        };
        let query = query::build_query(&qconfig, &handle.schema, &handle.index, None).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(1000).order_by_score();
        let results = searcher.search(&*query, &collector).unwrap();
        eprintln!("  WITH merge  d=0 \"{}\": {} results", q, results.len());
    }

    eprintln!("\n=== Multi-token contains d=0: WITHOUT merge ===");
    for q in &multi_queries {
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(q.to_string()),
            ..Default::default()
        };
        let query = query::build_query(&qconfig, &handle2.schema, &handle2.index, None).unwrap();
        let collector = ld_lucivy::collector::TopDocs::with_limit(1000).order_by_score();
        let results = searcher2.search(&*query, &collector).unwrap();
        eprintln!("  WITHOUT merge d=0 \"{}\": {} results", q, results.len());
    }

    // Ground truth: count how many files actually contain each sub-query
    eprintln!("\n=== Ground truth (brute force string search) ===");
    for q in &multi_queries {
        let count = files.iter().filter(|(_, c)| c.to_lowercase().contains(&q.to_lowercase())).count();
        eprintln!("  brute-force \"{}\": {} files", q, count);
    }

    // Detailed miss analysis for "use rag3weaver"
    eprintln!("\n=== Miss analysis: 'use rag3weaver' ===");
    let q = "use rag3weaver";
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(q.to_string()),
        ..Default::default()
    };
    let query_obj = query::build_query(&qconfig, &handle.schema, &handle.index, None).unwrap();
    let collector = ld_lucivy::collector::TopDocs::with_limit(1000).order_by_score();
    let results_with = searcher.search(&*query_obj, &collector).unwrap();

    // Retrieve paths of found docs
    let path_field = handle.field("path").unwrap();
    let mut found_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, addr) in &results_with {
        let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
        if let Some(v) = doc.get_first(path_field) {
            let owned: ld_lucivy::schema::OwnedValue = v.into();
            if let ld_lucivy::schema::OwnedValue::Str(s) = owned {
                found_paths.insert(s);
            }
        }
    }

    // Compare with brute-force
    let expected_paths: Vec<&str> = files.iter()
        .filter(|(_, c)| c.to_lowercase().contains(&q.to_lowercase()))
        .map(|(p, _)| p.as_str())
        .collect();

    for path in &expected_paths {
        if found_paths.contains(*path) {
            eprintln!("  HIT  {}", path);
        } else {
            let content = &files.iter().find(|(p, _)| p == path).unwrap().1;
            let lower = content.to_lowercase();
            let pos = lower.find(&q.to_lowercase()).unwrap();
            let start = pos.saturating_sub(10);
            let end = (pos + q.len() + 30).min(content.len());
            // Safe char boundaries
            let mut s = start;
            while s > 0 && !content.is_char_boundary(s) { s -= 1; }
            let mut e = end;
            while e < content.len() && !content.is_char_boundary(e) { e += 1; }
            eprintln!("  MISS {} — context: {:?}", path, &content[s..e]);
        }
    }
}
