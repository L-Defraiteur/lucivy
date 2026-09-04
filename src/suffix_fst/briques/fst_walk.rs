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
pub(crate) fn snap_to_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}
use crate::suffix_fst::builder_v3::{
    ParentEntryV3, MAX_OVERLAP_BYTES, SI_STRIPPED_PREFIX,
};
use crate::suffix_fst::file_v3::SfxFileReaderV3;

// ─── Types ─────────────────────────────────────────────────────────────────

/// A candidate from a direct FST lookup.
#[derive(Debug, Clone)]
pub struct FstCandidateV3 {
    /// Token ordinal, the key into the posting lists.
    pub raw_ordinal: u64,
    /// Suffix start index: byte offset of the matched suffix within the token.
    pub sti: u16,
    /// Byte length of the token itself (content + trailing separators, overlap excluded).
    pub own_len: u16,
    /// Number of trailing separator bytes included in `own_len`.
    pub sep_len: u8,
    /// Bytes of the next token following `own_len` — in the key up to
    /// container version 6, in the record (`overlap`) since version 7.
    pub overlap_len: u8,
    /// Those bytes when the record carries them (see `ParentEntryV3::overlap`).
    pub overlap: [u8; MAX_OVERLAP_BYTES],
    /// True if this token is the first chunk of its word.
    pub is_word_start: bool,
    /// Which FST partition this candidate was found in.
    /// 0x00 = SI0 (token start), 0x01 = SI>0 (suffix), 0x02 = word-stripped.
    pub partition: u8,
}

impl FstCandidateV3 {
    /// Content byte length: `own_len` minus the trailing separators.
    pub fn content_len(&self) -> u16 {
        self.own_len - self.sep_len as u16
    }

    /// True if this candidate is from the word-stripped partition (0x02).
    pub fn is_word_stripped(&self) -> bool {
        self.partition == SI_STRIPPED_PREFIX
    }

    fn from_parent(p: &ParentEntryV3, partition: u8) -> Self {
        Self {
            raw_ordinal: p.raw_ordinal,
            sti: p.sti,
            own_len: p.own_len,
            sep_len: p.sep_len,
            overlap_len: p.overlap_len,
            overlap: p.overlap,
            is_word_start: p.is_word_start,
            partition,
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
    ///
    /// Shared, not owned: every chain built from the same query remainder
    /// carries the same alternatives list at that position, and a query such as
    /// `__init` produces 3.4 million chains over 50k documents. Cloning the list
    /// per chain was the bulk of `build_chains_from_splits`.
    pub ordinals: Vec<std::sync::Arc<Vec<u64>>>,
    /// Suffix start index of the first token: 0 when the match begins at a token start.
    pub first_sti: u16,
    /// Query bytes consumed by the whole chain.
    pub total_query_consumed: usize,
    /// Query bytes consumed by the LAST position of the chain.
    ///
    /// Needed to compute a match end that measures the match itself rather than
    /// the end of the containing token. Without it, `byte_to` falls back to the
    /// last token's own end — separator included — which makes the span lie.
    pub last_consumed: usize,
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
        while let Some((key, val)) = stream.next() {
            let parents = reader.decode_parents(val, key);
            for p in parents {
                results.push(FstCandidateV3::from_parent(&p, partition));
            }
        }

        // Keys cut at the token boundary (container version 7): a query whose
        // tail lies in the next token is not a prefix of any key any more. It
        // used to be found because the key ran `overlap_len` bytes past the
        // boundary; those bytes are in the record now, so probe the keys that
        // are proper prefixes of the query, up to `MAX_OVERLAP_BYTES` shorter,
        // and keep the parents whose overlap completes it.
        if reader.keys_cut_at_boundary() {
            let len = query_bytes.len();
            let shortest = len.saturating_sub(MAX_OVERLAP_BYTES).max(1);
            for k in shortest..len {
                let tail = &query_bytes[k..];
                let mut probe = vec![partition];
                probe.extend_from_slice(&query_bytes[..k]);
                let Some(val) = fst.get(&probe) else { continue };
                let parents = reader.decode_parents_where(val, &probe, |ov| ov.len() >= tail.len() && ov[..tail.len()] == *tail);
                for p in parents {
                    results.push(FstCandidateV3::from_parent(&p, partition));
                }
            }
        }
    }

    results
}

// ─── falling_walk_v3 ──────────────────────────────────────────────────────

/// Split at a final node of a key cut at the token boundary (container
/// version 7). `prefix_len` is the node's depth and must be the boundary
/// itself; the parent's overlap bytes must match the query past it, and the
/// query must continue beyond them — the conditions under which the walk over
/// the older, longer keys reached the full key and accepted the split.
#[inline]
fn split_at_boundary(
    parent: &ParentEntryV3,
    prefix_len: usize,
    split_byte: usize,
    query_bytes: &[u8],
) -> Option<SplitCandidateV3> {
    if prefix_len != split_byte {
        return None;
    }
    let need = (parent.overlap_len as usize).min(MAX_OVERLAP_BYTES);
    let remainder = &query_bytes[split_byte..];
    if remainder.len() <= need || remainder[..need] != parent.overlap[..need] {
        return None;
    }
    Some(SplitCandidateV3 {
        query_consumed: split_byte,
        parent: parent.clone(),
        remainder_start: split_byte,
        overlap_validated: need,
    })
}

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
    let cut = reader.keys_cut_at_boundary();

    for &partition in &[SI0_PREFIX, SI_REST_PREFIX] {
        walk_partition(
            fst, reader, query_bytes, partition,
            |parent, prefix_len| {
                if parent.sti >= parent.own_len {
                    return None;
                }
                // The whole query sits inside this key (own bytes + overlap):
                // that is a single-token match, already found by
                // fst_candidates_v3. A chain from here only re-derives it —
                // 26 438 chains for `inc` over rag3db, every one redundant,
                // which is what made a 3-byte literal cost 120 ms of CPU.
                if prefix_len >= query_bytes.len() {
                    return None;
                }
                let split_byte = parent.own_len as usize - parent.sti as usize;
                if cut {
                    // The key ends at the boundary, so the final node IS the
                    // boundary; the record says which bytes follow. They must
                    // agree with the query, and the query must go on past
                    // them — exactly when the walk over the longer keys used
                    // to reach the full key (see below).
                    return split_at_boundary(parent, prefix_len, split_byte, query_bytes);
                }
                if prefix_len >= split_byte {
                    let overlap_consumed = prefix_len - split_byte;
                    // The key carries the next token's first bytes. When the
                    // query goes on past this token and those bytes are
                    // there, they must agree: a split of `TARGET>\n` for
                    // `target*>` contradicts itself at `*`. Kept, it outranked
                    // the real `TARGE|T*` split (6 bytes consumed against 5)
                    // and `build_chains_from_splits` keeps only the best
                    // consumed — `<const TARGET*>` then found nothing in 17
                    // rag3db files (coherence panel, 23 August).
                    let available = (parent.overlap_len as usize)
                        .min(query_bytes.len() - split_byte);
                    if overlap_consumed < available {
                        return None;
                    }
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

    let cut = reader.keys_cut_at_boundary();
    walk_partition(
        fst, reader, query_bytes, SI_STRIPPED_PREFIX,
        |parent, prefix_len| {
            if parent.sep_len == 0 {
                return None;
            }
            if prefix_len >= query_bytes.len() {
                return None;
            }
            let content_len = parent.content_len() as usize;
            let split_byte = content_len - parent.sti as usize;
            if split_byte == 0 {
                return None;
            }
            if cut {
                return split_at_boundary(parent, prefix_len, split_byte, query_bytes);
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

/// Sort splits by `query_consumed` then `overlap_validated`, both descending,
/// and drop duplicates sharing the same (ordinal, sti, query_consumed).
pub fn sort_and_dedup_splits(candidates: &mut Vec<SplitCandidateV3>) {
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
    // Keys cut at the boundary (version 7): at a final node of depth `d`
    // only the parents whose overlap is what the query says next can split
    // (`split_at_boundary`); the record is grouped by overlap so the other
    // groups are skipped unread. Older files return every parent here.
    let overlap_agrees = |ov: &[u8], d: usize| -> bool {
        query_bytes.len() > d + ov.len() && query_bytes[d..d + ov.len()] == *ov
    };
    let root = fst.root();
    let Some(idx) = root.find_input(partition) else { return };
    let trans = root.transition(idx);
    let mut output = raw::Output::zero().cat(trans.out);
    let mut node = fst.node(trans.addr);
    // The key under a final node of depth `d` is the partition byte and the
    // first `d` query bytes; the record decoder wants it (version 8).
    let mut query_key = Vec::with_capacity(1 + query_bytes.len());
    query_key.push(partition);
    query_key.extend_from_slice(query_bytes);

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
            let parents = reader.decode_parents_where(val, &query_key[..prefix_len + 1], |ov| overlap_agrees(ov, prefix_len));

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
    if fully_consumed && !node.is_final() && !reader.keys_cut_at_boundary() {
        overlap_lookahead(fst, reader, &node, output, query_bytes.len(),
            &check_split, candidates);
    }
}

/// DFS through remaining FST transitions after query exhaustion.
/// Walks until all reachable final nodes are found.
///
/// Iterative, with an explicit stack: the depth is the length of the longest
/// key beyond the query, and one 3 400-byte separator-free "word" in a corpus
/// overflowed a 2 MB thread stack recursing once per byte. Children are
/// pushed in reverse so the visiting order is the recursive one.
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
    let mut stack: Vec<(raw::CompiledAddr, raw::Output)> = Vec::new();
    for ti in (0..node.len()).rev() {
        let trans = node.transition(ti);
        stack.push((trans.addr, output.cat(trans.out)));
    }
    while let Some((addr, child_output)) = stack.pop() {
        let child = fst.node(addr);
        if child.is_final() {
            let val = child_output.cat(child.final_output()).value();
            // Files up to version 6 only (the walk skips this for cut keys),
            // and those ignore the key.
            let parents = reader.decode_parents(val, &[]);
            for parent in &parents {
                if let Some(split) = check_split(parent, query_len) {
                    candidates.push(split);
                }
            }
        }
        for ti in (0..child.len()).rev() {
            let trans = child.transition(ti);
            stack.push((trans.addr, child_output.cat(trans.out)));
        }
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

    // Every remainder this loop ever walks is a suffix of `query_lower`: the
    // first one is `query_lower[safe_start..]`, and each step only trims from
    // the front. A suffix is identified uniquely by where it starts, so the byte
    // offset is a complete cache key — and there are at most `query_lower.len()`
    // of them, however many splits come in.
    //
    // Without this, each of the tens of thousands of splits re-walked the FST
    // over the same handful of suffixes: measured at 15x redundancy on
    // `kmalloc`, 25x on `uint64_t`, 78x on `include`, for a stage that was
    // 78-96% of query time.
    let mut fst_memo: FnvHashMap<usize, std::sync::Arc<Vec<u64>>> = FnvHashMap::default();
    // Per remainder offset: the next-token alternatives, grouped by how much
    // of the remainder each group consumes (`ordinals`, consumed, where the
    // rest starts). One chain position holds ordinals that all consume the
    // same bytes, so groups with different consumed are different BRANCHES,
    // not alternatives — they used to be cut down to the first group ("best
    // consumed"), which is the longest token seen, not the one in the
    // document: `expression>` over `Expressi|on` (8) and `Expres|si` (6),
    // where the file is chunked 6+6, found nothing in 61 rag3db files.
    type Group = (std::sync::Arc<Vec<u64>>, usize, usize);
    let mut walk_memo: FnvHashMap<usize, std::sync::Arc<Vec<Group>>> = FnvHashMap::default();

    super::profile::bump(|c| &c.n_bcfs_splits, splits.len() as u64);

    for split in splits {
        let safe_start = snap_to_char_boundary(&query_lower, split.remainder_start);
        if safe_start >= query_lower.len() {
            chains.push(TokenChainV3 {
                ordinals: vec![std::sync::Arc::new(vec![split.parent.raw_ordinal])],
                first_sti: split.parent.sti,
                total_query_consumed: split.query_consumed,
                last_consumed: split.query_consumed,
            });
            continue;
        }

        // Depth-first over the branches; `stack` holds (positions so far,
        // remainder offset, depth, consumed by the last position).
        let head: Vec<std::sync::Arc<Vec<u64>>> =
            vec![std::sync::Arc::new(vec![split.parent.raw_ordinal])];
        let mut stack: Vec<(Vec<std::sync::Arc<Vec<u64>>>, usize, usize, usize)> =
            vec![(head, safe_start, 0, split.query_consumed)];

        while let Some((positions, rem_off, depth, last_consumed)) = stack.pop() {
            if rem_off >= query_lower.len() {
                chains.push(TokenChainV3 {
                    ordinals: positions,
                    first_sti: split.parent.sti,
                    total_query_consumed: query.len(),
                    last_consumed,
                });
                continue;
            }
            if depth >= MAX_CHAIN_DEPTH { continue; }
            let rem = &query_lower[rem_off..];

            super::profile::bump(|c| &c.n_bcfs_fst_reqs, 1);
            if let std::collections::hash_map::Entry::Vacant(slot) = fst_memo.entry(rem_off) {
                super::profile::bump(|c| &c.n_bcfs_fst_calls, 1);
                let cands = fst_candidates_v3(reader, rem, true, strict_sep_for_candidates);
                let mut unique_ords: Vec<u64> =
                    cands.iter().map(|c| c.raw_ordinal).collect();
                unique_ords.sort_unstable();
                unique_ords.dedup();
                slot.insert(std::sync::Arc::new(unique_ords));
            }
            let hit = &fst_memo[&rem_off];
            if !hit.is_empty() {
                // This position swallows the whole remainder — one branch,
                // not the only one. The same text is chunked differently
                // from one document to the next, so a key holding all of
                // `expression` (doc A: `Expressi`+`on`) must not stop the
                // walk for the split shape (doc B: `Expres|si` then `sion>`).
                // Stopping here lost `<binder::Expression` in every document
                // sharing a segment with doc A (pipeline test, 24 August).
                let mut swallowed = positions.clone();
                swallowed.push(std::sync::Arc::clone(hit));
                chains.push(TokenChainV3 {
                    ordinals: swallowed,
                    first_sti: split.parent.sti,
                    total_query_consumed: query.len(),
                    last_consumed: rem.len(),
                });
            }

            super::profile::bump(|c| &c.n_bcfs_walk_reqs, 1);
            if let std::collections::hash_map::Entry::Vacant(slot) = walk_memo.entry(rem_off) {
                super::profile::bump(|c| &c.n_bcfs_walk_calls, 1);
                let mut sub_splits = walk_fn(reader, rem);
                // Past the head, the query continues at the START of the next
                // token: the text is contiguous, and a token before the last
                // consumes exactly its own content. A split entering the next
                // token at sti > 0 skips that token's leading bytes — the chain
                // `[mv_ @2, vapor__ @7, init]` matched `_`+`_`+`init` with
                // `vapor _` silently in between, and reported the span from the
                // first `_`. verify_literal kept the document honest (a real
                // occurrence sat in the window); the span was wrong in 8% of
                // `__init` highlights on the kernel.
                sub_splits.retain(|s| s.parent.sti == 0);
                let mut groups: Vec<Group> = Vec::new();
                if filter_best_consumed {
                    // Chunk pipeline: one group per distinct consumed, each a
                    // branch of its own (sub_splits is sorted by consumed).
                    let mut i = 0;
                    while i < sub_splits.len() {
                        let consumed = sub_splits[i].query_consumed;
                        let rem_start = sub_splits[i].remainder_start;
                        let mut ords: Vec<u64> = Vec::new();
                        while i < sub_splits.len() && sub_splits[i].query_consumed == consumed {
                            ords.push(sub_splits[i].parent.raw_ordinal);
                            i += 1;
                        }
                        ords.sort_unstable();
                        ords.dedup();
                        groups.push((std::sync::Arc::new(ords), consumed, rem_start));
                    }
                } else if let Some(best) = sub_splits.first() {
                    // Word pipeline: word-stripped entries have unique prefixes,
                    // different consumed values are rare. Collect all.
                    let mut ords: Vec<u64> = sub_splits.iter().map(|s| s.parent.raw_ordinal).collect();
                    ords.sort_unstable();
                    ords.dedup();
                    groups.push((std::sync::Arc::new(ords), best.query_consumed, best.remainder_start));
                }
                slot.insert(std::sync::Arc::new(groups));
            }
            let groups = std::sync::Arc::clone(&walk_memo[&rem_off]);
            for (ords, consumed, rem_start) in groups.iter() {
                let mut positions = positions.clone();
                positions.push(std::sync::Arc::clone(ords));
                let next = rem_off + snap_to_char_boundary(rem, *rem_start);
                stack.push((positions, next, depth + 1, *consumed));
            }
        }
    }

    super::profile::bump(|c| &c.n_bcfs_distinct_rem, fst_memo.len() as u64);

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

/// Chunk chains from caller-chosen head splits (see `cross_chunk_chain_v3`).
///
/// Lets `find_literal_v3` keep only long heads on the forward path and anchor
/// short-head occurrences on their second token instead.
pub fn cross_chunk_chain_from_splits(
    reader: &SfxFileReaderV3,
    splits: &[SplitCandidateV3],
    query: &str,
) -> Vec<TokenChainV3> {
    build_chains_from_splits(reader, splits, query, falling_walk_chunks, true, true)
}

/// Cross-word chains (partition 0x02).
/// Word-level splits. Resolved with relaxed adjacency (posmap/termtexts required).
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

// ─── Sibling-based chain building ─────────────────────────────────────────

use fnv::FnvHashMap;
use crate::suffix_fst::sibling_table::SiblingTableReader;
use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;

/// Extract additional splits from fst_candidates results.
///
/// For each candidate where the query extends past the content boundary,
/// it's a split that the falling walk may have missed (FST node not final).
/// This catches the "uint64t" case: fst_candidates finds "uint64to",
/// content_len=6, query_len=7 > 6 → split at byte 6.
pub fn splits_from_fst_candidates(
    candidates: &[FstCandidateV3],
    query_len: usize,
) -> Vec<SplitCandidateV3> {
    let mut splits = Vec::new();
    for cand in candidates {
        let content_len = cand.content_len() as usize;
        let split_byte = content_len.saturating_sub(cand.sti as usize);
        if split_byte == 0 || split_byte >= query_len {
            continue;
        }
        // Query extends past content boundary → split
        splits.push(SplitCandidateV3 {
            query_consumed: split_byte,
            parent: ParentEntryV3 {
                raw_ordinal: cand.raw_ordinal,
                sti: cand.sti,
                own_len: cand.own_len,
                sep_len: cand.sep_len,
                overlap_len: cand.overlap_len,
                overlap: cand.overlap,
                is_word_start: cand.is_word_start,
            },
            remainder_start: split_byte,
            overlap_validated: query_len - split_byte,
        });
    }
    splits
}

/// Build chains using the sibling table instead of re-walking the FST.
///
/// Algorithm (same as v2 suffix_contains.rs:880-918):
/// 1. Start from initial splits (falling walk + fst_candidates)
/// 2. DFS: follow sibling links, compare remainder with sibling content text
/// 3. Terminal if sibling content covers the remainder (prefix match)
/// 4. Partial if remainder starts with sibling content → continue chain
pub fn sibling_chain_dfs(
    splits: &[SplitCandidateV3],
    query: &str,
    sibling_table: &SiblingTableReader<'_>,
    termtexts: &TermTextsReaderV3<'_>,
    strict_separators: bool,
    trace_id: Option<u64>,
) -> Vec<TokenChainV3> {
    let query_lower = query.to_lowercase();
    let mut chains = Vec::new();

    for split in splits {
        let safe_start = snap_to_char_boundary(&query_lower, split.remainder_start);
        let remainder = &query_lower[safe_start..];

        if let Some(tid) = trace_id {
            let split_text = termtexts.text(split.parent.raw_ordinal as u32)
                .unwrap_or("?");
            super::trace::trace_event(tid, "split", &[
                ("ord", &split.parent.raw_ordinal),
                ("sti", &split.parent.sti),
                ("consumed", &split.query_consumed),
                ("text", &split_text),
                ("remainder", &remainder),
            ]);
        }

        if remainder.is_empty() {
            chains.push(TokenChainV3 {
                ordinals: vec![std::sync::Arc::new(vec![split.parent.raw_ordinal])],
                first_sti: split.parent.sti,
                total_query_consumed: split.query_consumed,
                last_consumed: split.query_consumed,
            });
            continue;
        }

        let mut stack: Vec<(u64, &str, Vec<std::sync::Arc<Vec<u64>>>, usize)> = vec![
            (split.parent.raw_ordinal, remainder,
             vec![std::sync::Arc::new(vec![split.parent.raw_ordinal])], 0)
        ];

        while let Some((cur_ord, rem, chain, depth)) = stack.pop() {
            if depth >= MAX_CHAIN_DEPTH { continue; }

            let siblings = sibling_table.siblings(cur_ord as u32);
            if let Some(tid) = trace_id {
                super::trace::trace_event(tid, "dfs_step", &[
                    ("ord", &cur_ord),
                    ("rem", &rem),
                    ("depth", &depth),
                    ("num_siblings", &siblings.len()),
                ]);
            }

            for sib in &siblings {
                let next_ord = sib.next_ordinal;
                let next_text = match termtexts.text(next_ord) {
                    Some(t) => t,
                    None => continue,
                };
                let next_lower = next_text.to_lowercase();

                // How much of the next token the query must cover to step onto it.
                //
                // Relaxed: the destination's CONTENT length, separator excluded —
                // which means the query is allowed to skip over the separator. That
                // contract used to be applied in strict mode
                // too: a strict search for "TableFunction" then matched
                // "migra|table function| configuration", because the step from
                // "table" to "function" jumped the space. Under strict separators the
                // query has to cover content AND separator, i.e. own_len.
                //
                // Both come from `.termtexts` META. Tables written before `SIB3`
                // carried the content length in `gap_len`; it is the fallback for a
                // file without META, and equal to META's value when both exist.
                let step_len = match termtexts.meta(next_ord) {
                    Some(m) if strict_separators => m.own_len as usize,
                    Some(m) => m.own_len.saturating_sub(m.sep_len as u16) as usize,
                    None => sib.gap_len as usize,
                };
                let next_content = if step_len > 0 && step_len < next_lower.len() {
                    let cl = snap_to_char_boundary(&next_lower, step_len);
                    &next_lower[..cl]
                } else {
                    &next_lower
                };

                if rem == next_content || next_content.starts_with(rem) {
                    if let Some(tid) = trace_id {
                        super::trace::trace_event(tid, "TERMINAL", &[
                            ("ord", &next_ord),
                            ("content", &next_content),
                            ("rem", &rem),
                        ]);
                    }
                    let mut c = chain.clone();
                    c.push(std::sync::Arc::new(vec![next_ord as u64]));
                    chains.push(TokenChainV3 {
                        ordinals: c,
                        first_sti: split.parent.sti,
                        total_query_consumed: query_lower.len(),
                        last_consumed: rem.len(),
                    });
                } else if let Some(new_rem) = rem.strip_prefix(next_content) {
                    if let Some(tid) = trace_id {
                        super::trace::trace_event(tid, "PARTIAL", &[
                            ("ord", &next_ord),
                            ("content", &next_content),
                            ("consumed", &next_content.len()),
                            ("new_rem", &new_rem),
                        ]);
                    }
                    let mut c = chain.clone();
                    c.push(std::sync::Arc::new(vec![next_ord as u64]));
                    stack.push((next_ord as u64, new_rem, c, depth + 1));
                }
            }
        }
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
        let writer = SfxFileWriterV3::new(fst_data, parent_data);
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
            // "mutexlo" strict_sep=false → stripped partition key "mutexlo"
            // holds the WHOLE query (content "mutex" + overlap "lo"): that is
            // a single-token match, and the walk emits no split for it — a
            // chain from here would only re-derive what fst_candidates_v3
            // already found. A query running past the key does split.
            let s = falling_walk_v3(r, "mutexlo", false);
            assert!(s.is_empty(), "whole query inside one key: no split, it is a single match");
            let s = falling_walk_v3(r, "mutexlock", false);
            let split = s.iter().find(|s| s.query_consumed == 5).expect("split for the longer query");
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
