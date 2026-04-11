//! Ground truth test for fuzzy contains search.
//!
//! Indexes real repo files, builds ground truth via brute-force scan
//! (tokenize with CamelCaseSplit + lowercase, concatenate, semi-global
//! Levenshtein), then compares with the fuzzy query results.
//!
//! Validates:
//! 1. Recall — all ground truth docs are found
//! 2. Precision — no false positives
//! 3. Highlights — stripped of separators, within Levenshtein distance

use lucivy_core::handle::LucivyHandle;
use lucivy_core::query::{self, QueryConfig, SchemaConfig};
use lucivy_core::directory::StdFsDirectory;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ─── Ground truth tokenization ─────────────────────────────────────────────
// Reimplements raw_code tokenizer logic: SimpleTokenizer + CamelCaseSplit + LowerCase

/// SimpleTokenizer: split on non-alphanumeric.
fn simple_tokenize(text: &str) -> Vec<(usize, usize, &str)> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if !c.is_alphanumeric() {
            chars.next();
            continue;
        }
        let start = i;
        let mut end = i + c.len_utf8();
        chars.next();
        while let Some(&(j, c2)) = chars.peek() {
            if !c2.is_alphanumeric() {
                break;
            }
            end = j + c2.len_utf8();
            chars.next();
        }
        tokens.push((start, end, &text[start..end]));
    }
    tokens
}

/// CamelCaseSplit boundaries (same logic as camel_case_split.rs).
fn find_boundaries(text: &str) -> Vec<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut boundaries = vec![0usize];
    for i in 1..chars.len() {
        let (byte_pos, cur) = chars[i];
        let (_, prev) = chars[i - 1];
        let split =
            (prev.is_lowercase() && cur.is_uppercase())
            || (i + 1 < chars.len() && cur.is_uppercase()
                && prev.is_uppercase() && chars[i + 1].1.is_lowercase())
            || (prev.is_alphabetic() && cur.is_ascii_digit())
            || (prev.is_ascii_digit() && cur.is_alphabetic());
        if split {
            boundaries.push(byte_pos);
        }
    }
    boundaries
}

/// CamelCaseSplit with merge (chunks < 4 chars merged forward, max 2 raw chunks).
fn split_and_merge(text: &str) -> Vec<(usize, usize)> {
    let boundaries = find_boundaries(text);
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for i in 0..boundaries.len() {
        let start = boundaries[i];
        let end = if i + 1 < boundaries.len() { boundaries[i + 1] } else { text.len() };
        if start < end {
            ranges.push((start, end));
        }
    }
    if ranges.len() <= 1 {
        return ranges;
    }
    const MIN_CHUNK_CHARS: usize = 4;
    const MAX_MERGED_CHUNKS: usize = 2;
    let mut merged: Vec<(usize, usize)> = Vec::new();
    let mut acc_start = ranges[0].0;
    let mut acc_end = ranges[0].1;
    let mut acc_chunks = 1usize;
    for i in 1..ranges.len() {
        let acc_chars = text[acc_start..acc_end].chars().count();
        if acc_chars < MIN_CHUNK_CHARS && acc_chunks < MAX_MERGED_CHUNKS {
            acc_end = ranges[i].1;
            acc_chunks += 1;
        } else {
            merged.push((acc_start, acc_end));
            acc_start = ranges[i].0;
            acc_end = ranges[i].1;
            acc_chunks = 1;
        }
    }
    merged.push((acc_start, acc_end));
    merged
}

/// Full raw_code tokenization: SimpleTokenizer → CamelCaseSplit → lowercase.
/// Returns (offset_from, offset_to, lowercased_text) for each token.
fn tokenize_raw_code(text: &str) -> Vec<(usize, usize, String)> {
    let simple_tokens = simple_tokenize(text);
    let mut result = Vec::new();
    for (base_off, _, tok_text) in simple_tokens {
        let sub_ranges = split_and_merge(tok_text);
        for (rel_start, rel_end) in sub_ranges {
            let abs_start = base_off + rel_start;
            let abs_end = base_off + rel_end;
            let lowered = tok_text[rel_start..rel_end].to_lowercase();
            result.push((abs_start, abs_end, lowered));
        }
    }
    result
}

/// Semi-global Levenshtein: find if `pattern` appears as a fuzzy substring
/// of `text` within distance `max_d`. Returns all match end positions.
fn fuzzy_substring_matches(text: &str, pattern: &str, max_d: u32) -> Vec<(usize, u32)> {
    let text = text.as_bytes();
    let pat = pattern.as_bytes();
    let m = pat.len();
    if m == 0 {
        return vec![(0, 0)];
    }
    let n = text.len();
    if n == 0 {
        return vec![];
    }
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    let mut matches = Vec::new();
    for i in 1..=n {
        let mut curr = vec![0u32; m + 1];
        curr[0] = 0; // free prefix
        for j in 1..=m {
            let cost = if text[i - 1] == pat[j - 1] { 0 } else { 1 };
            curr[j] = std::cmp::min(
                std::cmp::min(curr[j - 1] + 1, prev[j] + 1),
                prev[j - 1] + cost,
            );
        }
        if curr[m] <= max_d {
            matches.push((i, curr[m]));
        }
        prev = curr;
    }
    matches
}

/// Traceback to find match start position for semi-global alignment ending at `end_pos`.
fn fuzzy_substring_with_start(text: &str, pattern: &str, max_d: u32) -> Vec<(usize, usize, u32)> {
    let text_bytes = text.as_bytes();
    let pat = pattern.as_bytes();
    let m = pat.len();
    if m == 0 {
        return vec![];
    }
    let n = text_bytes.len();
    if n == 0 {
        return vec![];
    }

    // Build full DP matrix for traceback
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for j in 0..=m {
        dp[0][j] = j as u32;
    }
    // Row 0: free prefix → dp[i][0] = 0 for all i
    for i in 1..=n {
        dp[i][0] = 0;
        for j in 1..=m {
            let cost = if text_bytes[i - 1] == pat[j - 1] { 0 } else { 1 };
            dp[i][j] = std::cmp::min(
                std::cmp::min(dp[i][j - 1] + 1, dp[i - 1][j] + 1),
                dp[i - 1][j - 1] + cost,
            );
        }
    }

    let mut results = Vec::new();
    for i in 1..=n {
        if dp[i][m] <= max_d {
            // Traceback to find start
            let end = i;
            let mut j = m;
            let mut row = i;
            while j > 0 && row > 0 {
                let cost = if text_bytes[row - 1] == pat[j - 1] { 0 } else { 1 };
                if dp[row][j] == dp[row - 1][j - 1] + cost {
                    row -= 1;
                    j -= 1;
                } else if dp[row][j] == dp[row - 1][j] + 1 {
                    row -= 1;
                } else {
                    j -= 1;
                }
            }
            let start = row;
            results.push((start, end, dp[i][m]));
        }
    }
    // Deduplicate overlapping matches: keep the best distance for each start position
    results.sort_by_key(|&(s, e, d)| (s, d, e));
    results.dedup_by_key(|x| x.0);
    results
}

/// Build ground truth: for each document, tokenize with raw_code, concatenate
/// tokens (no separators), find all fuzzy substring matches.
/// Returns: HashMap<doc_index, Vec<(matched_substring, distance)>>
fn build_ground_truth(
    files: &[(String, String)],
    query: &str,
    max_distance: u32,
) -> HashMap<usize, Vec<(String, u32)>> {
    let query_lower = query.to_lowercase();
    let mut ground_truth: HashMap<usize, Vec<(String, u32)>> = HashMap::new();

    for (doc_idx, (_path, content)) in files.iter().enumerate() {
        let tokens = tokenize_raw_code(content);
        if tokens.is_empty() {
            continue;
        }

        // Concatenate all token texts (no separators) for fuzzy substring search
        let concat: String = tokens.iter().map(|(_, _, t)| t.as_str()).collect();

        let matches = fuzzy_substring_with_start(&concat, &query_lower, max_distance);
        if !matches.is_empty() {
            let doc_matches: Vec<(String, u32)> = matches.iter()
                .map(|&(start, end, dist)| (concat[start..end].to_string(), dist))
                .collect();
            ground_truth.insert(doc_idx, doc_matches);
        }
    }
    ground_truth
}

// ─── Highlight verification ────────────────────────────────────────────────

/// Strip non-alphanumeric characters from highlighted text, lowercase, check distance.
fn verify_highlight(content: &str, hl_start: usize, hl_end: usize, query: &str, max_distance: u32) -> (bool, String, u32) {
    let hl_end = hl_end.min(content.len());
    if hl_start >= hl_end || hl_start >= content.len() {
        return (false, String::new(), u32::MAX);
    }
    // Make sure we're on char boundaries
    if !content.is_char_boundary(hl_start) || !content.is_char_boundary(hl_end) {
        return (false, String::new(), u32::MAX);
    }
    let hl_text = &content[hl_start..hl_end];
    // Strip separators (non-alphanumeric) and lowercase
    let stripped: String = hl_text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();

    let query_lower = query.to_lowercase();
    let dist = levenshtein(&stripped, &query_lower);
    (dist <= max_distance, stripped, dist)
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

// ─── File collection (same as test_merge_contains) ─────────────────────────

fn collect_repo_files() -> Vec<(String, String)> {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let exclude_dirs: Vec<&str> = vec![
        "target", "node_modules", "__pycache__", ".venv",
        ".pytest_cache", "pkg", ".git", "playground",
    ];
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

// ─── The test ──────────────────────────────────────────────────────────────

#[test]
fn test_fuzzy_ground_truth() {
    let files = collect_repo_files();
    eprintln!("Collected {} files", files.len());

    // ── 1. Create index ──────────────────────────────────────────────
    let tmp_path = std::path::Path::new("/tmp/test_fuzzy_ground_truth");
    let _ = std::fs::remove_dir_all(tmp_path);
    std::fs::create_dir_all(tmp_path).unwrap();

    let config = SchemaConfig {
        fields: vec![
            query::FieldDef {
                name: "content".into(),
                field_type: "text".into(),
                stored: Some(true),
                indexed: Some(true),
                fast: None,
            },
        ],
        ..Default::default()
    };
    let dir = StdFsDirectory::open(tmp_path).unwrap();
    let handle = LucivyHandle::create(dir, &config).unwrap();

    // Add all docs — one by one for doc_id = insertion order
    {
        let mut guard = handle.writer.lock().unwrap();
        let writer = guard.as_mut().unwrap();
        let content_field = handle.field("content").unwrap();
        for (_path, content) in files.iter() {
            let mut doc = ld_lucivy::LucivyDocument::new();
            doc.add_text(content_field, content);
            writer.add_document(doc).unwrap();
        }
        eprintln!("Added {} docs, committing...", files.len());
        writer.commit().unwrap();
        writer.drain_merges().unwrap();
        eprintln!("Committed + merged.");
    }
    handle.reader.reload().unwrap();

    let searcher = handle.reader.searcher();
    let num_segments = searcher.segment_readers().len();
    eprintln!("Index: {} docs, {} segments", searcher.num_docs(), num_segments);

    let content_field = handle.field("content").unwrap();

    // Build a map: file content hash → doc_indices (for matching).
    // Multiple files can have identical content (e.g. README.md duplicates),
    // so we map to Vec<usize> to avoid losing entries on hash collision.
    let mut file_content_map: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, (_, content)) in files.iter().enumerate() {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        file_content_map.entry(hasher.finish()).or_default().push(i);
    }

    // ── 3. Test multiple queries ──────────────────────────────────────
    let queries: Vec<(&str, u32)> = vec![
        ("rag3weaver", 1),
        ("rak3weaver", 1),
    ];

    let mut any_failure = false;

    for (query_text, distance) in &queries {
        eprintln!("\n============================================================");
        eprintln!("=== QUERY: \"{}\" d={} ===", query_text, distance);
        eprintln!("============================================================");

        let ground_truth = build_ground_truth(&files, query_text, *distance);
        eprintln!("Ground truth: {} docs", ground_truth.len());

        // Run fuzzy query via handle.search() (prescan + global IDF)
        let sink = Arc::new(ld_lucivy::query::HighlightSink::new());
        let qconfig = QueryConfig {
            query_type: "contains".into(),
            field: Some("content".into()),
            value: Some(query_text.to_string()),
            distance: Some(*distance as u8),
            ..Default::default()
        };
        let results = handle.search(&qconfig, 10_000, Some(sink.clone())).unwrap();
        eprintln!("Query returned {} results", results.len());

        let mut found_doc_indices: HashSet<usize> = HashSet::new();
        let mut highlight_failures: Vec<String> = Vec::new();
        let mut highlight_successes = 0usize;
        let mut highlight_total = 0usize;

        eprintln!("\n--- HIGHLIGHTS ---");
        for (score, addr) in &results {
            let doc: ld_lucivy::LucivyDocument = searcher.doc(*addr).unwrap();
            let content: String = doc.get_first(content_field)
                .map(|v| {
                    let owned: ld_lucivy::schema::OwnedValue = v.into();
                    match owned {
                        ld_lucivy::schema::OwnedValue::Str(s) => s,
                        _ => String::new(),
                    }
                })
                .unwrap_or_default();

            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            content.hash(&mut hasher);
            let hash = hasher.finish();
            let doc_indices = file_content_map.get(&hash);

            if let Some(indices) = doc_indices {
                for &idx in indices {
                    found_doc_indices.insert(idx);
                }
            }

            let path_str = doc_indices.and_then(|v| v.first()).map(|&i| files[i].0.as_str()).unwrap_or("???");

            let seg_id = searcher.segment_reader(addr.segment_ord as u32).segment_id();
            let hl_map = sink.get(seg_id, addr.doc_id);

            if let Some(fields) = &hl_map {
                if let Some(offsets) = fields.get("content") {
                    for hl in offsets {
                        highlight_total += 1;
                        let hl_end = hl[1].min(content.len());
                        let hl_text = if hl[0] < content.len() && content.is_char_boundary(hl[0]) && content.is_char_boundary(hl_end) {
                            content[hl[0]..hl_end].to_string()
                        } else {
                            format!("<invalid range {}-{}>", hl[0], hl[1])
                        };

                        let (ok, stripped, dist) = verify_highlight(
                            &content, hl[0], hl[1], query_text, *distance,
                        );

                        let mut ctx_start = hl[0].saturating_sub(20);
                        while ctx_start > 0 && !content.is_char_boundary(ctx_start) { ctx_start -= 1; }
                        let mut ctx_end = (hl_end + 20).min(content.len());
                        while ctx_end < content.len() && !content.is_char_boundary(ctx_end) { ctx_end += 1; }
                        let context = &content[ctx_start..ctx_end];

                        let status = if ok { "OK" } else { "FAIL" };
                        eprintln!("  [{}] {} hl=[{},{}] len={} stripped=\"{}\" dist={} {} | raw: \"{}\" | ctx: ...{}...",
                            status, path_str, hl[0], hl[1], hl[1] - hl[0],
                            stripped, dist,
                            if ok { "" } else { "<---" },
                            hl_text.replace('\n', "\\n"),
                            context.replace('\n', "\\n"));

                        if ok {
                            highlight_successes += 1;
                        } else {
                            highlight_failures.push(format!(
                                "[{}] hl=[{},{}] raw=\"{}\" stripped=\"{}\" dist={}",
                                path_str, hl[0], hl[1],
                                hl_text.replace('\n', "\\n"),
                                stripped, dist
                            ));
                        }
                    }
                }
            }
        }

        // Check recall (filter out duplicate-content files that can't be mapped)
        let mut missed = Vec::new();
        for (&doc_idx, matches) in &ground_truth {
            if !found_doc_indices.contains(&doc_idx) {
                // Check if this is a duplicate file (hash maps to different index)
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                files[doc_idx].1.hash(&mut h);
                if !file_content_map.get(&h.finish()).map_or(false, |v| v.contains(&doc_idx)) {
                    continue; // duplicate content, not a real miss
                }
                let path = &files[doc_idx].0;
                let match_strs: Vec<String> = matches.iter().map(|(s, d)| format!("\"{}\"(d={})", s, d)).collect();
                missed.push(format!("  MISSED [{}] {} : {}", doc_idx, path, match_strs.join(", ")));
            }
        }

        // Check precision
        let mut false_positives = Vec::new();
        for &idx in &found_doc_indices {
            if !ground_truth.contains_key(&idx) {
                false_positives.push(format!("  FALSE POSITIVE [{}] {}", idx, files[idx].0));
            }
        }

        eprintln!("\n--- RESULTS for \"{}\" d={} ---", query_text, distance);
        eprintln!("Ground truth: {} docs", ground_truth.len());
        eprintln!("Query results: {} docs", results.len());
        if missed.is_empty() {
            eprintln!("Recall: {}/{} OK", ground_truth.len(), ground_truth.len());
        } else {
            eprintln!("Recall: MISSING {} docs:", missed.len());
            for m in &missed { eprintln!("{}", m); }
        }
        if false_positives.is_empty() {
            eprintln!("Precision: {}/{} OK", results.len(), results.len());
        } else {
            eprintln!("Precision: {} false positives:", false_positives.len());
            for fp in &false_positives { eprintln!("{}", fp); }
        }
        if highlight_failures.is_empty() {
            eprintln!("Highlights: {}/{} valid!", highlight_successes, highlight_total);
        } else {
            eprintln!("Highlights: {}/{} valid — {} FAILURES:", highlight_successes, highlight_total, highlight_failures.len());
            for f in &highlight_failures { eprintln!("  {}", f); }
        }

        if !missed.is_empty() || !false_positives.is_empty() || !highlight_failures.is_empty() {
            any_failure = true;
        }
    }

    assert!(!any_failure, "One or more queries had failures — see output above");
}
