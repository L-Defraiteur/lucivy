//! Query orchestrators for SFX v3.
//!
//! Thin wrappers that validate input and route to the correct briques.
//! Each function is the public entry point for one query type.
//!
//! - `contains_v3`: exact substring search (single + cross-token)
//! - `fuzzy_v3`: fuzzy substring search via trigram pigeonhole
//! - `regex_v3`: regex search (TODO — needs DFA integration)

use std::collections::HashSet;

use crate::tokenizer::equal_chunk::is_content_char;

use common::BitSet;

use crate::DocId;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file_v3::SfxFileReaderV3;

use super::composite;
use super::resolve::MatchV3;

/// Maximum query length in bytes. Queries longer than this are rejected.
const MAX_QUERY_LEN: usize = 2048;

// ─── contains_v3 ──────────────────────────────────────────────────────────

/// Exact substring search (d=0).
///
/// Finds all occurrences of `query` in the index, handling cross-token
/// boundaries via falling walk chain.
///
/// Parameters:
/// - `anchor_start`: only match at token start (SI=0), for startsWith
/// - `exact_match`: match must cover entire word(s), for term()
/// - `strict_separators`: if false, tolerates different/missing separators
/// - `filter_docs`: optional doc_id filter for rarest-first optimization
///
/// Returns matches sorted by (doc_id, position).
pub fn contains_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    resolver: &dyn PostingResolver,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    // Validate input
    if query.is_empty() || query.len() > MAX_QUERY_LEN {
        return Vec::new();
    }

    // For strict_sep=false: strip all non-alphanum from the query.
    // The stripped partition (0x02) in the FST will match content+overlap without seps.
    let effective_query;
    let query_ref = if !strict_separators {
        effective_query = query.chars().filter(|c| is_content_char(*c)).collect::<String>();
        if effective_query.is_empty() {
            return Vec::new();
        }
        effective_query.as_str()
    } else {
        query
    };

    let mut matches = composite::find_literal_v3(
        reader, query_ref, resolver, anchor_start, strict_separators, filter_docs,
    );

    // Apply exact_match filter: match must cover exactly the content of the word(s)
    if exact_match {
        let query_lower = query.to_lowercase();
        let query_len = query_lower.len() as u32;
        matches.retain(|m| {
            // For exact match: the byte span must equal the query length
            // (accounting for STI offset in the first token)
            (m.byte_to - m.byte_from) == query_len
        });
    }

    matches
}

// ─── fuzzy_v3 ─────────────────────────────────────────────────────────────

/// Fuzzy substring search via trigram pigeonhole principle.
///
/// Query is used as-is (no concat_query, no separator stripping).
/// Threshold = max(T - n*d, 1) — no boundary trigram compensation needed
/// because the overlap covers all cross-boundary trigrams.
///
/// Parameters:
/// - `distance`: Levenshtein edit distance tolerance (1-3 typical)
/// - `strict_separators`: if false, searches stripped partition too
///
/// Returns: (doc_bitset, highlights, doc_coverage)
/// - doc_bitset: which docs matched
/// - highlights: (doc_id, byte_from, byte_to) per match
/// - doc_coverage: (doc_id, score) where score = -(miss_count as f32)
pub fn fuzzy_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    distance: u8,
    resolver: &dyn PostingResolver,
    strict_separators: bool,
    max_doc: DocId,
) -> (BitSet, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>) {
    // Validate input
    if query.is_empty() || query.len() > MAX_QUERY_LEN || distance > 3 {
        return (BitSet::with_max_value(max_doc), Vec::new(), Vec::new());
    }

    // For strict_sep=false: strip non-alphanum from the query
    let effective_query;
    let query_ref = if !strict_separators {
        effective_query = query.chars().filter(|c| is_content_char(*c)).collect::<String>();
        if effective_query.is_empty() {
            return (BitSet::with_max_value(max_doc), Vec::new(), Vec::new());
        }
        effective_query.as_str()
    } else {
        query
    };

    // d=0 → route to exact contains (no trigram overhead)
    if distance == 0 {
        let matches = contains_v3(reader, query_ref, resolver, false, false, strict_separators, None);
        let mut bitset = BitSet::with_max_value(max_doc);
        let mut highlights = Vec::new();
        let mut coverage = Vec::new();
        for m in &matches {
            bitset.insert(m.doc_id);
            highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
            coverage.push((m.doc_id, 0.0)); // 0 misses = perfect match
        }
        return (bitset, highlights, coverage);
    }

    composite::resolve_trigrams_v3(
        reader, query_ref, distance, resolver, strict_separators, max_doc,
    )
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

    // ── contains_v3 ──

    #[test]
    fn test_contains_basic() {
        let (sfx, post) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let matches = contains_v3(&reader, "mutex", &resolver, false, false, true, None);
        assert!(!matches.is_empty());
        assert_eq!(matches[0].doc_id, 0);
    }

    #[test]
    fn test_contains_cross_token() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let matches = contains_v3(&reader, "mutex_lock", &resolver, false, false, true, None);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_contains_sep_skip() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutexlock" (no sep) → should match "mutex_lock" with strict_sep=false
        let matches = contains_v3(&reader, "mutexlock", &resolver, false, false, false, None);
        assert!(!matches.is_empty(), "sep-skip should work");
    }

    #[test]
    fn test_contains_strict_rejects() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex lock" (space) strict=true → should NOT match "mutex_lock" (underscore)
        let matches = contains_v3(&reader, "mutex lock", &resolver, false, false, true, None);
        assert!(matches.is_empty(), "strict should reject different separator");
    }

    #[test]
    fn test_contains_anchor_start() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex" with anchor → found (starts at SI=0)
        let matches = contains_v3(&reader, "mutex_lo", &resolver, true, false, true, None);
        assert!(!matches.is_empty());

        // "tex" with anchor → not found (SI>0)
        let matches = contains_v3(&reader, "tex_lo", &resolver, true, false, true, None);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_contains_empty_query() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let matches = contains_v3(&reader, "", &resolver, false, false, true, None);
        assert!(matches.is_empty());
    }

    // ── fuzzy_v3 ──

    #[test]
    fn test_fuzzy_basic() {
        let (sfx, post) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex_lck" d=1 → should find "mutex_lock"
        let (bitset, highlights, _) = fuzzy_v3(&reader, "mutex_lck", 1, &resolver, true, 2);
        assert!(bitset.contains(0), "doc 0 should match fuzzy");
        assert!(!highlights.is_empty());
    }

    #[test]
    fn test_fuzzy_d0_routes_to_exact() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // d=0 → exact match via contains_v3
        let (bitset, _, coverage) = fuzzy_v3(&reader, "mutex_lo", 0, &resolver, true, 1);
        assert!(bitset.contains(0));
        // Coverage should be 0.0 (perfect match, no misses)
        assert!(coverage.iter().any(|&(_, score)| score == 0.0));
    }

    #[test]
    fn test_fuzzy_sep_skip() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutexlck" d=1 strict_sep=false
        let (bitset, _, _) = fuzzy_v3(&reader, "mutexlck", 1, &resolver, false, 1);
        assert!(bitset.contains(0), "fuzzy + sep-skip should find match");
    }

    #[test]
    fn test_fuzzy_no_match() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let (bitset, _, _) = fuzzy_v3(&reader, "zzzzzzzzz", 1, &resolver, true, 1);
        assert!(!bitset.contains(0));
    }

    #[test]
    fn test_fuzzy_distance_too_high() {
        let (sfx, post) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // d=4 → rejected (max 3)
        let (bitset, _, _) = fuzzy_v3(&reader, "mutex", 4, &resolver, true, 1);
        assert!(!bitset.contains(0));
    }
}
