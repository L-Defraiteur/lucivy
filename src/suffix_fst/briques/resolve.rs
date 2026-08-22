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
    /// Ordinal of the last token (for chain verification). Same as ordinal when span=1.
    pub last_ordinal: u64,
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
        // Word-stripped ordinals (partition 0x02) have empty postings in sfxpost.
        // Their postings are in WordSfxPost, resolved by resolve_word_chains_v3.
        // Skip them here to avoid phantom matches.
        if cand.is_word_stripped() { continue; }

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
                last_ordinal: cand.raw_ordinal,
            });
        }
    }

    results
}

// ─── resolve_single_word_v3 ───────────────────────────────────────────────

/// Resolve word-stripped candidates (partition 0x02) directly via WordSfxPost.
///
/// Symmetric to resolve_single_v3 for chunks: candidates that match within a
/// single word-stripped token are resolved here instead of depending on the
/// chain pipeline. This guarantees word-level single matches are found by
/// construction, not by luck of chain formation.
pub fn resolve_single_word_v3(
    candidates: &[FstCandidateV3],
    word_sfxpost: &crate::suffix_fst::word_sfxpost::WordSfxPostReader<'_>,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    for cand in candidates {
        if !cand.is_word_stripped() { continue; }

        let entries = word_sfxpost.entries(cand.raw_ordinal as u32);
        for e in &entries {
            if let Some(filter) = filter_docs {
                if !filter.contains(&e.doc_id) { continue; }
            }
            // byte_from/byte_to from WordSfxPost are word-level coordinates.
            // Adjust byte_from by sti (suffix offset within the word) and
            // compute byte_to to cover exactly the query match, not the whole word.
            let content_len = cand.content_len() as u32;
            let query_byte_len = content_len - cand.sti as u32;
            results.push(MatchV3 {
                doc_id: e.doc_id,
                position: e.first_position,
                span: if e.last_position > e.first_position {
                    e.last_position - e.first_position + 1
                } else { 1 },
                byte_from: e.byte_from + cand.sti as u32,
                byte_to: e.byte_from + cand.sti as u32 + query_byte_len,
                sti: cand.sti,
                ordinal: cand.raw_ordinal,
                last_ordinal: cand.raw_ordinal,
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
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
) -> Vec<MatchV3> {
    resolve_chains_impl(chains, resolver, filter_docs,
        AdjacencyMode::Relaxed { posmap, bytemap })
}

/// Resolve cross-word chains using the WordSfxPost (partition 0x02 postings).
///
/// Word postings have `last_position` (for adjacency) and `byte_from` from
/// the first chunk (for highlights). Relaxed adjacency checks intermediate
/// tokens via posmap/bytemap.
/// Resolve cross-word chains using WordSfxPost + chunk PostingResolver.
///
/// Word chains may contain ordinals from both partition 0x02 (word-stripped,
/// resolved via WordSfxPost) and partitions 0x00/0x01 (chunks, resolved via
/// PostingResolver). Each ordinal is looked up in the word sfxpost first;
/// if not found, falls back to the chunk resolver.
pub fn resolve_word_chains_v3(
    chains: &[TokenChainV3],
    word_sfxpost: &crate::suffix_fst::word_sfxpost::WordSfxPostReader<'_>,
    chunk_resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
) -> Vec<MatchV3> {
    use crate::suffix_fst::word_sfxpost::WordPostingEntry;

    let mut results = Vec::new();

    for chain in chains {
        if chain.ordinals.is_empty() {
            continue;
        }

        // Resolve first position from word sfxpost, fall back to chunk resolver
        let first_entries: Vec<WordPostingEntry> = chain.ordinals[0].iter()
            .flat_map(|&ord| {
                let word_entries = word_sfxpost.entries(ord as u32);
                let entries: Vec<WordPostingEntry> = if !word_entries.is_empty() {
                    word_entries
                } else {
                    let chunk = if let Some(filter) = filter_docs {
                        chunk_resolver.resolve_filtered(ord, filter)
                    } else {
                        chunk_resolver.resolve(ord)
                    };
                    chunk.into_iter().map(|e| WordPostingEntry {
                        doc_id: e.doc_id,
                        first_position: e.position,
                        last_position: e.position,
                        byte_from: e.byte_from,
                        byte_to: e.byte_to,
                    }).collect()
                };
                let mut filtered = entries;
                if let Some(filter) = filter_docs {
                    filtered.retain(|e| filter.contains(&e.doc_id));
                }
                filtered
            })
            .collect();

        if chain.ordinals.len() == 1 {
            for e in &first_entries {
                let bf = e.byte_from + chain.first_sti as u32;
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.first_position,
                    span: if e.last_position > e.first_position {
                        e.last_position - e.first_position + 1
                    } else { 1 },
                    byte_from: bf,
                    byte_to: bf + chain.total_query_consumed as u32,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: chain.ordinals[0][0],
                });
            }
            continue;
        }

        // Multi-position chain
        // Active: (doc_id, prev_last_position, byte_from_first, byte_to_prev, last_ord)
        let mut active: Vec<(DocId, u32, u32, u32, u64)> = first_entries.iter()
            .map(|e| (e.doc_id, e.last_position, e.byte_from + chain.first_sti as u32, e.byte_to, 0u64))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() { break; }

            // Resolve from word sfxpost first, fall back to chunk resolver
            let mut entries: Vec<(WordPostingEntry, u64)> = Vec::new();
            for &ord in &chain.ordinals[ord_idx] {
                let word_entries = word_sfxpost.entries(ord as u32);
                if !word_entries.is_empty() {
                    for e in word_entries {
                        entries.push((e, ord));
                    }
                } else {
                    // Chunk ordinal — convert PostingEntry to WordPostingEntry
                    let chunk_entries = if let Some(filter) = filter_docs {
                        chunk_resolver.resolve_filtered(ord, filter)
                    } else {
                        chunk_resolver.resolve(ord)
                    };
                    for e in chunk_entries {
                        entries.push((WordPostingEntry {
                            doc_id: e.doc_id,
                            first_position: e.position,
                            last_position: e.position, // chunk = single position
                            byte_from: e.byte_from,
                            byte_to: e.byte_to,
                        }, ord));
                    }
                }
            }

            let mut new_active: Vec<(DocId, u32, u32, u32, u64)> = Vec::new();

            for &(doc_id, prev_last_pos, byte_from_first, _, _) in &active {
                for (e, ord) in &entries {
                    if e.doc_id != doc_id { continue; }
                    // Use first_position of the next word for adjacency check
                    // against last_position of the previous word
                    let next_first_pos = e.first_position;
                    let valid = if next_first_pos <= prev_last_pos {
                        false
                    } else if next_first_pos == prev_last_pos + 1 {
                        true // directly adjacent
                    } else {
                        // Check intermediates between prev last chunk and next first chunk
                        intermediates_are_pure_sep(
                            posmap, bytemap,
                            doc_id, prev_last_pos + 1, next_first_pos,
                        )
                    };

                    if valid {
                        new_active.push((doc_id, e.last_position, byte_from_first, e.byte_to, *ord));
                        break;
                    }
                }
            }

            active = new_active;
        }

        // Emit matches
        for &(doc_id, _, byte_from, byte_to, last_ord) in &active {
            let position = first_entries.iter()
                .find(|e| e.doc_id == doc_id)
                .map(|e| e.first_position)
                .unwrap_or(0);
            results.push(MatchV3 {
                doc_id,
                position,
                span: chain.ordinals.len() as u32,
                byte_from,
                byte_to,
                sti: chain.first_sti,
                ordinal: chain.ordinals[0][0],
                last_ordinal: last_ord,
            });
        }
    }

    results
}

enum AdjacencyMode<'a> {
    /// pos[i+1] == pos[i] + 1
    Strict,
    /// pos[i+1] > pos[i], intermediate tokens verified as pure non-alphanum via ByteMap.
    /// PosMap + ByteMap are REQUIRED — no fallback to unverified byte ordering.
    Relaxed {
        posmap: &'a crate::suffix_fst::posmap::PosMapReader<'a>,
        bytemap: &'a crate::suffix_fst::bytemap::ByteBitmapReader<'a>,
    },
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

        // Resolve all alternatives at position 0
        let first_entries = resolve_alternatives(resolver, &chain.ordinals[0], filter_docs);

        if chain.ordinals.len() == 1 {
            for e in &first_entries {
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.position,
                    span: 1,
                    byte_from: e.byte_from + chain.first_sti as u32,
                    byte_to: e.byte_to,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: chain.ordinals[0][0],
                });
            }
            continue;
        }

        // Multi-position chain
        // Active set: (doc_id, prev_position, byte_from_first, byte_to_prev, last_ord_matched)
        let mut active: Vec<(DocId, u32, u32, u32, u64)> = first_entries
            .iter()
            .map(|e| (e.doc_id, e.position, e.byte_from + chain.first_sti as u32, e.byte_to, 0u64))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() {
                break;
            }

            // Union postings from all alternative ordinals at this position,
            // tagging each with its ordinal for word map verification.
            let mut entries: Vec<(PostingEntry, u64)> = Vec::new();
            for &ord in &chain.ordinals[ord_idx] {
                for e in resolver.resolve(ord) {
                    entries.push((e, ord));
                }
            }

            let mut new_active: Vec<(DocId, u32, u32, u32, u64)> = Vec::new();

            for &(doc_id, prev_pos, byte_from_first, byte_to_prev, _) in &active {
                for (e, ord) in &entries {
                    if e.doc_id != doc_id {
                        continue;
                    }

                    let valid = match &adjacency {
                        AdjacencyMode::Strict => {
                            e.position == prev_pos + 1
                        }
                        AdjacencyMode::Relaxed { posmap, bytemap } => {
                            if e.position <= prev_pos {
                                false
                            } else if e.position == prev_pos + 1 {
                                true // directly adjacent, always OK
                            } else {
                                intermediates_are_pure_sep(
                                    *posmap, *bytemap,
                                    doc_id, prev_pos + 1, e.position,
                                )
                            }
                        }
                    };

                    if valid {
                        new_active.push((doc_id, e.position, byte_from_first, e.byte_to, *ord));
                        break;
                    }
                }
            }

            active = new_active;
        }

        // Emit matches
        for &(doc_id, _last_pos, byte_from, byte_to, last_ord) in &active {
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
                ordinal: chain.ordinals[0][0],
                last_ordinal: last_ord,
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

/// Resolve alternative ordinals at one chain position, union results.
fn resolve_alternatives(
    resolver: &dyn PostingResolver,
    ordinals: &[u64],
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<PostingEntry> {
    let mut entries = Vec::new();
    for &ord in ordinals {
        entries.extend(resolve_ordinal(resolver, ord, filter_docs));
    }
    entries
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
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(text, content_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        for ws in &data.word_stripped {
            let final_ord = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(
                &ws.word_content, &ws.content_overlap,
                final_ord as u64, ws.first_own_len, ws.last_sep_len, ws.is_word_start,
            );
        }
        let (fst_data, parent_data) = builder.build().unwrap();

        // Build sfxpost
        let num_terms = data.num_content_ords;
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (content_ord, postings) in data.content_postings.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in postings {
                post_writer.add_entry(content_ord as u32, doc_id, ti, bf, bt);
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
