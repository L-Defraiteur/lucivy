//! Suffix FST contains search — v2 path parallel to ngram_contains_query.
//!
//! Uses the .sfx file (suffix FST with redirection to ._raw posting lists)
//! to resolve contains queries without stored text verification.
//!
//! Kept as a standalone module — can be activated/deactivated without
//! touching the existing ngram contains path.

use crate::suffix_fst::file::SfxFileReader;

/// A single contains match result.
#[derive(Debug, Clone)]
pub struct SuffixContainsMatch {
    /// Document ID within the segment.
    pub doc_id: u32,
    /// Token index of the matched token in the document.
    pub token_index: u32,
    /// Byte offset in the original text where the match starts.
    pub byte_from: usize,
    /// Byte offset where the match ends (byte_from + query_len).
    pub byte_to: usize,
    /// The parent token text (from ._raw FST). Empty if not resolved.
    pub parent_term: String,
    /// Suffix index (0 = full token match, >0 = substring).
    pub si: u16,
}

/// Search for single-token contains matches using the suffix FST.
///
/// This is the core search function — walks the .sfx FST with a prefix query,
/// resolves parents to ._raw posting lists, and returns matches.
///
/// `raw_term_resolver` is a callback that maps a raw_ordinal to its posting list
/// entries: Vec<(doc_id, token_index, byte_from, byte_to)>.
/// This decouples the search from the posting list implementation.
pub fn suffix_contains_single_token<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    let query_lower = query.to_lowercase();
    let query_len = query_lower.len();

    // Prefix walk on the suffix FST
    let walk_results = sfx_reader.prefix_walk(&query_lower);

    let mut matches: Vec<SuffixContainsMatch> = Vec::new();

    for (suffix_term, parents) in &walk_results {
        for parent in parents {
            // Resolve parent ordinal to posting list entries
            let postings = raw_term_resolver(parent.raw_ordinal);

            for entry in &postings {
                matches.push(SuffixContainsMatch {
                    doc_id: entry.doc_id,
                    token_index: entry.token_index,
                    byte_from: entry.byte_from as usize + parent.si as usize,
                    byte_to: entry.byte_from as usize + parent.si as usize + query_len,
                    parent_term: String::new(), // resolved later if needed
                    si: parent.si,
                });
            }
        }
    }

    // Sort by (doc_id, byte_from) for consistent output
    matches.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.byte_from.cmp(&b.byte_from)));

    // Deduplicate same (doc_id, byte_from) matches
    matches.dedup_by(|a, b| a.doc_id == b.doc_id && a.byte_from == b.byte_from);

    matches
}

/// A posting list entry from the ._raw field.
#[derive(Debug, Clone)]
pub struct RawPostingEntry {
    pub doc_id: u32,
    pub token_index: u32,
    pub byte_from: u32,
    pub byte_to: u32,
}

/// Search for multi-token contains matches.
///
/// Rules:
/// - First token: .sfx exact lookup, any SI (can be suffix of doc token)
/// - Middle tokens: ._raw exact lookup, SI=0 (must be full tokens)
/// - Last token: .sfx prefix walk, SI=0 (can be prefix of doc token)
///
/// `raw_exact_resolver` resolves a term string to its posting entries from ._raw.
/// `sfx_reader` provides suffix FST access.
pub fn suffix_contains_multi_token<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    separators: &[&str],
    raw_exact_resolver: F,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(&str) -> Vec<RawPostingEntry>,
{
    if query_tokens.is_empty() {
        return Vec::new();
    }
    if query_tokens.len() == 1 {
        // Delegate to single-token with a simple resolver adapter
        let results = suffix_contains_single_token(sfx_reader, query_tokens[0], |ord| {
            // For single token via multi-token path, we'd need the ordinal resolver.
            // This path shouldn't normally be taken — use suffix_contains_single_token directly.
            Vec::new()
        });
        return results
            .into_iter()
            .map(|m| SuffixContainsMultiMatch {
                doc_id: m.doc_id,
                byte_from: m.byte_from,
                byte_to: m.byte_to,
                token_matches: vec![m],
            })
            .collect();
    }

    // Step 1: Resolve first token via .sfx (any SI)
    let first_query = query_tokens[0].to_lowercase();
    let first_parents = sfx_reader.resolve_suffix(&first_query);
    if first_parents.is_empty() {
        return Vec::new();
    }

    // Step 2: Resolve middle tokens via ._raw exact (SI=0)
    let mut middle_postings: Vec<Vec<RawPostingEntry>> = Vec::new();
    for &token in &query_tokens[1..query_tokens.len() - 1] {
        let postings = raw_exact_resolver(&token.to_lowercase());
        if postings.is_empty() {
            return Vec::new(); // A middle token doesn't exist → no match possible
        }
        middle_postings.push(postings);
    }

    // Step 3: Resolve last token via .sfx prefix walk (SI=0 only)
    let last_query = query_tokens[query_tokens.len() - 1].to_lowercase();
    let last_walk = sfx_reader.prefix_walk(&last_query);
    let mut last_postings: Vec<RawPostingEntry> = Vec::new();
    for (_, parents) in &last_walk {
        for parent in parents {
            if parent.si != 0 {
                continue; // Last token must match at SI=0 (start of doc token)
            }
            let entries = raw_exact_resolver(""); // Would need ordinal resolver here
            // TODO: This needs the ordinal-based resolver, not string-based.
            // For now, this is a placeholder. The full integration will use the
            // inverted index reader to resolve ordinals to posting lists.
        }
    }

    // TODO: Step 4-7: intersection of consecutive Ti, GapMap validation
    // This is a placeholder for the full multi-token flow.
    // The core logic is:
    // - For each first_parent posting, find consecutive Ti in middle + last postings
    // - Validate separators via GapMap
    // - Verify first token reaches end of doc token (SI + len = parent_len)

    Vec::new()
}

/// A multi-token contains match.
#[derive(Debug, Clone)]
pub struct SuffixContainsMultiMatch {
    pub doc_id: u32,
    pub byte_from: usize,
    pub byte_to: usize,
    pub token_matches: Vec<SuffixContainsMatch>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::suffix_fst::SfxCollector;
    use crate::suffix_fst::file::SfxFileReader;

    /// Build a test .sfx and a fake ._raw posting resolver.
    fn build_test_index() -> (Vec<u8>, HashMap<u64, Vec<RawPostingEntry>>) {
        let mut collector = SfxCollector::new();

        // Doc 0: "import rag3db from 'rag3db_core';"
        collector.begin_doc();
        collector.begin_value("import rag3db from 'rag3db_core';", 0);
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.add_token("from", 14, 18);
        collector.add_token("rag3db", 20, 26);
        collector.add_token("core", 27, 31);
        collector.end_value();
        collector.end_doc();

        // Doc 1: "rag3db is cool"
        collector.begin_doc();
        collector.begin_value("rag3db is cool", 0);
        collector.add_token("rag3db", 0, 6);
        collector.add_token("is", 7, 9);
        collector.add_token("cool", 10, 14);
        collector.end_value();
        collector.end_doc();

        let sfx_bytes = collector.build().unwrap();

        // Build fake ._raw posting lists indexed by raw_ordinal.
        // Sorted unique tokens: cool(0), core(1), from(2), import(3), is(4), rag3db(5)
        let mut raw_postings: HashMap<u64, Vec<RawPostingEntry>> = HashMap::new();

        // cool(0) → doc=0 Ti=4 (wait, not in doc0... let me recalculate)
        // Doc 0 tokens: import(Ti=0), rag3db(Ti=1), from(Ti=2), rag3db(Ti=3), core(Ti=4)
        // Doc 1 tokens: rag3db(Ti=0), is(Ti=1), cool(Ti=2)

        raw_postings.insert(0, vec![ // "cool"
            RawPostingEntry { doc_id: 1, token_index: 2, byte_from: 10, byte_to: 14 },
        ]);
        raw_postings.insert(1, vec![ // "core"
            RawPostingEntry { doc_id: 0, token_index: 4, byte_from: 27, byte_to: 31 },
        ]);
        raw_postings.insert(2, vec![ // "from"
            RawPostingEntry { doc_id: 0, token_index: 2, byte_from: 14, byte_to: 18 },
        ]);
        raw_postings.insert(3, vec![ // "import"
            RawPostingEntry { doc_id: 0, token_index: 0, byte_from: 0, byte_to: 6 },
        ]);
        raw_postings.insert(4, vec![ // "is"
            RawPostingEntry { doc_id: 1, token_index: 1, byte_from: 7, byte_to: 9 },
        ]);
        raw_postings.insert(5, vec![ // "rag3db"
            RawPostingEntry { doc_id: 0, token_index: 1, byte_from: 7, byte_to: 13 },
            RawPostingEntry { doc_id: 0, token_index: 3, byte_from: 20, byte_to: 26 },
            RawPostingEntry { doc_id: 1, token_index: 0, byte_from: 0, byte_to: 6 },
        ]);

        (sfx_bytes, raw_postings)
    }

    #[test]
    fn test_single_token_exact() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_single_token(&reader, "rag3db", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        // "rag3db" should match at SI=0 in both docs
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 7);  // first occurrence
        assert_eq!(results[0].si, 0);
        assert_eq!(results[1].doc_id, 0);
        assert_eq!(results[1].byte_from, 20); // second occurrence
        assert_eq!(results[2].doc_id, 1);
        assert_eq!(results[2].byte_from, 0);
    }

    #[test]
    fn test_single_token_substring() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "g3db" is a suffix of "rag3db" at SI=2
        let results = suffix_contains_single_token(&reader, "g3db", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 3);
        // byte_from = original byte_from + SI
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 9);   // 7 + 2
        assert_eq!(results[0].si, 2);
        assert_eq!(results[1].doc_id, 0);
        assert_eq!(results[1].byte_from, 22);  // 20 + 2
        assert_eq!(results[2].doc_id, 1);
        assert_eq!(results[2].byte_from, 2);   // 0 + 2
    }

    #[test]
    fn test_single_token_prefix_match() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "rag" is a prefix of suffix "rag3db" → prefix walk finds it
        let results = suffix_contains_single_token(&reader, "rag", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 3); // same 3 occurrences of "rag3db"
        assert_eq!(results[0].byte_from, 7);
        assert_eq!(results[0].byte_to, 10);  // 7 + len("rag")=3
    }

    #[test]
    fn test_single_token_mid_word() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "port" is a suffix of "import" at SI=2, prefix walk finds "port" in suffix FST
        let results = suffix_contains_single_token(&reader, "port", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 2);  // 0 + 2
        assert_eq!(results[0].byte_to, 6);    // 2 + len("port")=4
    }

    #[test]
    fn test_single_token_no_match() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_single_token(&reader, "xyz", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert!(results.is_empty());
    }

    #[test]
    fn test_single_token_highlights() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "ore" is suffix of "core" at SI=1
        let results = suffix_contains_single_token(&reader, "ore", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 28); // 27 + 1
        assert_eq!(results[0].byte_to, 31);   // 28 + 3
        assert_eq!(results[0].si, 1);
    }
}
