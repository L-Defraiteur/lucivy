//! Tier 1 — FST walk primitives for SFX v3.
//!
//! Pure FST operations: no posting resolution, no doc filtering.
//!
//! - `fst_candidates_v3`: find all suffix entries matching a literal
//! - `falling_walk_v3`: byte-by-byte walk with split detection + overlap validation
//! - `cross_token_chain_v3`: chain falling walks across token boundaries

use lucivy_fst::raw;

use crate::suffix_fst::builder::SI0_PREFIX;
use crate::suffix_fst::builder::SI_REST_PREFIX;

/// Snap a byte position to the next valid UTF-8 char boundary.
/// If `pos` is already a boundary, returns it unchanged.
/// If `pos` is past the end, returns `len`.
fn snap_to_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
use crate::suffix_fst::builder_v3::{
    decode_output_v3, ParentEntryV3, ParentRefV3, SI_STRIPPED_PREFIX,
};
use crate::suffix_fst::file_v3::SfxFileReaderV3;

// ─── Types ─────────────────────────────────────────────────────────────────

/// A candidate from a direct FST lookup.
#[derive(Debug, Clone)]
pub struct FstCandidateV3 {
    pub raw_ordinal: u64,
    pub sti: u16,
    pub own_len: u16,
    pub sep_len: u8,
    pub overlap_len: u8,
    pub is_word_start: bool,
}

impl FstCandidateV3 {
    pub fn content_len(&self) -> u16 {
        self.own_len - self.sep_len as u16
    }

    fn from_parent(p: &ParentEntryV3) -> Self {
        Self {
            raw_ordinal: p.raw_ordinal,
            sti: p.sti,
            own_len: p.own_len,
            sep_len: p.sep_len,
            overlap_len: p.overlap_len,
            is_word_start: p.is_word_start,
        }
    }
}

/// A split candidate: the query prefix reaches a token boundary.
#[derive(Debug, Clone)]
pub struct SplitCandidateV3 {
    /// Bytes of the query consumed by this token's content+sep (up to own_len).
    pub query_consumed: usize,
    /// The parent entry.
    pub parent: ParentEntryV3,
    /// Byte offset in the query where the next token starts.
    pub remainder_start: usize,
    /// Number of overlap bytes validated (0..overlap_len).
    pub overlap_validated: usize,
}

/// A chain of tokens matching a query across token boundaries.
///
/// Each position stores alternative ordinals: different tokens may match
/// the same query fragment (e.g., "ion" vs "ions" for remainder "ion").
/// Resolve unions postings from all alternatives before adjacency check.
#[derive(Debug, Clone)]
pub struct TokenChainV3 {
    /// `ordinals[i]` = alternative ordinals at chain position i.
    /// Typically 1 element; multiple when the query prefix-matches
    /// several content keys at that position.
    pub ordinals: Vec<Vec<u64>>,
    pub first_sti: u16,
    pub total_query_consumed: usize,
}

// ─── fst_candidates_v3 ────────────────────────────────────────────────────

/// Find all suffix entries matching the given literal (exact key match).
///
/// Partitions searched:
/// - anchor_start=true: 0x00 only
/// - strict_sep=true: 0x00 + 0x01
/// - strict_sep=false: 0x00 + 0x01 + 0x02 (includes sep-stripped)
pub fn fst_candidates_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
) -> Vec<FstCandidateV3> {
    let lower = query.to_lowercase();
    let query_bytes = lower.as_bytes();
    let fst = reader.fst();
    let mut results = Vec::new();

    let partitions: &[u8] = if anchor_start && strict_separators {
        &[SI0_PREFIX]
    } else if anchor_start && !strict_separators {
        &[SI0_PREFIX, SI_STRIPPED_PREFIX]
    } else if strict_separators {
        &[SI0_PREFIX, SI_REST_PREFIX]
    } else {
        &[SI0_PREFIX, SI_REST_PREFIX, SI_STRIPPED_PREFIX]
    };

    for &partition in partitions {
        // Build range: ge = [partition, query...], lt = [partition, query with last byte +1]
        let mut ge_key = vec![partition];
        ge_key.extend_from_slice(query_bytes);

        let mut lt_key = ge_key.clone();
        // Increment last byte for exclusive upper bound
        if let Some(last) = lt_key.last_mut() {
            if *last < 0xFF {
                *last += 1;
            } else {
                // Edge case: last byte is 0xFF — truncate and increment previous
                lt_key.pop();
                while let Some(last) = lt_key.last_mut() {
                    if *last < 0xFF {
                        *last += 1;
                        break;
                    }
                    lt_key.pop();
                }
            }
        }

        use lucivy_fst::{IntoStreamer, Streamer};
        let mut stream = fst.range().ge(&ge_key).lt(&lt_key).into_stream();
        while let Some((_key, val)) = stream.next() {
            let parents = reader.decode_parents(val);
            for p in parents {
                results.push(FstCandidateV3::from_parent(&p));
            }
        }
    }

    results
}

// ─── falling_walk_v3 ──────────────────────────────────────────────────────

/// Falling walk v3: byte-by-byte FST walk.
///
/// Detects split points where the query prefix reaches a token boundary:
/// - Normal partitions (0x00/0x01): split at `own_len - sti` bytes consumed
/// - Stripped partition (0x02): split at `content_len - sti` bytes consumed
///
/// The FST key is longer than own_len (includes overlap), so the split point
/// is in the MIDDLE of the key. We detect it at the final node by checking
/// `prefix_len >= own_len - sti`.
///
/// Returns split candidates sorted by query_consumed descending.
/// Falling walk on chunk partitions (0x00 + 0x01).
///
/// Chunk-level splits. Markers are included (overlap_consumed >= 0).
/// The chain builder uses best_consumed filter to prevent intra-partition
/// mixing of different consumed values.
pub fn falling_walk_chunks(
    reader: &SfxFileReaderV3,
    query: &str,
) -> Vec<SplitCandidateV3> {
    let lower = query.to_lowercase();
    let query_bytes = lower.as_bytes();
    let map = reader.fst();
    let fst = map.as_fst();
    let mut candidates = Vec::new();

    for &partition in &[SI0_PREFIX, SI_REST_PREFIX] {
        walk_partition(
            fst, reader, query_bytes, partition,
            |parent, prefix_len| {
                if parent.sti >= parent.own_len {
                    return None;
                }
                let split_byte = parent.own_len as usize - parent.sti as usize;
                if prefix_len >= split_byte {
                    let overlap_consumed = prefix_len - split_byte;
                    Some(SplitCandidateV3 {
                        query_consumed: split_byte,
                        parent: parent.clone(),
                        remainder_start: split_byte,
                        overlap_validated: overlap_consumed,
                    })
                } else {
                    None
                }
            },
            &mut candidates,
        );
    }

    sort_and_dedup_splits(&mut candidates);
    candidates
}

/// Falling walk on word-stripped partition (0x02).
///
/// For strict_sep=false queries. Word-level splits with content_len boundary.
/// Markers are kept (overlap_consumed >= 0) because word-level keys are longer
/// and less prone to collision than chunk-level markers.
pub fn falling_walk_words(
    reader: &SfxFileReaderV3,
    query: &str,
) -> Vec<SplitCandidateV3> {
    let lower = query.to_lowercase();
    let query_bytes = lower.as_bytes();
    let map = reader.fst();
    let fst = map.as_fst();
    let mut candidates = Vec::new();

    walk_partition(
        fst, reader, query_bytes, SI_STRIPPED_PREFIX,
        |parent, prefix_len| {
            if parent.sep_len == 0 {
                return None;
            }
            let content_len = parent.content_len() as usize;
            let split_byte = content_len - parent.sti as usize;
            if split_byte == 0 {
                return None;
            }
            if prefix_len >= split_byte {
                let overlap_consumed = prefix_len - split_byte;
                Some(SplitCandidateV3 {
                    query_consumed: split_byte,
                    parent: parent.clone(),
                    remainder_start: split_byte,
                    overlap_validated: overlap_consumed,
                })
            } else {
                None
            }
        },
        &mut candidates,
    );

    sort_and_dedup_splits(&mut candidates);
    candidates
}

/// Combined falling walk (both partitions). Legacy API for callers that
/// don't need partition separation.
pub fn falling_walk_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
) -> Vec<SplitCandidateV3> {
    let mut candidates = falling_walk_chunks(reader, query);
    if !strict_separators {
        candidates.extend(falling_walk_words(reader, query));
        sort_and_dedup_splits(&mut candidates);
    }
    candidates
}

fn sort_and_dedup_splits(candidates: &mut Vec<SplitCandidateV3>) {
    candidates.sort_by(|a, b| {
        b.query_consumed.cmp(&a.query_consumed)
            .then(b.overlap_validated.cmp(&a.overlap_validated))
    });
    candidates.dedup_by(|a, b| {
        a.parent.raw_ordinal == b.parent.raw_ordinal
            && a.parent.sti == b.parent.sti
            && a.query_consumed == b.query_consumed
    });
}

/// Walk a single partition byte-by-byte, calling `check_split` at each final node.
///
/// When the query runs out on a non-final node, performs a look-ahead: continues
/// walking all FST transitions until reaching final nodes. This handles the case
/// where a word's overlap extends the FST key beyond the query length — e.g.,
/// query "uint64t" (7 bytes) against word key "uint64to" (8 bytes, overlap "to").
/// The split was already passed (query consumed > content_len), we just need to
/// reach the final node to decode the parent entries.
fn walk_partition<D: AsRef<[u8]>, F>(
    fst: &raw::Fst<D>,
    reader: &SfxFileReaderV3,
    query_bytes: &[u8],
    partition: u8,
    check_split: F,
    candidates: &mut Vec<SplitCandidateV3>,
) where
    F: Fn(&ParentEntryV3, usize) -> Option<SplitCandidateV3>,
{
    let root = fst.root();
    let Some(idx) = root.find_input(partition) else { return };
    let trans = root.transition(idx);
    let mut output = raw::Output::zero().cat(trans.out);
    let mut node = fst.node(trans.addr);

    let mut fully_consumed = false;
    for (i, &byte) in query_bytes.iter().enumerate() {
        let Some(idx) = node.find_input(byte) else { break };
        let trans = node.transition(idx);
        output = output.cat(trans.out);
        node = fst.node(trans.addr);

        if i + 1 == query_bytes.len() {
            fully_consumed = true;
        }

        if node.is_final() {
            let val = output.cat(node.final_output()).value();
            let prefix_len = i + 1;
            let parents = reader.decode_parents(val);

            for parent in &parents {
                if let Some(split) = check_split(parent, prefix_len) {
                    candidates.push(split);
                }
            }
        }
    }

    // Look-ahead: query fully consumed on a non-final node. The FST key
    // continues into the overlap. Walk all remaining transitions to reach
    // final nodes and decode their parent entries.
    // The check_split receives prefix_len = query_len (what the query actually
    // consumed), not the FST depth — the extra bytes are overlap, not query.
    if fully_consumed && !node.is_final() {
        overlap_lookahead(fst, reader, &node, output, query_bytes.len(),
            &check_split, candidates);
    }
}

/// DFS through remaining FST transitions after query exhaustion.
/// Walks until all reachable final nodes are found.
fn overlap_lookahead<D: AsRef<[u8]>, F>(
    fst: &raw::Fst<D>,
    reader: &SfxFileReaderV3,
    node: &raw::Node<'_>,
    output: raw::Output,
    query_len: usize,
    check_split: &F,
    candidates: &mut Vec<SplitCandidateV3>,
) where
    F: Fn(&ParentEntryV3, usize) -> Option<SplitCandidateV3>,
{
    for ti in 0..node.len() {
        let trans = node.transition(ti);
        let child_output = output.cat(trans.out);
        let child = fst.node(trans.addr);

        if child.is_final() {
            let val = child_output.cat(child.final_output()).value();
            let parents = reader.decode_parents(val);
            for parent in &parents {
                if let Some(split) = check_split(parent, query_len) {
                    candidates.push(split);
                }
            }
        }

        overlap_lookahead(fst, reader, &child, child_output, query_len,
            check_split, candidates);
    }
}

// ─── Chain builders ──────────────────────────────────────────────────────

const MAX_CHAIN_DEPTH: usize = 8;

/// Build a chain from a list of initial splits using a given falling_walk function.
///
/// `filter_best_consumed`: if true, only keep sub_split ordinals with the same
/// consumed as the best. Required for chunk pipeline (0x00/0x01) where marker
/// entries create multi-parent nodes at different positions → different consumed.
/// Not needed for word pipeline (0x02) where word-stripped entries have unique prefixes.
fn build_chains_from_splits(
    reader: &SfxFileReaderV3,
    splits: &[SplitCandidateV3],
    query: &str,
    walk_fn: fn(&SfxFileReaderV3, &str) -> Vec<SplitCandidateV3>,
    strict_sep_for_candidates: bool,
    filter_best_consumed: bool,
) -> Vec<TokenChainV3> {
    let mut chains = Vec::new();
    let query_lower = query.to_lowercase();

    for split in splits {
        let safe_start = snap_to_char_boundary(&query_lower, split.remainder_start);
        let remainder = &query_lower[safe_start..];
        if remainder.is_empty() {
            chains.push(TokenChainV3 {
                ordinals: vec![vec![split.parent.raw_ordinal]],
                first_sti: split.parent.sti,
                total_query_consumed: split.query_consumed,
            });
            continue;
        }

        let mut positions: Vec<Vec<u64>> = vec![vec![split.parent.raw_ordinal]];
        let mut rem = remainder.to_string();
        let mut depth = 0;

        while !rem.is_empty() && depth < MAX_CHAIN_DEPTH {
            let cands = fst_candidates_v3(reader, &rem, true, strict_sep_for_candidates);
            if !cands.is_empty() {
                let mut unique_ords: Vec<u64> = cands.iter().map(|c| c.raw_ordinal).collect();
                unique_ords.sort_unstable();
                unique_ords.dedup();
                positions.push(unique_ords);
                rem.clear();
                break;
            }

            let sub_splits = walk_fn(reader, &rem);
            if sub_splits.is_empty() {
                break;
            }
            let mut unique_ords: Vec<u64> = if filter_best_consumed {
                // Chunk pipeline: marker entries create multi-parent nodes at
                // different FST positions → different consumed values. Only keep
                // ordinals from the best consumed to avoid mixing chain positions.
                let best_consumed = sub_splits[0].query_consumed;
                sub_splits.iter()
                    .filter(|s| s.query_consumed == best_consumed)
                    .map(|s| s.parent.raw_ordinal)
                    .collect()
            } else {
                // Word pipeline: word-stripped entries have unique prefixes,
                // different consumed values are rare. Collect all.
                sub_splits.iter()
                    .map(|s| s.parent.raw_ordinal)
                    .collect()
            };
            unique_ords.sort_unstable();
            unique_ords.dedup();
            positions.push(unique_ords);

            let best = &sub_splits[0];
            let safe = snap_to_char_boundary(&rem, best.remainder_start);
            rem = rem[safe..].to_string();
            depth += 1;
        }

        if rem.is_empty() {
            chains.push(TokenChainV3 {
                ordinals: positions,
                first_sti: split.parent.sti,
                total_query_consumed: query.len(),
            });
        }
    }

    chains
}

/// Cross-chunk chains (partitions 0x00 + 0x01).
/// Uses overlap_consumed > 0 (no markers). Resolved with strict adjacency (pos+1).
pub fn cross_chunk_chain_v3(
    reader: &SfxFileReaderV3,
    query: &str,
) -> Vec<TokenChainV3> {
    let splits = falling_walk_chunks(reader, query);
    build_chains_from_splits(reader, &splits, query, falling_walk_chunks, true, true)
}

/// Cross-word chains (partition 0x02).
/// Word-level splits. Resolved with relaxed adjacency (posmap/bytemap required).
pub fn cross_word_chain_v3(
    reader: &SfxFileReaderV3,
    query: &str,
) -> Vec<TokenChainV3> {
    let splits = falling_walk_words(reader, query);
    build_chains_from_splits(reader, &splits, query, falling_walk_words, false, false)
}

/// Legacy combined API — builds chains from both partitions mixed.
pub fn cross_token_chain_v3(
    reader: &SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
) -> Vec<TokenChainV3> {
    let mut chains = cross_chunk_chain_v3(reader, query);
    if !strict_separators {
        chains.extend(cross_word_chain_v3(reader, query));
    }
    chains
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
    use crate::suffix_fst::file_v3::SfxFileWriterV3;

    /// Build reader from token specs: (text, ord, own_len, sep_len, overlap_len, is_word_start)
    fn with_reader<F>(specs: &[(&str, u64, u16, u8, u8, bool)], f: F)
    where
        F: FnOnce(&SfxFileReaderV3),
    {
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(1);
        for &(text, ord, own_len, sep_len, overlap_len, is_ws) in specs {
            builder.add_token(text, ord, own_len, sep_len, overlap_len, is_ws);
            if sep_len > 0 {
                let content_end = (own_len - sep_len as u16) as usize;
                let overlap_start = own_len as usize;
                builder.add_word_stripped(
                    &text[..content_end],
                    &text[overlap_start..],
                    ord, own_len, sep_len, is_ws,
                );
            }
        }
        let (fst_data, parent_data) = builder.build().unwrap();
        let writer = SfxFileWriterV3::new(fst_data, parent_data, 1);
        let sfx_bytes = writer.to_bytes();
        let reader = SfxFileReaderV3::open(&sfx_bytes).unwrap();
        f(&reader);
    }

    // ── fst_candidates_v3 ──

    #[test]
    fn test_candidates_exact_key() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            let c = fst_candidates_v3(r, "mutex_lo", false, true);
            assert!(!c.is_empty());
            assert!(c.iter().any(|c| c.sti == 0));
        });
    }

    #[test]
    fn test_candidates_substring() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            let c = fst_candidates_v3(r, "tex_lo", false, true);
            assert!(!c.is_empty());
            assert!(c.iter().any(|c| c.sti == 2));
        });
    }

    #[test]
    fn test_candidates_stripped() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            // strict=true: "exlo" not found
            assert!(fst_candidates_v3(r, "exlo", false, true).is_empty());
            // strict=false: "exlo" found in stripped partition
            let c = fst_candidates_v3(r, "exlo", false, false);
            assert!(!c.is_empty());
            assert!(c.iter().any(|c| c.sti == 3));
        });
    }

    #[test]
    fn test_candidates_anchor_start() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            let c = fst_candidates_v3(r, "mutex_lo", true, true);
            assert!(c.iter().all(|c| c.sti == 0));
            // Substring not found with anchor
            assert!(fst_candidates_v3(r, "tex_lo", true, true).is_empty());
        });
    }

    // ── falling_walk_v3 ──

    #[test]
    fn test_walk_no_split_short_query() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            // "tex" (3 bytes) doesn't reach own_len(6) → no split
            let s = falling_walk_v3(r, "tex", true);
            assert!(s.is_empty());
        });
    }

    #[test]
    fn test_walk_split_at_own_len() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
            ("lock", 1, 4, 0, 0, true),
        ], |r| {
            // "mutex_lock": walk "mutex_lo" → final at 8 bytes
            // split_byte = 6 - 0 = 6, prefix_len=8 >= 6 → split
            // overlap_validated = 8 - 6 = 2
            let s = falling_walk_v3(r, "mutex_lock", true);
            assert!(!s.is_empty(), "should find split");
            let split = &s[0];
            assert_eq!(split.query_consumed, 6);
            assert_eq!(split.remainder_start, 6);
            assert_eq!(split.overlap_validated, 2);
            assert_eq!(split.parent.own_len, 6);
        });
    }

    #[test]
    fn test_walk_stripped_sep_skip() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            // "mutexlo" strict_sep=false → stripped partition "mutexlo" matches
            // split_byte = content_len(5) - sti(0) = 5
            // prefix_len = 7 (full key), 7 >= 5 → split
            let s = falling_walk_v3(r, "mutexlo", false);
            assert!(!s.is_empty(), "stripped walk should find split for 'mutexlo'");
            let split = s.iter().find(|s| s.query_consumed == 5).unwrap();
            assert_eq!(split.overlap_validated, 2); // "lo" validated
        });
    }

    #[test]
    fn test_walk_strict_rejects_wrong_sep() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            // "mutex lo" (space) strict=true → walk breaks at '_' vs ' '
            let s = falling_walk_v3(r, "mutex lo", true);
            // Should not find a split with query_consumed >= 6
            assert!(
                s.iter().all(|s| s.query_consumed < 6),
                "strict should reject wrong separator"
            );
        });
    }

    // ── cross_token_chain_v3 ──

    #[test]
    fn test_chain_two_tokens() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
            ("lock", 1, 4, 0, 0, true),
        ], |r| {
            let chains = cross_token_chain_v3(r, "mutex_lock", true);
            assert!(!chains.is_empty(), "should find cross-token chain");
            let c = &chains[0];
            assert_eq!(c.ordinals.len(), 2);
            assert_eq!(c.total_query_consumed, 10);
        });
    }

    #[test]
    fn test_chain_sep_skip() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
            ("lock", 1, 4, 0, 0, true),
        ], |r| {
            // "mutexlock" strict_sep=false
            let chains = cross_token_chain_v3(r, "mutexlock", false);
            assert!(!chains.is_empty(), "sep-skip chain should work");
            let c = &chains[0];
            assert_eq!(c.ordinals.len(), 2);
        });
    }

    #[test]
    fn test_chain_three_tokens() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
            ("lock_in", 1, 5, 1, 2, true),
            ("init", 2, 4, 0, 0, true),
        ], |r| {
            let chains = cross_token_chain_v3(r, "mutex_lock_init", true);
            assert!(!chains.is_empty(), "should find 3-token chain");
            let c = &chains[0];
            assert_eq!(c.ordinals.len(), 3);
        });
    }

    #[test]
    fn test_overlap_trigram_findable() {
        with_reader(&[
            ("mutex_lo", 0, 6, 1, 2, true),
        ], |r| {
            // "x_lo" is a suffix at STI=4, exact key match
            let c = fst_candidates_v3(r, "x_lo", false, true);
            assert!(!c.is_empty());
            assert!(c.iter().any(|c| c.sti == 4));
        });
    }
}
