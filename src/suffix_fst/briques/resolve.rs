//! Tier 2 — Posting resolution for SFX v3.
//!
//! Converts FST results (ordinals/candidates) into document matches
//! with adjacency verification for cross-token chains.
//!
//! - `resolve_single_v3`: single-token candidates → doc matches
//! - `resolve_chains_v3`: cross-token chains → doc matches with position adjacency
//! - `selectivity_v3`: estimate selectivity without resolving postings

use std::collections::HashSet;

use crate::DocId;
use crate::query::posting_resolver::{PostingEntry, PostingResolver};

use super::fst_walk::{FstCandidateV3, TokenChainV3};

// ─── Types ─────────────────────────────────────────────────────────────────

/// Unified match result from posting resolution.
#[derive(Debug, Clone)]
pub struct MatchV3 {
    /// Document containing the match.
    pub doc_id: DocId,
    /// Token position of the first token in the match.
    pub position: u32,
    /// Number of tokens covered by the match.
    pub span: u32,
    /// Start byte offset in the original text.
    pub byte_from: u32,
    /// End byte offset (exclusive) in the original text.
    pub byte_to: u32,
    /// STI in the first token.
    pub sti: u16,
    /// Ordinal of the first token.
    pub ordinal: u64,
}

// ─── resolve_single_v3 ────────────────────────────────────────────────────

/// Resolve single-token candidates to document matches.
///
/// Each candidate is an FST entry where the query matches within a single token.
/// Resolves posting lists and optionally filters by doc_id set.
pub fn resolve_single_v3(
    candidates: &[FstCandidateV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    for cand in candidates {
        let entries = if let Some(filter) = filter_docs {
            resolver.resolve_filtered(cand.raw_ordinal, filter)
        } else {
            resolver.resolve(cand.raw_ordinal)
        };

        for e in &entries {
            results.push(MatchV3 {
                doc_id: e.doc_id,
                position: e.position,
                span: 1,
                byte_from: e.byte_from + cand.sti as u32,
                byte_to: e.byte_from + cand.sti as u32 + cand.own_len as u32 - cand.sep_len as u32,
                sti: cand.sti,
                ordinal: cand.raw_ordinal,
            });
        }
    }

    results
}

// ─── resolve_chains_v3 ────────────────────────────────────────────────────

/// Resolve cross-token chains to document matches with adjacency verification.
///
/// For each chain, resolves posting lists for all ordinals and verifies that
/// they appear at consecutive positions in the same document.
///
/// Adjacency rule: `position[i+1] == position[i] + 1` for each pair.
pub fn resolve_chains_v3(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    for chain in chains {
        if chain.ordinals.is_empty() {
            continue;
        }
        if chain.ordinals.len() == 1 {
            // Single-token chain — just resolve directly
            let entries = resolve_ordinal(resolver, chain.ordinals[0], filter_docs);
            for e in &entries {
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.position,
                    span: 1,
                    byte_from: e.byte_from + chain.first_sti as u32,
                    byte_to: e.byte_to,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0],
                });
            }
            continue;
        }

        // Multi-ordinal chain: resolve first ordinal, then verify adjacency
        let first_entries = resolve_ordinal(resolver, chain.ordinals[0], filter_docs);

        // Build active set: (doc_id, next_expected_position, byte_from_first, byte_to_prev)
        let mut active: Vec<(DocId, u32, u32, u32)> = first_entries
            .iter()
            .map(|e| (e.doc_id, e.position + 1, e.byte_from + chain.first_sti as u32, e.byte_to))
            .collect();

        // Walk through remaining ordinals in the chain
        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() {
                break;
            }

            let ord = chain.ordinals[ord_idx];
            // Collect all postings for this ordinal (no doc filter — we filter by active set)
            let entries = resolver.resolve(ord);

            let mut new_active: Vec<(DocId, u32, u32, u32)> = Vec::new();

            for &(doc_id, expected_pos, byte_from_first, _byte_to_prev) in &active {
                // Find an entry at the expected position in this document
                for e in &entries {
                    if e.doc_id == doc_id && e.position == expected_pos {
                        new_active.push((
                            doc_id,
                            expected_pos + 1,
                            byte_from_first,
                            e.byte_to,
                        ));
                        break; // One match per active entry per ordinal
                    }
                }
            }

            active = new_active;
        }

        // Emit matches from surviving active entries
        for (doc_id, _next_pos, byte_from, byte_to) in &active {
            results.push(MatchV3 {
                doc_id: *doc_id,
                position: 0, // Will be set from first entry
                span: chain.ordinals.len() as u32,
                byte_from: *byte_from,
                byte_to: *byte_to,
                sti: chain.first_sti,
                ordinal: chain.ordinals[0],
            });
        }

        // Fix position from first entries
        for m in results.iter_mut().rev().take(active.len()) {
            if let Some(fe) = first_entries.iter().find(|e| e.doc_id == m.doc_id) {
                m.position = fe.position;
            }
        }
    }

    results
}

/// Resolve an ordinal with optional doc filtering.
fn resolve_ordinal(
    resolver: &dyn PostingResolver,
    ordinal: u64,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<PostingEntry> {
    if let Some(filter) = filter_docs {
        resolver.resolve_filtered(ordinal, filter)
    } else {
        resolver.resolve(ordinal)
    }
}

// ─── selectivity_v3 ───────────────────────────────────────────────────────

/// Estimate selectivity of a query without resolving postings.
///
/// Returns the number of FST candidates (single-token) + chain candidates (cross-token).
/// Lower = more selective = resolve first in rarest-first ordering.
pub fn selectivity_v3(
    reader: &crate::suffix_fst::file_v3::SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
) -> usize {
    let cands = super::fst_walk::fst_candidates_v3(reader, query, false, strict_separators);
    let chains = super::fst_walk::cross_token_chain_v3(reader, query, strict_separators);
    cands.len() + chains.len()
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::file_v3::{SfxFileReaderV3, SfxFileWriterV3};
    use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;

    /// Mock PostingResolver backed by SfxPostReaderV2.
    struct MockResolver {
        data: SfxPostReaderV2,
        /// Remap from final ordinals to sfxpost ordinals (identity in tests).
        remap: Vec<u32>,
    }

    impl MockResolver {
        fn new(sfxpost_bytes: &[u8], num_terms: usize) -> Self {
            let data = SfxPostReaderV2::open_slice(sfxpost_bytes).unwrap();
            Self {
                data,
                remap: (0..num_terms as u32).collect(),
            }
        }
    }

    impl PostingResolver for MockResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            let entries = self.data.entries(ordinal as u32);
            entries.into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id,
                position: e.token_index,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }).collect()
        }
    }

    /// Build everything from text values, return (sfx_bytes, sfxpost_bytes, num_terms).
    fn build_index(texts: &[&str]) -> (Vec<u8>, Vec<u8>, usize) {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();

        // Build FST
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(text, final_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        // Build sfxpost
        let num_terms = data.tokens.len();
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (final_ord, &old_ord) in data.sorted_indices.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in &data.token_postings[old_ord as usize] {
                post_writer.add_entry(final_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost_data = post_writer.finish();

        let writer = SfxFileWriterV3::new(fst_data, parent_data, data.num_docs);
        (writer.to_bytes(), sfxpost_data, num_terms)
    }

    // ── resolve_single_v3 ──

    #[test]
    fn test_resolve_single_basic() {
        let (sfx, post, nt) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post, nt);

        let cands = super::super::fst_walk::fst_candidates_v3(&reader, "mutex_lo", false, true);
        let matches = resolve_single_v3(&cands, &resolver, None);

        assert!(!matches.is_empty(), "should find matches");
        assert_eq!(matches[0].doc_id, 0);
        assert_eq!(matches[0].span, 1);
    }

    #[test]
    fn test_resolve_single_filtered() {
        let (sfx, post, nt) = build_index(&["mutex_lock", "mutex_core"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post, nt);

        let cands = super::super::fst_walk::fst_candidates_v3(&reader, "mutex_lo", false, true);

        // Filter to doc 0 only
        let filter: HashSet<DocId> = [0].into();
        let matches = resolve_single_v3(&cands, &resolver, Some(&filter));

        assert!(matches.iter().all(|m| m.doc_id == 0), "should only have doc 0");
    }

    // ── resolve_chains_v3 ──

    #[test]
    fn test_resolve_chain_two_tokens() {
        let (sfx, post, nt) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post, nt);

        let chains = super::super::fst_walk::cross_token_chain_v3(&reader, "mutex_lock", true);
        let matches = resolve_chains_v3(&chains, &resolver, None);

        assert!(!matches.is_empty(), "should resolve cross-token chain");
        let m = &matches[0];
        assert_eq!(m.doc_id, 0);
        assert_eq!(m.span, 2); // 2 tokens
    }

    #[test]
    fn test_resolve_chain_adjacency_verified() {
        let (sfx, post, nt) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post, nt);

        // "mutex_lock" chain should only match doc 0, not doc 1
        let chains = super::super::fst_walk::cross_token_chain_v3(&reader, "mutex_lock", true);
        let matches = resolve_chains_v3(&chains, &resolver, None);

        let doc_ids: HashSet<DocId> = matches.iter().map(|m| m.doc_id).collect();
        assert!(doc_ids.contains(&0), "doc 0 should match");
        // doc 1 has "hello_world" not "mutex_lock" → should not match
        assert!(!doc_ids.contains(&1), "doc 1 should not match");
    }

    #[test]
    fn test_resolve_chain_sep_skip() {
        let (sfx, post, nt) = build_index(&["mutex_lock"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post, nt);

        // "mutexlock" strict_sep=false → should find via stripped partition
        let chains = super::super::fst_walk::cross_token_chain_v3(&reader, "mutexlock", false);
        let matches = resolve_chains_v3(&chains, &resolver, None);

        assert!(!matches.is_empty(), "sep-skip chain should resolve");
        assert_eq!(matches[0].doc_id, 0);
    }

    // ── selectivity_v3 ──

    #[test]
    fn test_selectivity() {
        let (sfx, _, _) = build_index(&["mutex_lock", "hello_world", "foo_bar"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();

        let s = selectivity_v3(&reader, "mutex_lo", true);
        assert!(s > 0, "known token should have selectivity > 0");

        let s_none = selectivity_v3(&reader, "zzzzzzz", true);
        assert_eq!(s_none, 0, "unknown token should have selectivity 0");
    }
}
