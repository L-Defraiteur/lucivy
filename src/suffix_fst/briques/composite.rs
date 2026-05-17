//! Tier 3 — Composite operations for SFX v3.
//!
//! High-level building blocks that compose Tier 1 (FST walk) and Tier 2 (resolve):
//!
//! - `find_literal_v3`: find all occurrences of a literal (single + cross-token)
//! - `find_multi_token_v3`: multi-token adjacency search with pivot optimization
//! - `resolve_trigrams_v3`: fuzzy trigram pigeonhole pipeline

use std::collections::HashSet;

use common::BitSet;

use crate::DocId;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file_v3::SfxFileReaderV3;

use super::fst_walk::{self, FstCandidateV3, TokenChainV3};
use super::resolve::{self, MatchV3};

// ─── find_literal_v3 ──────────────────────────────────────────────────────

/// Find all occurrences of a literal string (single-token + cross-token).
///
/// Combines `fst_candidates_v3` (single-token matches) with
/// `cross_token_chain_v3` (cross-token matches via falling walk chain).
/// Deduplicates by (doc_id, position).
pub fn find_literal_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    // Without PosMap, relaxed chains use ByteOrdered fallback (pos_B > pos_A, byte ordered)
    find_literal_v3_full(reader, query, resolver, anchor_start, strict_separators, filter_docs, None, None)
}

/// Like `find_literal_v3` but with PosMap + ord_to_term for strict_sep=false.

/// Like `find_literal_v3` but with PosMap + ByteMap for strict_sep=false
/// intermediate token verification (ensures only sep tokens between matches).
pub fn find_literal_v3_full(
    reader: &SfxFileReaderV3,
    query: &str,
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: Option<&crate::suffix_fst::posmap::PosMapReader<'_>>,
    bytemap: Option<&crate::suffix_fst::bytemap::ByteBitmapReader<'_>>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    // Single-token matches
    let candidates = fst_walk::fst_candidates_v3(reader, query, anchor_start, strict_separators);
    let single = resolve::resolve_single_v3(&candidates, resolver, filter_docs);
    results.extend(single);

    // Cross-token matches (only if not anchor_start — anchor means token start)
    if !anchor_start {
        let chains = fst_walk::cross_token_chain_v3(reader, query, strict_separators);
        let cross = if !strict_separators {
            resolve::resolve_chains_v3_relaxed(&chains, resolver, filter_docs, posmap, bytemap)
        } else {
            resolve::resolve_chains_v3(&chains, resolver, filter_docs)
        };
        results.extend(cross);
    }

    // Dedup by (doc_id, position)
    results.sort_by_key(|m| (m.doc_id, m.position));
    results.dedup_by_key(|m| (m.doc_id, m.position));

    results
}

// ─── find_multi_token_v3 ──────────────────────────────────────────────────

/// Multi-token adjacency search with pivot optimization.
///
/// Splits the query on non-alphanum boundaries, resolves each sub-token
/// independently, picks the most selective as pivot, then verifies
/// adjacency bidirectionally.
pub fn find_multi_token_v3(
    reader: &SfxFileReaderV3,
    query_tokens: &[&str],
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    if query_tokens.is_empty() {
        return Vec::new();
    }
    if query_tokens.len() == 1 {
        return find_literal_v3(
            reader, query_tokens[0], resolver,
            anchor_start, strict_separators, filter_docs,
        );
    }

    // Resolve each sub-token independently
    let per_token: Vec<Vec<MatchV3>> = query_tokens
        .iter()
        .enumerate()
        .map(|(i, token)| {
            let anchor = anchor_start && i == 0;
            find_literal_v3(reader, token, resolver, anchor, strict_separators, filter_docs)
        })
        .collect();

    // Pick pivot: the sub-token with fewest matches (most selective)
    let pivot_idx = per_token
        .iter()
        .enumerate()
        .min_by_key(|(_, matches)| matches.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    // From pivot, verify adjacency backward and forward
    let mut results = Vec::new();

    for pivot_match in &per_token[pivot_idx] {
        let doc_id = pivot_match.doc_id;
        let pivot_pos = pivot_match.position;

        // Check backward: tokens before pivot must be at consecutive positions
        let mut valid = true;
        let mut byte_from = pivot_match.byte_from;

        for step in (0..pivot_idx).rev() {
            let expected_pos = pivot_pos - (pivot_idx - step) as u32;
            let found = per_token[step]
                .iter()
                .any(|m| m.doc_id == doc_id && m.position == expected_pos);
            if !found {
                valid = false;
                break;
            }
            if let Some(m) = per_token[step]
                .iter()
                .find(|m| m.doc_id == doc_id && m.position == expected_pos)
            {
                byte_from = m.byte_from;
            }
        }

        if !valid {
            continue;
        }

        // Check forward: tokens after pivot must be at consecutive positions
        let mut byte_to = pivot_match.byte_to;

        for step in (pivot_idx + 1)..query_tokens.len() {
            let expected_pos = pivot_pos + (step - pivot_idx) as u32;
            let found = per_token[step]
                .iter()
                .any(|m| m.doc_id == doc_id && m.position == expected_pos);
            if !found {
                valid = false;
                break;
            }
            if let Some(m) = per_token[step]
                .iter()
                .find(|m| m.doc_id == doc_id && m.position == expected_pos)
            {
                byte_to = m.byte_to;
            }
        }

        if valid {
            results.push(MatchV3 {
                doc_id,
                position: pivot_pos - pivot_idx as u32,
                span: query_tokens.len() as u32,
                byte_from,
                byte_to,
                sti: 0,
                ordinal: pivot_match.ordinal,
            });
        }
    }

    // Dedup
    results.sort_by_key(|m| (m.doc_id, m.position));
    results.dedup_by_key(|m| (m.doc_id, m.position));

    results
}

// ─── resolve_trigrams_v3 ──────────────────────────────────────────────────

/// Trigram hit for position-based matching.
#[derive(Debug, Clone)]
struct TrigramHit {
    tri_idx: usize,
    doc_id: DocId,
    position: u32,
    byte_from: u32,
    byte_to: u32,
}

/// Generate n-grams from query text.
/// n=2 if query is short (len ≤ 3*(distance+1)), n=3 otherwise.
fn generate_trigrams(query: &str, distance: u8) -> (Vec<String>, usize) {
    let lower = query.to_lowercase();
    let bytes = lower.as_bytes();
    let n = if bytes.len() <= 3 * (distance as usize + 1) { 2 } else { 3 };

    let mut ngrams = Vec::new();
    if bytes.len() < n {
        return (ngrams, n);
    }
    for i in 0..=bytes.len() - n {
        // Respect UTF-8 boundaries
        if !lower.is_char_boundary(i) || !lower.is_char_boundary(i + n) {
            continue;
        }
        ngrams.push(lower[i..i + n].to_string());
    }
    (ngrams, n)
}

/// Fuzzy trigram pigeonhole resolution.
///
/// Pipeline:
/// 1. Generate trigrams from query (no concat_query — query stays as-is with seps)
/// 2. Estimate selectivity per trigram via fst_candidates_v3
/// 3. Resolve rarest first → build doc filter → resolve rest
/// 4. Two-pointer sliding window → find compact match zones
/// 5. Score by miss_count
///
/// Returns: (doc_bitset, highlights, doc_coverage)
pub fn resolve_trigrams_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    distance: u8,
    resolver: &dyn PostingResolver,
    strict_separators: bool,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();
    let mut doc_coverage: Vec<(DocId, f32)> = Vec::new();

    let (ngrams, n) = generate_trigrams(query, distance);
    if ngrams.is_empty() {
        return (doc_bitset, highlights, doc_coverage);
    }

    // Threshold: pigeonhole principle. No boundary correction (overlap covers all).
    let threshold = (ngrams.len() as i32 - n as i32 * distance as i32).max(1) as usize;
    let max_span = (query.len() as u32 / 4 + 1) + distance as u32;

    // Phase A: estimate selectivity per trigram (no posting resolution)
    let mut selectivity: Vec<(usize, usize)> = ngrams
        .iter()
        .enumerate()
        .map(|(i, gram)| {
            let count = fst_walk::fst_candidates_v3(reader, gram, false, strict_separators).len();
            (i, count)
        })
        .collect();
    selectivity.sort_by_key(|&(_, count)| count);

    // Phase B: resolve trigrams, rarest first
    let mut all_hits: Vec<TrigramHit> = Vec::new();
    let mut doc_filter: Option<HashSet<DocId>> = None;

    for &(gram_idx, _) in &selectivity {
        let cands = fst_walk::fst_candidates_v3(reader, &ngrams[gram_idx], false, strict_separators);
        let matches = resolve::resolve_single_v3(&cands, resolver, doc_filter.as_ref());

        for m in &matches {
            all_hits.push(TrigramHit {
                tri_idx: gram_idx,
                doc_id: m.doc_id,
                position: m.position,
                byte_from: m.byte_from,
                byte_to: m.byte_to,
            });
        }

        // Build doc filter from first few resolved trigrams
        if doc_filter.is_none() && !matches.is_empty() {
            let docs: HashSet<DocId> = matches.iter().map(|m| m.doc_id).collect();
            doc_filter = Some(docs);
        } else if let Some(ref mut filter) = doc_filter {
            for m in &matches {
                filter.insert(m.doc_id);
            }
        }
    }

    // Phase C: group hits by doc, find matching windows
    let mut hits_by_doc: std::collections::HashMap<DocId, Vec<&TrigramHit>> =
        std::collections::HashMap::new();
    for hit in &all_hits {
        hits_by_doc.entry(hit.doc_id).or_default().push(hit);
    }

    for (&doc_id, hits) in &hits_by_doc {
        // Sort by position
        let mut sorted: Vec<&&TrigramHit> = hits.iter().collect();
        sorted.sort_by_key(|h| h.position);

        // Count distinct trigram indices hit
        let distinct: HashSet<usize> = sorted.iter().map(|h| h.tri_idx).collect();

        if distinct.len() >= threshold {
            doc_bitset.insert(doc_id);

            // Compute highlight span
            let min_bf = sorted.iter().map(|h| h.byte_from).min().unwrap_or(0);
            let max_bt = sorted.iter().map(|h| h.byte_to).max().unwrap_or(0);
            highlights.push((doc_id, min_bf as usize, max_bt as usize));

            // Coverage score: negative miss count
            let miss_count = ngrams.len() - distinct.len();
            doc_coverage.push((doc_id, -(miss_count as f32)));
        }
    }

    (doc_bitset, highlights, doc_coverage)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::file_v3::SfxFileWriterV3;
    use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
    use crate::query::posting_resolver::PostingEntry;

    struct MockResolver(SfxPostReaderV2);

    impl MockResolver {
        fn new(data: &[u8]) -> Self {
            Self(SfxPostReaderV2::open_slice(data).unwrap())
        }
    }

    impl PostingResolver for MockResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            self.0.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id,
                position: e.token_index,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }).collect()
        }
    }

    fn build_index(texts: &[&str]) -> (Vec<u8>, Vec<u8>) {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();

        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(text, final_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        let num_terms = data.tokens.len();
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (final_ord, &old_ord) in data.sorted_indices.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in &data.token_postings[old_ord as usize] {
                post_writer.add_entry(final_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost = post_writer.finish();

        let writer = SfxFileWriterV3::new(fst_data, parent_data, data.num_docs);
        (writer.to_bytes(), sfxpost)
    }

    // ── find_literal_v3 ──

    #[test]
    fn test_find_literal_single_token() {
        let (sfx, post) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "tex" is within a single token
        let matches = find_literal_v3(&reader, "tex", &resolver, false, true, None);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
    }

    #[test]
    fn test_find_literal_cross_token() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex_lock" spans two tokens
        let matches = find_literal_v3(&reader, "mutex_lock", &resolver, false, true, None);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
        assert!(matches[0].span >= 2);
    }

    #[test]
    fn test_find_literal_sep_skip() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutexlock" (no sep) with strict_sep=false
        let matches = find_literal_v3(&reader, "mutexlock", &resolver, false, false, None);
        assert!(!matches.is_empty(), "sep-skip should find match");
    }

    #[test]
    fn test_find_literal_anchor_start() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex" with anchor_start → should find at SI=0
        let matches = find_literal_v3(&reader, "mutex_lo", &resolver, true, true, None);
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|m| m.sti == 0));

        // "tex" with anchor_start → NOT at SI=0
        let matches = find_literal_v3(&reader, "tex_lo", &resolver, true, true, None);
        assert!(matches.is_empty());
    }

    // ── find_multi_token_v3 ──

    #[test]
    fn test_multi_token_basic() {
        let (sfx, post) = build_index(&["mutex_lock_init"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let tokens = vec!["mutex_lo", "lock_in", "init"];
        let matches = find_multi_token_v3(&reader, &tokens, &resolver, false, false, true, None);
        assert!(!matches.is_empty(), "multi-token should match");
        assert_eq!(matches[0].span, 3);
    }

    #[test]
    fn test_multi_token_no_match() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "hello" + "world" not in "mutex_lock"
        let tokens = vec!["hello", "world"];
        let matches = find_multi_token_v3(&reader, &tokens, &resolver, false, false, true, None);
        assert!(matches.is_empty());
    }

    // ── generate_trigrams ──

    #[test]
    fn test_trigrams_basic() {
        let (grams, n) = generate_trigrams("mutex_lock", 1);
        assert_eq!(n, 3);
        assert!(grams.contains(&"mut".to_string()));
        assert!(grams.contains(&"x_l".to_string()));
        assert!(grams.contains(&"ock".to_string()));
    }

    #[test]
    fn test_trigrams_short_query() {
        let (grams, n) = generate_trigrams("abc", 1);
        // len=3 <= 3*(1+1)=6 → bigrams
        assert_eq!(n, 2);
        assert_eq!(grams, vec!["ab", "bc"]);
    }

    #[test]
    fn test_trigrams_with_seps() {
        // Query keeps seps — no concat_query
        let (grams, n) = generate_trigrams("mutex_lock", 1);
        assert_eq!(n, 3);
        // Sep byte "_" is part of the trigrams
        assert!(grams.contains(&"x_l".to_string()), "sep bytes should be in trigrams");
        assert!(grams.contains(&"ex_".to_string()));
        assert!(grams.contains(&"_lo".to_string()));
    }

    // ── resolve_trigrams_v3 ──

    #[test]
    fn test_fuzzy_basic() {
        let (sfx, post) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex_lck" d=1 (missing 'o') should find "mutex_lock"
        let (bitset, highlights, coverage) =
            resolve_trigrams_v3(&reader, "mutex_lck", 1, &resolver, true, 2);

        assert!(bitset.contains(0), "doc 0 should match fuzzy");
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_fuzzy_no_concat_query() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // Query with sep "_" kept as-is (NOT stripped by concat_query)
        // Trigrams include "x_l" which is in the FST thanks to overlap
        let (bitset, _, _) =
            resolve_trigrams_v3(&reader, "mutex_lock", 0, &resolver, true, 1);

        assert!(bitset.contains(0), "exact query should match via trigrams");
    }

    #[test]
    fn test_fuzzy_sep_skip() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutexlock" (no seps) d=1 strict_sep=false
        // Trigrams "exl" and "xlo" found in stripped partition
        let (bitset, _, _) =
            resolve_trigrams_v3(&reader, "mutexlock", 1, &resolver, false, 1);

        assert!(bitset.contains(0), "fuzzy with sep-skip should find match");
    }

    #[test]
    fn test_fuzzy_no_match() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "zzzzzzzzz" should not match anything
        let (bitset, _, _) =
            resolve_trigrams_v3(&reader, "zzzzzzzzz", 1, &resolver, true, 1);

        assert!(!bitset.contains(0));
    }
}
