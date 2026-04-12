//! Monotonicity test: fuzzy d=1 must be a superset of exact d=0.
//!
//! Two test suites:
//! 1. Real repo queries: edge-case cross-token substrings from the rag3db repo
//! 2. Synthetic SKU test: random alphanumeric codes in documents, verified
//!    that fuzzy search finds them all (simulates product catalog RAG)

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::collections::HashSet;

// ═══════════════════════════════════════════════════════════════════════
// 1. Real repo: cross-token edge cases
// ═══════════════════════════════════════════════════════════════════════

fn collect_repo_files() -> Vec<(String, String)> {
    let root = if let Ok(path) = std::env::var("RAG3DB_ROOT") {
        std::path::PathBuf::from(path)
    } else {
        let clone_path = std::path::Path::new("/tmp/test_rag3db_clone");
        if !clone_path.exists() {
            eprintln!("Cloning rag3db...");
            let status = std::process::Command::new("git")
                .args(&["clone", "--depth", "1", "https://github.com/L-Defraiteur/rag3db.git",
                    clone_path.to_str().unwrap()])
                .status().expect("git clone failed");
            assert!(status.success());
        }
        clone_path.to_path_buf()
    };

    let exclude = vec![".git"];
    let mut files = Vec::new();
    walk(&root, &exclude, &mut files, &root);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

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

fn is_text_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    if let Some(dot_pos) = lower.rfind('.') {
        let ext = &lower[dot_pos + 1..];
        if TEXT_EXTENSIONS.contains(&ext) { return true; }
    }
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(base, "makefile" | "dockerfile" | "readme" | "license" | "changelog" | "authors" | "cargo.lock")
}

fn walk(dir: &std::path::Path, exclude: &[&str], files: &mut Vec<(String, String)>, root: &std::path::Path) {
    let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) { continue; }
        if path.is_dir() {
            if !exclude.contains(&name.as_str()) { walk(&path, exclude, files, root); }
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
            if !is_text_filename(&rel) { continue; }
            if let Ok(meta) = path.metadata() { if meta.len() > 100_000 { continue; } }
            let bytes = match std::fs::read(&path) { Ok(b) => b, Err(_) => continue };
            if bytes[..bytes.len().min(512)].contains(&0) { continue; }
            let content = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => continue };
            if content.is_empty() { continue; }
            files.push((rel, content));
        }
    }
}

fn build_index(files: &[(String, String)], drain: bool) -> (LucivyHandle, ld_lucivy::schema::Field) {
    let tmp = if drain {
        std::path::PathBuf::from("/tmp/test_monotonicity_drained")
    } else {
        std::path::PathBuf::from("/tmp/test_monotonicity_segments")
    };
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
            query::FieldDef { name: "path".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(false), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(&tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_field = handle.field("content").unwrap();
    let path_field = handle.field("path").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for (i, (path, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
            doc.add_text(path_field, path);
            writer.add_document(doc).unwrap();
            if (i + 1) % 200 == 0 { writer.commit().unwrap(); }
        }
        writer.commit().unwrap();
        if drain { writer.drain_merges().unwrap(); }
    }
    handle.reader.reload().unwrap();

    let searcher = handle.reader.searcher();
    eprintln!("Index: {} docs, {} segments (drain={})",
        searcher.num_docs(), searcher.segment_readers().len(), drain);

    (handle, content_field)
}

fn make_contains_config(query_text: &str, distance: u8) -> QueryConfig {
    QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(query_text.to_string()),
        distance: Some(distance),
        ..Default::default()
    }
}

fn count_docs(handle: &LucivyHandle, query_text: &str, distance: u8) -> usize {
    let qconfig = make_contains_config(query_text, distance);
    handle.search(&qconfig, 100_000, None).unwrap().len()
}

fn collect_doc_addrs(handle: &LucivyHandle, query_text: &str, distance: u8) -> HashSet<(u32, u32)> {
    let qconfig = make_contains_config(query_text, distance);
    let results = handle.search(&qconfig, 100_000, None).unwrap();
    results.iter().map(|(_, addr)| (addr.segment_ord as u32, addr.doc_id)).collect()
}

/// Search and return results ordered by score (highest first).
/// Returns Vec<(score, segment_ord, doc_id)>.
fn search_ranked(handle: &LucivyHandle, query_text: &str, distance: u8, limit: usize) -> Vec<(f32, u32, u32)> {
    let qconfig = make_contains_config(query_text, distance);
    let results = handle.search(&qconfig, limit, None).unwrap();
    results.iter().map(|(score, addr)| (*score, addr.segment_ord as u32, addr.doc_id)).collect()
}

#[test]
fn test_monotonicity_real_repo() {
    let files = collect_repo_files();
    if files.is_empty() {
        eprintln!("SKIP: no repo files found");
        return;
    }
    eprintln!("Collected {} files", files.len());

    // Build with multiple segments (like WASM playground — no drain)
    let (handle, _) = build_index(&files, false);

    // Edge-case queries: cross-token substrings, mid-token starts/ends
    let queries = vec![
        // Cross CamelCase boundary (rag3 + weaver → rag3weaver)
        "rag3weaver",
        // Cross CamelCase with typo
        "rak3weaver",
        // Mid-token start: "3db" starts mid-token "rag3db"
        "3db",
        // Cross-token mid→mid: ends mid-token "rag3db", crosses gap, starts "is"
        // (only works if content has "rag3db is")
        "3db_val",
        // Compound function names
        "rag3db_value_destroy",
        // Substring of compound: starts mid-token
        "alue_dest",
        // Short fuzzy-sensitive tokens
        "S3File",
        "S3Auth",
        // Long cross-token substring
        "query_result_is_success",
    ];

    let mut any_failure = false;
    for query_text in &queries {
        let d0 = collect_doc_addrs(&handle, query_text, 0);
        let d1 = collect_doc_addrs(&handle, query_text, 1);
        let missing: Vec<_> = d0.difference(&d1).collect();

        let status = if missing.is_empty() { "OK" } else { "FAIL" };
        eprintln!("  [{}] \"{}\": d=0={} docs, d=1={} docs, missing={}",
            status, query_text, d0.len(), d1.len(), missing.len());

        if !missing.is_empty() {
            any_failure = true;
            for &(seg, doc) in missing.iter().take(5) {
                eprintln!("    MISSING seg={} doc={}", seg, doc);
            }
        }
    }
    assert!(!any_failure, "Monotonicity violated: some d=0 results missing from d=1");

    // Ranking check: for multi-word queries, d=0 results should be ranked
    // among the top results in d=1 (coverage boost should prioritize them).
    let ranking_queries = vec!["3db_val", "rag3db_value_destroy", "alue_dest", "query_result_is_success"];
    let mut ranking_failures = 0;
    let searcher = handle.reader.searcher();
    let content_field = handle.field("content").unwrap();
    let path_field = handle.field("path").unwrap();

    let get_path = |seg: u32, doc: u32| -> String {
        let addr = ld_lucivy::DocAddress::new(seg, doc);
        searcher.doc(addr).ok()
            .and_then(|d: ld_lucivy::LucivyDocument| {
                d.get_first(path_field).map(|v| {
                    let o: ld_lucivy::schema::OwnedValue = v.into();
                    match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() }
                })
            })
            .unwrap_or_else(|| format!("seg={}/doc={}", seg, doc))
    };

    for query_text in &ranking_queries {
        let d0 = collect_doc_addrs(&handle, query_text, 0);
        if d0.is_empty() { continue; }

        // Get ALL d=1 results (not just top 100)
        let ranked = search_ranked(&handle, query_text, 1, 100_000);
        if ranked.is_empty() { continue; }

        // Build score map for d=1
        let d1_scores: std::collections::HashMap<(u32, u32), (f32, usize)> = ranked.iter()
            .enumerate()
            .map(|(rank, &(score, seg, doc))| ((seg, doc), (score, rank)))
            .collect();

        // Check: all d=0 docs should appear in the top N results of d=1,
        // where N = 2 * d0.len() (generous margin).
        let top_n = (d0.len() * 2).min(ranked.len());
        let top_addrs: HashSet<(u32, u32)> = ranked[..top_n].iter()
            .map(|&(_, seg, doc)| (seg, doc))
            .collect();

        let not_in_top: Vec<_> = d0.iter()
            .filter(|addr| !top_addrs.contains(addr))
            .collect();

        if not_in_top.is_empty() {
            eprintln!("  [RANK OK] \"{}\": all {} d=0 docs in top {} of {} d=1 results (top score={:.4})",
                query_text, d0.len(), top_n, ranked.len(), ranked[0].0);
        } else {
            eprintln!("  [RANK FAIL] \"{}\": {} of {} d=0 docs NOT in top {} (total d=1={})",
                query_text, not_in_top.len(), d0.len(), top_n, ranked.len());
            // Show top 5 results
            for &(score, seg, doc) in ranked.iter().take(5) {
                let is_d0 = d0.contains(&(seg, doc));
                eprintln!("    top: seg={} doc={} score={:.4} {} | {}", seg, doc, score,
                    if is_d0 { "← d=0" } else { "" }, get_path(seg, doc));
            }
            // Show each missing d=0 doc: its rank in d=1, score, and content snippet
            eprintln!("    --- missing d=0 docs in d=1 ---");
            for &&(seg, doc) in not_in_top.iter().take(3) {
                let d1_info = d1_scores.get(&(seg, doc));
                let path = get_path(seg, doc);
                let doc_addr = ld_lucivy::DocAddress::new(seg, doc);
                let content: String = searcher.doc(doc_addr).ok()
                    .and_then(|d: ld_lucivy::LucivyDocument| {
                        d.get_first(content_field).map(|v| {
                            let o: ld_lucivy::schema::OwnedValue = v.into();
                            match o { ld_lucivy::schema::OwnedValue::Str(s) => s, _ => String::new() }
                        })
                    })
                    .unwrap_or_default();

                // Find the query text in the content (case-insensitive)
                let query_lower = query_text.to_lowercase();
                let content_lower = content.to_lowercase();
                let snippet = if let Some(pos) = content_lower.find(&query_lower) {
                    let start = pos.saturating_sub(30);
                    let end = (pos + query_text.len() + 30).min(content.len());
                    let mut s = start; while s > 0 && !content.is_char_boundary(s) { s -= 1; }
                    let mut e = end; while e < content.len() && !content.is_char_boundary(e) { e += 1; }
                    format!("...{}...", content[s..e].replace('\n', "\\n"))
                } else {
                    format!("(query not found in content, len={})", content.len())
                };

                match d1_info {
                    Some(&(score, rank)) => {
                        eprintln!("    seg={} doc={} rank={} score={:.4} | {} | {}", seg, doc, rank, score, path, snippet);
                    }
                    None => {
                        eprintln!("    seg={} doc={} NOT IN d=1! | {} | {}", seg, doc, path, snippet);
                    }
                }
            }
            ranking_failures += 1;
        }
    }

    if ranking_failures > 0 {
        eprintln!("  WARNING: {} ranking issues (d=0 docs not in top results)", ranking_failures);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 2. Synthetic SKU test
// ═══════════════════════════════════════════════════════════════════════

/// Simple deterministic pseudo-random for reproducibility (no rand crate needed)
struct SimpleRng(u64);
impl SimpleRng {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn next_range(&mut self, max: u64) -> u64 { self.next() % max }
    fn next_char(&mut self) -> char {
        let charset = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        charset[self.next_range(charset.len() as u64) as usize] as char
    }
    fn gen_sku(&mut self, len: usize) -> String {
        (0..len).map(|_| self.next_char()).collect()
    }
}

#[test]
fn test_fuzzy_sku_catalog() {
    let mut rng = SimpleRng::new(42);

    // Generate 50 random SKUs of varying length (6-12 chars)
    let skus: Vec<String> = (0..50).map(|_| {
        let len = 6 + rng.next_range(7) as usize;
        rng.gen_sku(len)
    }).collect();

    eprintln!("Generated {} SKUs: {:?}", skus.len(), &skus[..5]);

    // Create 200 "product documents" each containing 1-5 random SKUs
    let mut docs: Vec<String> = Vec::new();
    let mut sku_in_docs: Vec<Vec<usize>> = vec![Vec::new(); skus.len()]; // sku_idx → doc_indices

    for doc_idx in 0..200 {
        let num_skus = 1 + rng.next_range(5) as usize;
        let mut content = format!("Product listing #{}\n\n", doc_idx);
        for _ in 0..num_skus {
            let sku_idx = rng.next_range(skus.len() as u64) as usize;
            content.push_str(&format!("Item: {} - Price: ${}.{:02}\n",
                skus[sku_idx],
                10 + rng.next_range(990),
                rng.next_range(100)));
            sku_in_docs[sku_idx].push(doc_idx);
        }
        content.push_str("\nEnd of listing.\n");
        docs.push(content);
    }

    // Build index
    let tmp = std::path::Path::new("/tmp/test_sku_fuzzy");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_field = handle.field("content").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for content in &docs {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        // No drain_merges — test multi-segment like WASM
    }
    handle.reader.reload().unwrap();

    let searcher = handle.reader.searcher();
    eprintln!("SKU index: {} docs, {} segments", searcher.num_docs(), searcher.segment_readers().len());

    // Test each SKU: d=0 exact must find all docs containing it
    // Test d=1: must be superset of d=0
    // Test with 1-char typo: must still find the original SKU docs
    let mut exact_failures = 0;
    let mut mono_failures = 0;

    for (sku_idx, sku) in skus.iter().enumerate() {
        let expected_docs: HashSet<usize> = sku_in_docs[sku_idx].iter().copied().collect();
        if expected_docs.is_empty() { continue; }

        // d=0: should find at least the expected docs
        let d0_count = count_docs(&handle, sku, 0);
        if d0_count < expected_docs.len() {
            eprintln!("  [FAIL] SKU \"{}\" d=0: found {} but expected >= {}",
                sku, d0_count, expected_docs.len());
            exact_failures += 1;
        }

        // Monotonicity: d=1 ⊇ d=0
        let d0_addrs = collect_doc_addrs(&handle, sku, 0);
        let d1_addrs = collect_doc_addrs(&handle, sku, 1);
        let missing: Vec<_> = d0_addrs.difference(&d1_addrs).collect();
        if !missing.is_empty() {
            eprintln!("  [MONO FAIL] SKU \"{}\" d=0={} d=1={} missing={}",
                sku, d0_addrs.len(), d1_addrs.len(), missing.len());
            mono_failures += 1;
        }

        // Typo variant: change 1 char in the SKU, search d=1
        // The original docs should still be found
        if sku.len() >= 4 {
            let mut typo_sku: Vec<u8> = sku.bytes().collect();
            let pos = (rng.next_range(sku.len() as u64 - 2) + 1) as usize; // avoid first/last
            typo_sku[pos] = if typo_sku[pos] == b'X' { b'Y' } else { b'X' };
            let typo_str = String::from_utf8(typo_sku).unwrap();

            let typo_d1 = collect_doc_addrs(&handle, &typo_str, 1);
            // The original d=0 docs should be in the typo d=1 results
            let typo_missing: Vec<_> = d0_addrs.difference(&typo_d1).collect();
            if !typo_missing.is_empty() {
                eprintln!("  [TYPO FAIL] SKU \"{}\" typo \"{}\" d=1: missing {} of {} d=0 docs",
                    sku, typo_str, typo_missing.len(), d0_addrs.len());
                mono_failures += 1;
            }
        }
    }

    eprintln!("\nSKU test summary: {} SKUs, {} exact failures, {} monotonicity failures",
        skus.len(), exact_failures, mono_failures);
    assert_eq!(exact_failures, 0, "Some SKUs not found at d=0");
    assert_eq!(mono_failures, 0, "Monotonicity violated for some SKUs");
}

#[test]
fn test_fuzzy_long_api_keys() {
    let mut rng = SimpleRng::new(99);

    // Generate API key-like strings: "sk-proj-abc123def456ghi789jkl012"
    // Mix of separators (-, _) and alphanumeric segments of varying length.
    let keys: Vec<String> = (0..20).map(|_| {
        let mut key = String::from("sk-proj-");
        let num_segments = 3 + rng.next_range(4) as usize; // 3-6 segments
        for seg_idx in 0..num_segments {
            let seg_len = 4 + rng.next_range(8) as usize; // 4-11 chars per segment
            for _ in 0..seg_len {
                key.push(rng.next_char());
            }
            if seg_idx < num_segments - 1 {
                // Random separator: - or _
                key.push(if rng.next_range(2) == 0 { '-' } else { '_' });
            }
        }
        key
    }).collect();

    eprintln!("Generated {} API keys, lengths: {:?}",
        keys.len(), keys.iter().map(|k| k.len()).collect::<Vec<_>>());
    eprintln!("  Examples: {:?}", &keys[..3]);

    // Create docs with 1-3 keys each
    let mut docs: Vec<String> = Vec::new();
    let mut key_in_docs: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];

    for doc_idx in 0..100 {
        let num_keys = 1 + rng.next_range(3) as usize;
        let mut content = format!("Config file #{}\n\n", doc_idx);
        for _ in 0..num_keys {
            let key_idx = rng.next_range(keys.len() as u64) as usize;
            content.push_str(&format!("API_KEY={}\nSECRET={}\n",
                keys[key_idx],
                rng.gen_sku(8)));
            key_in_docs[key_idx].push(doc_idx);
        }
        content.push_str("\n# end config\n");
        docs.push(content);
    }

    // Build index
    let tmp = std::path::Path::new("/tmp/test_apikey_fuzzy");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_field = handle.field("content").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for content in &docs {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();

    eprintln!("API key index: {} docs", docs.len());

    let mut exact_failures = 0;
    let mut mono_failures = 0;

    for (key_idx, key) in keys.iter().enumerate() {
        let expected: HashSet<usize> = key_in_docs[key_idx].iter().copied().collect();
        if expected.is_empty() { continue; }

        let d0 = collect_doc_addrs(&handle, key, 0);
        let d1 = collect_doc_addrs(&handle, key, 1);

        if d0.len() < expected.len() {
            eprintln!("  [EXACT FAIL] key[{}] len={}: found {} expected >= {}",
                key_idx, key.len(), d0.len(), expected.len());
            exact_failures += 1;
        }

        let missing: Vec<_> = d0.difference(&d1).collect();
        if !missing.is_empty() {
            eprintln!("  [MONO FAIL] key[{}] len={}: d=0={} d=1={} missing={}",
                key_idx, key.len(), d0.len(), d1.len(), missing.len());
            mono_failures += 1;
        }

        // Typo test: change 1 char mid-key
        if key.len() >= 12 {
            let alpha_positions: Vec<usize> = key.char_indices()
                .filter(|(_, c)| c.is_alphanumeric())
                .map(|(i, _)| i)
                .collect();
            if alpha_positions.len() >= 6 {
                let pos = alpha_positions[alpha_positions.len() / 2]; // mid-key
                let mut typo: Vec<u8> = key.bytes().collect();
                typo[pos] = if typo[pos] == b'X' { b'Z' } else { b'X' };
                let typo_str = String::from_utf8(typo).unwrap();

                let typo_d1 = collect_doc_addrs(&handle, &typo_str, 1);
                let typo_missing: Vec<_> = d0.difference(&typo_d1).collect();
                if !typo_missing.is_empty() {
                    eprintln!("  [TYPO FAIL] key[{}] \"{}\" typo \"{}\" d=1: missing {} of {} d=0 docs",
                        key_idx, &key[..20], &typo_str[..20], typo_missing.len(), d0.len());
                    mono_failures += 1;
                }
            }
        }
    }

    eprintln!("\nAPI key test summary: {} keys, {} exact failures, {} monotonicity failures",
        keys.len(), exact_failures, mono_failures);
    assert_eq!(exact_failures, 0, "Some API keys not found at d=0");
    assert_eq!(mono_failures, 0, "Monotonicity violated for API keys");
}

// ═══════════════════════════════════════════════════════════════════════
// 4. Diagnostic: per-trigram miss analysis on a real file
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_diag_miss_count() {
    // Index a single file that contains "rag3db_value_destroy" exactly.
    // Diagnose which trigrams the fuzzy pipeline misses.
    let content = std::fs::read_to_string(
        std::env::var("DIAG_FILE").unwrap_or_else(|_| {
            let root = std::env::var("RAG3DB_ROOT")
                .unwrap_or_else(|_| "/tmp/test_rag3db_clone".into());
            format!("{}/test/c_api/query_result_test.cpp", root)
        })
    );
    let content = match content {
        Ok(c) => c,
        Err(_) => { eprintln!("SKIP: file not found"); return; }
    };

    let query_text = std::env::var("DIAG_QUERY")
        .unwrap_or_else(|_| "rag3db_value_destroy".into());

    eprintln!("=== DIAG: query='{}' file_len={} ===", query_text, content.len());

    // Build single-doc index
    let tmp = std::path::Path::new("/tmp/test_diag_miss");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_field = handle.field("content").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(content_field, &content);
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();

    let searcher = handle.reader.searcher();
    eprintln!("Index: {} docs, {} segments", searcher.num_docs(), searcher.segment_readers().len());

    // Call fuzzy_contains_diag directly on the segment reader
    let seg_reader = &searcher.segment_readers()[0];
    let sfx_data = seg_reader.sfx_file(content_field).expect("no sfx file");
    let sfx_bytes = sfx_data.read_bytes().unwrap();
    let sfx_reader = ld_lucivy::suffix_fst::file::SfxFileReader::open(sfx_bytes.as_ref()).unwrap();

    let pr = ld_lucivy::query::build_resolver(seg_reader, content_field).unwrap();

    let tt_bytes = seg_reader.sfx_index_file("termtexts", content_field)
        .and_then(|fs| fs.read_bytes().ok())
        .map(|b| b.as_ref().to_vec())
        .expect("no termtexts file");
    let tt_reader = ld_lucivy::suffix_fst::TermTextsReader::open(&tt_bytes)
        .expect("cannot open termtexts");
    let ord_to_term = |ord: u64| -> Option<String> { tt_reader.text(ord as u32).map(|s| s.to_string()) };

    let diag_docs: HashSet<u32> = vec![0u32].into_iter().collect(); // doc 0 = our only doc

    let (bitset, highlights, doc_coverage) = ld_lucivy::query::phrase_query::fuzzy_contains::fuzzy_contains_diag(
        &query_text, 1, &sfx_reader, pr.as_ref(), &ord_to_term,
        searcher.num_docs() as u32, &diag_docs,
    ).unwrap();

    eprintln!("\n=== RESULTS ===");
    eprintln!("bitset has doc 0: {}", bitset.contains(0));
    eprintln!("highlights for doc 0: {:?}", highlights.iter().filter(|(d, _, _)| *d == 0).collect::<Vec<_>>());
    eprintln!("doc_coverage for doc 0: {:?}", doc_coverage.iter().filter(|(d, _)| *d == 0).collect::<Vec<_>>());

    if let Some(&(_, miss_neg)) = doc_coverage.iter().find(|(d, _)| *d == 0) {
        let miss = (-miss_neg) as u32;
        eprintln!("miss_count = {}", miss);
        if miss > 0 {
            eprintln!("WARNING: doc has exact match but {} trigrams missed — see [diag] output above", miss);
        }
    }

    // Dump: what tokens exist at positions around the expected match?
    // Find byte offset of the query in the content to locate the right position range.
    let query_lower = query_text.to_lowercase();
    let content_lower = content.to_lowercase();
    if let Some(byte_pos) = content_lower.find(&query_lower) {
        eprintln!("\n=== TOKEN DUMP around byte {} (query at bytes {}-{}) ===",
            byte_pos, byte_pos, byte_pos + query_text.len());

        // Walk all ordinals in the FST and find those with postings at doc=0
        // near the byte offset of the match.
        let target_byte_start = byte_pos.saturating_sub(10) as u32;
        let target_byte_end = (byte_pos + query_text.len() + 10) as u32;

        // Iterate ordinals: the SFX FST has ordinals 0..N. Check each.
        let mut found_at_pos = Vec::new();
        let mut ord = 0u64;
        loop {
            let term = tt_reader.text(ord as u32);
            if term.is_none() { break; }
            let term_str = term.unwrap();

            let entries = pr.resolve(ord);
            for e in &entries {
                if e.doc_id == 0 && e.byte_from >= target_byte_start && e.byte_from < target_byte_end {
                    found_at_pos.push((e.position, e.byte_from, e.byte_to, ord, term_str.to_string()));
                }
            }
            ord += 1;
            if ord > 100_000 { break; } // safety
        }
        found_at_pos.sort_by_key(|&(pos, bf, _, _, _)| (pos, bf));
        for (pos, bf, bt, ord, term) in &found_at_pos {
            eprintln!("  pos={} byte=[{},{}] ord={} token='{}'", pos, bf, bt, ord, term);
        }
    }
}

#[test]
fn test_camelcase_matched_by_underscore_query() {
    // Verify: query "query_result_is_success" (with underscores) matches
    // content "queryResult.isSuccess()" (CamelCase) in exact contains (d=0).
    let tmp = std::path::Path::new("/tmp/test_camel_underscore");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef { name: "content".into(), field_type: "text".into(),
                stored: Some(true), indexed: Some(true), fast: None },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let f = handle.field("content").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        // Doc 0: CamelCase variant
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(f, "let ok = queryResult.isSuccess();");
        writer.add_document(doc).unwrap();

        // Doc 1: underscore variant (exact match)
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(f, "bool ok = query_result_is_success(&res);");
        writer.add_document(doc).unwrap();

        // Doc 2: no match
        let mut doc = ld_lucivy::LucivyDocument::new();
        doc.add_text(f, "nothing relevant here at all");
        writer.add_document(doc).unwrap();

        writer.commit().unwrap();
    }
    handle.reader.reload().unwrap();

    // d=0: exact contains "query_result_is_success"
    let d0 = count_docs(&handle, "query_result_is_success", 0);
    eprintln!("d=0 'query_result_is_success': {} docs", d0);

    // d=1: fuzzy
    let d1 = count_docs(&handle, "query_result_is_success", 1);
    eprintln!("d=1 'query_result_is_success': {} docs", d1);

    assert!(d0 >= 1, "underscore variant (doc 1) must match d=0");

    // Also test: CamelCase query matches CamelCase content in d=0?
    let d0_camel = count_docs(&handle, "queryResult.isSuccess", 0);
    eprintln!("d=0 'queryResult.isSuccess': {} docs", d0_camel);

    // And: underscore query matches CamelCase content?
    eprintln!("d=0 'query_result_is_success': {} docs (does it find CamelCase?)", d0);

    assert!(d1 >= d0, "d=1 must be superset of d=0");
}
