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
use std::sync::Arc;

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
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(&tmp).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();
    let content_field = handle.field("content").unwrap();

    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        for (i, (_, content)) in files.iter().enumerate() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
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

fn count_docs(handle: &LucivyHandle, query_text: &str, distance: u8) -> usize {
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(query_text.to_string()),
        distance: Some(distance),
        ..Default::default()
    };
    let q = query::build_query(&qconfig, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let results = searcher.search(&*q,
        &ld_lucivy::collector::TopDocs::with_limit(100_000).order_by_score()).unwrap();
    results.len()
}

fn collect_doc_addrs(handle: &LucivyHandle, query_text: &str, distance: u8) -> HashSet<(u32, u32)> {
    let qconfig = QueryConfig {
        query_type: "contains".into(),
        field: Some("content".into()),
        value: Some(query_text.to_string()),
        distance: Some(distance),
        ..Default::default()
    };
    let q = query::build_query(&qconfig, &handle.schema, &handle.index, None).unwrap();
    let searcher = handle.reader.searcher();
    let results = searcher.search(&*q,
        &ld_lucivy::collector::TopDocs::with_limit(100_000).order_by_score()).unwrap();
    results.iter().map(|(_, addr)| (addr.segment_ord as u32, addr.doc_id)).collect()
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
