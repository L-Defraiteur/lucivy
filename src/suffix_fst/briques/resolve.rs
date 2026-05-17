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
                byte_to: e.byte_from + cand.own_len as u32 - cand.sep_len as u32,
                sti: cand.sti,
                ordinal: cand.raw_ordinal,
            });
        }
    }

    results
}

// ─── resolve_chains_v3 ────────────────────────────────────────────────────

/// Resolve cross-token chains to document matches with strict adjacency.
///
/// For each chain, resolves posting lists for all ordinals and verifies that
/// they appear at consecutive positions (`pos+1`) in the same document.
pub fn resolve_chains_v3(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    resolve_chains_impl(chains, resolver, filter_docs, AdjacencyMode::Strict)
}

/// Resolve cross-token chains with relaxed adjacency for strict_sep=false.
///
/// Allows gaps between chain ordinals (pure-sep tokens in between).
/// Verifies that intermediate tokens are all non-alphanum via PosMap + ByteMap.
pub fn resolve_chains_v3_relaxed(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: Option<&crate::suffix_fst::posmap::PosMapReader<'_>>,
    bytemap: Option<&crate::suffix_fst::bytemap::ByteBitmapReader<'_>>,
) -> Vec<MatchV3> {
    if posmap.is_some() && bytemap.is_some() {
        resolve_chains_impl(chains, resolver, filter_docs,
            AdjacencyMode::Relaxed { posmap: posmap.unwrap(), bytemap: bytemap.unwrap() })
    } else {
        // No PosMap/ByteMap available — fallback to byte-ordered check
        resolve_chains_impl(chains, resolver, filter_docs, AdjacencyMode::ByteOrdered)
    }
}

enum AdjacencyMode<'a> {
    /// pos[i+1] == pos[i] + 1
    Strict,
    /// pos[i+1] > pos[i], intermediate tokens verified as pure non-alphanum via ByteMap
    Relaxed {
        posmap: &'a crate::suffix_fst::posmap::PosMapReader<'a>,
        bytemap: &'a crate::suffix_fst::bytemap::ByteBitmapReader<'a>,
    },
    /// pos[i+1] > pos[i] && byte_from[i+1] >= byte_to[i] (no verification, fallback)
    ByteOrdered,
}

fn resolve_chains_impl(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    adjacency: AdjacencyMode<'_>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    for chain in chains {
        if chain.ordinals.is_empty() {
            continue;
        }
        if chain.ordinals.len() == 1 {
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

        // Multi-ordinal chain
        let first_entries = resolve_ordinal(resolver, chain.ordinals[0], filter_docs);

        // Active set: (doc_id, prev_position, byte_from_first, byte_to_prev)
        let mut active: Vec<(DocId, u32, u32, u32)> = first_entries
            .iter()
            .map(|e| (e.doc_id, e.position, e.byte_from + chain.first_sti as u32, e.byte_to))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() {
                break;
            }

            let ord = chain.ordinals[ord_idx];
            let entries = resolver.resolve(ord);

            let mut new_active: Vec<(DocId, u32, u32, u32)> = Vec::new();

            for &(doc_id, prev_pos, byte_from_first, byte_to_prev) in &active {
                for e in &entries {
                    if e.doc_id != doc_id {
                        continue;
                    }

                    let valid = match &adjacency {
                        AdjacencyMode::Strict => {
                            e.position == prev_pos + 1
                        }
                        AdjacencyMode::ByteOrdered => {
                            e.position > prev_pos && e.byte_from >= byte_to_prev
                        }
                        AdjacencyMode::Relaxed { posmap, bytemap } => {
                            if e.position <= prev_pos {
                                false
                            } else if e.position == prev_pos + 1 {
                                true // directly adjacent, always OK
                            } else {
                                // Check intermediate tokens are all pure non-alphanum via ByteMap
                                intermediates_are_pure_sep(
                                    *posmap, *bytemap,
                                    doc_id, prev_pos + 1, e.position,
                                )
                            }
                        }
                    };

                    if valid {
                        new_active.push((doc_id, e.position, byte_from_first, e.byte_to));
                        break;
                    }
                }
            }

            active = new_active;
        }

        // Emit matches
        for &(doc_id, _last_pos, byte_from, byte_to) in &active {
            let position = first_entries.iter()
                .find(|e| e.doc_id == doc_id)
                .map(|e| e.position)
                .unwrap_or(0);
            results.push(MatchV3 {
                doc_id,
                position,
                span: chain.ordinals.len() as u32,
                byte_from,
                byte_to,
                sti: chain.first_sti,
                ordinal: chain.ordinals[0],
            });
        }
    }

    results
}

/// Check that all tokens between pos_from (inclusive) and pos_to (exclusive)
/// are pure non-alphanum (separator-only tokens).
///
/// Uses PosMap (position → ordinal) + ByteMap (ordinal → byte bitmap).
/// A token is "pure sep" if it contains no content bytes.
/// Content bytes = ASCII alphanumeric OR non-ASCII (>= 0x80, i.e. emoji, CJK, accented, etc.).
/// Consistent with `is_content_char()` in the tokenizer.
fn intermediates_are_pure_sep(
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
    doc_id: DocId,
    pos_from: u32,
    pos_to: u32,
) -> bool {
    // Content byte ranges: ASCII alphanumeric + non-ASCII (UTF-8 lead/continuation bytes)
    const CONTENT_RANGES: &[(u8, u8)] = &[
        (b'0', b'9'),
        (b'A', b'Z'),
        (b'a', b'z'),
        (0x80, 0xFF), // non-ASCII bytes → content (emoji, CJK, accented, etc.)
    ];

    for pos in pos_from..pos_to {
        let Some(ord) = posmap.ordinal_at(doc_id, pos) else {
            return false; // Can't verify → reject
        };
        // Check via ByteMap: if any content byte is present → not pure sep
        if bytemap.bytes_in_ranges(ord as u32, CONTENT_RANGES) {
            return false;
        }
    }
    true
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
