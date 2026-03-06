//! Shared scoring utilities for contains queries (ngram and cascade).
//!
//! Consolidates functions previously duplicated between `contains_scorer.rs`,
//! `ngram_query.rs`, and `matching.rs`.

use std::cmp::min;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::DocId;
use crate::index::SegmentId;

// ─── Highlight Sink ─────────────────────────────────────────────────────────

/// Key for highlight data: (segment_id, doc_id).
///
/// Uses `SegmentId` (UUID) instead of a counter-based ordinal so that
/// multiple sub-queries (e.g. in a BooleanQuery) that score the same
/// segment all share the same key space.
type HighlightKey = (SegmentId, DocId);

/// Side-channel for highlight byte offsets, shared between caller and scorers.
///
/// The caller creates an `Arc<HighlightSink>` and passes it to the query via
/// `with_highlight_sink()`. During scoring, when a match is confirmed, the
/// scorer inserts byte offsets into the sink tagged with a field name.
/// After search, the caller reads the sink to populate highlights per field.
#[derive(Debug)]
pub struct HighlightSink {
    #[allow(clippy::type_complexity)]
    data: Mutex<HashMap<HighlightKey, Vec<(String, usize, usize)>>>,
}

impl HighlightSink {
    /// Creates a new empty highlight sink.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        HighlightSink {
            data: Mutex::new(HashMap::new()),
        }
    }

    /// Called by scorers when a match is confirmed.
    /// Appends offsets tagged with `field_name` (does not overwrite previous entries).
    pub fn insert(
        &self,
        segment_id: SegmentId,
        doc_id: DocId,
        field_name: &str,
        offsets: Vec<[usize; 2]>,
    ) {
        let entries: Vec<(String, usize, usize)> = offsets
            .into_iter()
            .map(|[s, e]| (field_name.to_string(), s, e))
            .collect();
        self.data
            .lock()
            .unwrap()
            .entry((segment_id, doc_id))
            .or_default()
            .extend(entries);
    }

    /// Called after search to retrieve offsets grouped by field name.
    pub fn get(
        &self,
        segment_id: SegmentId,
        doc_id: DocId,
    ) -> Option<HashMap<String, Vec<[usize; 2]>>> {
        let data = self.data.lock().unwrap();
        let entries = data.get(&(segment_id, doc_id))?;
        let mut by_field: HashMap<String, Vec<[usize; 2]>> = HashMap::new();
        for (field, start, end) in entries {
            by_field
                .entry(field.clone())
                .or_default()
                .push([*start, *end]);
        }
        Some(by_field)
    }
}

// ─── Tokenization ───────────────────────────────────────────────────────────

/// Re-tokenize raw text into (byte_offset_from, byte_offset_to) pairs.
/// Splits on non-alphanumeric characters (mirrors the default tokenizer).
pub(crate) fn tokenize_raw(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        tokens.push((start, i));
    }
    tokens
}

/// Levenshtein edit distance between two strings.
pub(crate) fn edit_distance(a: &str, b: &str) -> u32 {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let m = a.len();
    let n = b.len();
    let mut prev = (0..=n as u32).collect::<Vec<_>>();
    let mut curr = vec![0u32; n + 1];
    for i in 1..=m {
        curr[0] = i as u32;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = min(min(curr[j - 1] + 1, prev[j] + 1), prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Check if `text` contains a substring within Levenshtein distance `max_d` of `pattern`.
/// Uses semi-global alignment (free prefix/suffix gaps in `text`).
pub(crate) fn contains_fuzzy_substring(text: &str, pattern: &str, max_d: u32) -> bool {
    let text = text.as_bytes();
    let pattern = pattern.as_bytes();
    let m = pattern.len();
    if m == 0 {
        return true;
    }
    let n = text.len();
    if n == 0 {
        return false;
    }
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    for i in 1..=n {
        let mut curr = vec![0u32; m + 1];
        curr[0] = 0; // Free prefix: can start matching at any text position.
        for j in 1..=m {
            let cost = if text[i - 1] == pattern[j - 1] { 0 } else { 1 };
            curr[j] = min(min(curr[j - 1] + 1, prev[j] + 1), prev[j - 1] + cost);
        }
        // Free suffix: if full pattern matched within budget, we're done.
        if curr[m] <= max_d {
            return true;
        }
        prev = curr;
    }
    false
}

/// Check if a doc token matches a query token via exact, substring, fuzzy, or fuzzy substring.
/// Returns the match distance (0 for exact/substring, d for fuzzy).
pub(crate) fn token_match_distance(
    doc_token: &str,
    query_token: &str,
    fuzzy_distance: u8,
) -> Option<u32> {
    // Exact
    if doc_token == query_token {
        return Some(0);
    }
    // Query is substring of doc token (e.g. "program" in "programming")
    if doc_token.contains(query_token) {
        return Some(0);
    }
    if fuzzy_distance > 0 {
        // Fuzzy whole-word
        let d = edit_distance(doc_token, query_token);
        if d <= fuzzy_distance as u32 {
            return Some(d);
        }
        // Fuzzy substring (e.g. "progam" ≈ substring of "programming")
        if contains_fuzzy_substring(doc_token, query_token, fuzzy_distance as u32) {
            return Some(fuzzy_distance as u32);
        }
    }
    None
}

// ─── N-gram Utilities ────────────────────────────────────────────────────────

const NGRAM_SIZE: usize = 3;

/// Generate character-level trigrams from a token.
/// Tokens shorter than 3 are returned as-is.
pub(crate) fn generate_trigrams(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < NGRAM_SIZE {
        return vec![token.to_string()];
    }
    chars
        .windows(NGRAM_SIZE)
        .map(|w| w.iter().collect())
        .collect()
}

/// Minimum number of shared trigrams required for a candidate match.
/// For fuzzy distance `d`, each edit can destroy up to 3 trigrams.
pub(crate) fn ngram_threshold(num_trigrams: usize, fuzzy_distance: u8) -> usize {
    let threshold = num_trigrams as i32 - (fuzzy_distance as i32 * 3);
    std::cmp::max(1, threshold) as usize
}

/// Intersect multiple sorted doc_id vectors.
pub(crate) fn intersect_sorted_vecs(mut vecs: Vec<Vec<DocId>>) -> Vec<DocId> {
    if vecs.is_empty() {
        return Vec::new();
    }
    if vecs.len() == 1 {
        return vecs.into_iter().next().unwrap();
    }
    // Process smallest first for efficiency.
    vecs.sort_by_key(|v| v.len());

    let mut result = vecs.remove(0);
    for other in &vecs {
        let mut merged = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < result.len() && j < other.len() {
            match result[i].cmp(&other[j]) {
                std::cmp::Ordering::Equal => {
                    merged.push(result[i]);
                    i += 1;
                    j += 1;
                }
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
            }
        }
        result = merged;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SegmentId;

    fn sid() -> SegmentId {
        SegmentId::generate_random()
    }

    // ─── tokenize_raw ────────────────────────────────────────────────────

    #[test]
    fn test_tokenize_raw() {
        assert_eq!(tokenize_raw("hello world"), vec![(0, 5), (6, 11)]);
    }

    #[test]
    fn test_tokenize_raw_special_chars() {
        assert_eq!(
            tokenize_raw("std::collections::HashMap"),
            vec![(0, 3), (5, 16), (18, 25)]
        );
        assert_eq!(
            tokenize_raw("c++ is great"),
            vec![(0, 1), (4, 6), (7, 12)]
        );
    }

    #[test]
    fn test_tokenize_raw_separators() {
        assert_eq!(tokenize_raw("hello-world"), vec![(0, 5), (6, 11)]);
        assert_eq!(tokenize_raw("a--b"), vec![(0, 1), (3, 4)]);
    }

    // ─── edit_distance ───────────────────────────────────────────────────

    #[test]
    fn test_edit_distance() {
        assert_eq!(edit_distance("hello", "hello"), 0);
        assert_eq!(edit_distance("hello", "helo"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("-", "_"), 1);
        assert_eq!(edit_distance("++", "+"), 1);
    }

    // ─── contains_fuzzy_substring ────────────────────────────────────────

    #[test]
    fn test_contains_fuzzy_substring() {
        assert!(contains_fuzzy_substring("programming", "program", 0));
        assert!(contains_fuzzy_substring("programming", "progam", 1));
        assert!(!contains_fuzzy_substring("programming", "xyz", 1));
        assert!(contains_fuzzy_substring("hello", "", 0));
    }

    // ─── token_match_distance ────────────────────────────────────────────

    #[test]
    fn test_token_match_distance() {
        assert_eq!(token_match_distance("hello", "hello", 0), Some(0));
        assert_eq!(token_match_distance("programming", "program", 0), Some(0));
        assert_eq!(token_match_distance("hello", "helo", 1), Some(1));
        assert_eq!(token_match_distance("programming", "progam", 1), Some(1));
        assert_eq!(token_match_distance("hello", "xyz", 1), None);
    }

    // ─── generate_trigrams ───────────────────────────────────────────────

    #[test]
    fn test_generate_trigrams() {
        assert_eq!(generate_trigrams("hello"), vec!["hel", "ell", "llo"]);
        assert_eq!(generate_trigrams("ab"), vec!["ab"]);
        assert_eq!(generate_trigrams("abc"), vec!["abc"]);
    }

    // ─── ngram_threshold ─────────────────────────────────────────────────

    #[test]
    fn test_ngram_threshold() {
        assert_eq!(ngram_threshold(3, 0), 3);
        assert_eq!(ngram_threshold(3, 1), 1);
        assert_eq!(ngram_threshold(9, 0), 9);
        assert_eq!(ngram_threshold(9, 1), 6);
    }

    // ─── intersect_sorted_vecs ───────────────────────────────────────────

    #[test]
    fn test_intersect_sorted_vecs() {
        let a = vec![1, 3, 5, 7, 9];
        let b = vec![2, 3, 5, 8, 9];
        assert_eq!(intersect_sorted_vecs(vec![a, b]), vec![3, 5, 9]);
    }

    #[test]
    fn test_intersect_sorted_vecs_empty() {
        assert_eq!(intersect_sorted_vecs(vec![]), Vec::<DocId>::new());
    }

    #[test]
    fn test_intersect_sorted_vecs_single() {
        assert_eq!(intersect_sorted_vecs(vec![vec![1, 2, 3]]), vec![1, 2, 3]);
    }

    #[test]
    fn test_intersect_sorted_vecs_disjoint() {
        assert_eq!(
            intersect_sorted_vecs(vec![vec![1, 3, 5], vec![2, 4, 6]]),
            Vec::<DocId>::new()
        );
    }

    #[test]
    fn test_intersect_sorted_vecs_three() {
        let a = vec![1, 2, 3, 5, 8];
        let b = vec![2, 3, 5, 7];
        let c = vec![3, 5, 9];
        assert_eq!(intersect_sorted_vecs(vec![a, b, c]), vec![3, 5]);
    }

    // ─── generate_trigrams edge cases ──────────────────────────────────

    #[test]
    fn test_generate_trigrams_empty() {
        assert_eq!(generate_trigrams(""), vec![""]);
    }

    #[test]
    fn test_generate_trigrams_single_char() {
        assert_eq!(generate_trigrams("a"), vec!["a"]);
    }

    #[test]
    fn test_generate_trigrams_long() {
        let trigrams = generate_trigrams("programming");
        assert_eq!(trigrams.len(), 9); // 11 chars - 3 + 1 = 9 trigrams
        assert_eq!(trigrams[0], "pro");
        assert_eq!(trigrams[8], "ing");
    }

    // ─── token_match_distance edge cases ───────────────────────────────

    #[test]
    fn test_token_match_distance_substring() {
        // "program" is a substring of "programming"
        assert_eq!(token_match_distance("programming", "program", 0), Some(0));
    }

    #[test]
    fn test_token_match_distance_fuzzy_substring() {
        // "progam" is fuzzy-substring of "programming" (distance 1)
        assert_eq!(token_match_distance("programming", "progam", 1), Some(1));
    }

    #[test]
    fn test_token_match_distance_too_far() {
        // "xyz" is more than 1 edit from any token
        assert_eq!(token_match_distance("hello", "xyz", 1), None);
    }

    // ─── contains_fuzzy_substring edge cases ────────────────────────────

    #[test]
    fn test_contains_fuzzy_substring_empty_pattern() {
        assert!(contains_fuzzy_substring("anything", "", 0));
    }

    #[test]
    fn test_contains_fuzzy_substring_empty_text() {
        assert!(!contains_fuzzy_substring("", "hello", 0));
    }

    #[test]
    fn test_contains_fuzzy_substring_exact_match() {
        assert!(contains_fuzzy_substring("hello", "hello", 0));
    }

    // ─── ngram_threshold edge cases ─────────────────────────────────────

    #[test]
    fn test_ngram_threshold_zero_trigrams() {
        // With 0 trigrams and distance 0, threshold should be min 1
        assert_eq!(ngram_threshold(0, 0), 1);
    }

    #[test]
    fn test_ngram_threshold_high_distance() {
        // With distance > num_trigrams / 3, threshold floors to 1
        assert_eq!(ngram_threshold(3, 2), 1);
    }

    // ─── edit_distance edge cases ───────────────────────────────────────

    #[test]
    fn test_edit_distance_same_length() {
        assert_eq!(edit_distance("abc", "axc"), 1);
    }

    #[test]
    fn test_edit_distance_insert_delete() {
        assert_eq!(edit_distance("abc", "abcd"), 1);
        assert_eq!(edit_distance("abcd", "abc"), 1);
    }

    // ─── tokenize_raw edge cases ────────────────────────────────────────

    #[test]
    fn test_tokenize_raw_empty() {
        assert_eq!(tokenize_raw(""), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn test_tokenize_raw_only_separators() {
        assert_eq!(tokenize_raw("---...   "), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn test_tokenize_raw_single_word() {
        assert_eq!(tokenize_raw("hello"), vec![(0, 5)]);
    }

    // ─── HighlightSink ─────────────────────────────────────────────────

    #[test]
    fn test_highlight_sink_insert_get() {
        let sink = HighlightSink::new();
        let s = sid();
        sink.insert(s, 42, "body", vec![[5, 10], [20, 30]]);
        let by_field = sink.get(s, 42).unwrap();
        assert_eq!(by_field.len(), 1);
        assert_eq!(by_field["body"], vec![[5, 10], [20, 30]]);
    }

    #[test]
    fn test_highlight_sink_multi_field() {
        let sink = HighlightSink::new();
        let s = sid();
        sink.insert(s, 42, "title", vec![[0, 5]]);
        sink.insert(s, 42, "body", vec![[100, 200], [500, 550]]);
        let by_field = sink.get(s, 42).unwrap();
        assert_eq!(by_field.len(), 2);
        assert_eq!(by_field["title"], vec![[0, 5]]);
        assert_eq!(by_field["body"], vec![[100, 200], [500, 550]]);
    }

    #[test]
    fn test_highlight_sink_same_field_appends() {
        let sink = HighlightSink::new();
        let s = sid();
        sink.insert(s, 42, "body", vec![[5, 10]]);
        sink.insert(s, 42, "body", vec![[20, 30]]);
        let by_field = sink.get(s, 42).unwrap();
        assert_eq!(by_field["body"], vec![[5, 10], [20, 30]]);
    }

    #[test]
    fn test_highlight_sink_get_missing() {
        let sink = HighlightSink::new();
        assert!(sink.get(sid(), 99).is_none());
    }

    #[test]
    fn test_highlight_sink_same_segment_different_docs() {
        let sink = HighlightSink::new();
        let s = sid();
        sink.insert(s, 1, "body", vec![[0, 5]]);
        sink.insert(s, 2, "body", vec![[10, 20]]);
        assert_eq!(sink.get(s, 1).unwrap()["body"], vec![[0, 5]]);
        assert_eq!(sink.get(s, 2).unwrap()["body"], vec![[10, 20]]);
    }
}
