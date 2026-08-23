//! Tier 2 — Posting resolution for SFX v3.
//!
//! Converts FST results (ordinals/candidates) into document matches
//! with adjacency verification for cross-token chains.
//!
//! - `resolve_single_v3`: single-token candidates → doc matches
//! - `resolve_chains_v3`: cross-token chains → doc matches with position adjacency
//! - `selectivity_v3`: estimate selectivity without resolving postings

use std::collections::HashSet;
use fnv::FnvHashMap;

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
    /// Number of TOKEN POSITIONS covered by the match, inclusive.
    ///
    /// Always `last_position - position + 1`. It used to be the chunk count on one
    /// path and the chain-position count (i.e. words) on another, which made any
    /// consumer walking `position..position+span` walk the wrong range.
    pub span: u32,
    /// Start byte offset of the MATCH in the original text.
    pub byte_from: u32,
    /// Bytes of the match that lie in the NEXT content token, when the match was
    /// found through a word-stripped key's content overlap (partition 0x02).
    ///
    /// A 0x02 key is `word + first two content bytes of the next word`, and those
    /// two bytes may sit after separators in the text. `byte_to` then stops at
    /// the word's own content end and the orchestrator extends it to
    /// `next_word.byte_from + overlap_overflow`, using posmap. Chunk keys need
    /// none of this: their overlap is the next chunk's first bytes, contiguous.
    pub overlap_overflow: u8,
    /// End byte offset (exclusive) of the MATCH in the original text.
    ///
    /// This measures the match itself. It used to mean "end of the containing
    /// token" — four different variants of it depending on the resolution path,
    /// separator sometimes included — which made every span comparison unreliable.
    /// For the container end, use `token_end`.
    pub byte_to: u32,
    /// End byte offset (exclusive) of the CONTAINING token, separator excluded.
    ///
    /// For a chain, the end of its last token. Only the `exact_match` filter reads
    /// this: `token_end - byte_from == query_content_len` is what makes a `term`
    /// query a whole-token match instead of a substring one. Keeping it in its own
    /// field means a change to `byte_to` can no longer silently turn `term` into
    /// `contains`.
    pub token_end: u32,
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
///
/// `query_len` is the byte length of the query as matched against the FST keys
/// (already separator-stripped in relaxed mode). It is what makes `byte_to`
/// measure the match rather than the token.
pub fn resolve_single_v3(
    candidates: &[FstCandidateV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    query_len: u32,
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

        // Content end of the chunk: keys in 0x00/0x01 are contiguous raw text
        // (content + sep + overlap), so this is a real offset in the source.
        let content_end = |bf: u32| bf + cand.own_len as u32 - cand.sep_len as u32;

        for e in &entries {
            let byte_from = e.byte_from + cand.sti as u32;
            results.push(MatchV3 {
                doc_id: e.doc_id,
                position: e.position,
                span: 1,
                byte_from,
                overlap_overflow: 0,
                byte_to: byte_from + query_len,
                token_end: content_end(e.byte_from),
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
///
/// `query_len` is the byte length of the query as matched against the FST keys.
pub fn resolve_single_word_v3(
    candidates: &[FstCandidateV3],
    word_sfxpost: &crate::suffix_fst::word_sfxpost::WordSfxPostReader<'_>,
    filter_docs: Option<&HashSet<DocId>>,
    query_len: u32,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    for cand in candidates {
        if !cand.is_word_stripped() { continue; }

        let entries = word_sfxpost.entries(cand.raw_ordinal as u32);
        for e in &entries {
            if let Some(filter) = filter_docs {
                if !filter.contains(&e.doc_id) { continue; }
            }
            // `word_content` is a contiguous run of source bytes, so
            // byte_from + sti is a real source offset. A 0x02 key does not fix
            // where its word ends: "0ui" is "0"+"ui" in one document and
            // "0u"+"i" in another, under one ordinal — and the FST entry's
            // metadata belongs to whichever occurrence was interned first, which
            // a merge reorders. Only the posting knows this word's content end
            // (WSP2). A match starting at or past it lives in the content
            // overlap — the next word's bytes — and the next word reports it
            // itself at sti 0.
            let content_end = e.byte_to;
            if cand.sti as u32 >= (e.byte_to - e.byte_from) { continue; }
            let byte_from = e.byte_from + cand.sti as u32;
            results.push(MatchV3 {
                doc_id: e.doc_id,
                position: e.first_position,
                span: if e.last_position > e.first_position {
                    e.last_position - e.first_position + 1
                } else { 1 },
                byte_from,
                // Clamped: past the word's content the match would cross a
                // separator whose source offset this entry does not know. The
                // excess is reported and placed by the orchestrator.
                overlap_overflow: (byte_from + query_len).saturating_sub(content_end) as u8,
                byte_to: (byte_from + query_len).min(content_end),
                token_end: content_end,
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

/// Strict adjacency resolved through posmap — see `AdjacencyMode::StrictPosmap`.
///
/// Same results as `resolve_chains_v3`; what changes is the work. On `__init`
/// over 5 000 kernel files the posting path materialised 10.2 million entries
/// and ran 25.3 million pair iterations for 15 hits.
pub fn resolve_chains_v3_posmap(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
) -> Vec<MatchV3> {
    resolve_chains_impl(chains, resolver, filter_docs, AdjacencyMode::StrictPosmap { posmap })
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
    termtexts: Option<&crate::suffix_fst::termtexts_v3::TermTextsReaderV3<'_>>,
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

        // The head must start inside this posting's own word. A 0x02 key does
        // not fix where its word ends ("0ui" is "0"+"ui" or "0u"+"i" under one
        // ordinal); the posting's byte_to is the content end (WSP2).
        // Checked here, not inside the memoised resolution: the memo is keyed
        // by ordinal list alone and is shared across chains with different
        // first_sti.
        let head_ok = |e: &WordPostingEntry| {
            (e.byte_from + chain.first_sti as u32) < e.byte_to
        };

        if chain.ordinals.len() == 1 {
            for e in first_entries.iter().filter(|e| head_ok(e)) {
                let bf = e.byte_from + chain.first_sti as u32;
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.first_position,
                    span: if e.last_position > e.first_position {
                        e.last_position - e.first_position + 1
                    } else { 1 },
                    byte_from: bf,
                    overlap_overflow: 0,
                    byte_to: bf + chain.total_query_consumed as u32,
                    token_end: bf + chain.total_query_consumed as u32,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: chain.ordinals[0][0],
                });
            }
            continue;
        }

        // Multi-position chain
        // Active: (doc_id, prev_last_position, byte_from_first, token_end_last,
        //          byte_from_last, last_ord)
        // byte_from_last is what lets the emitted byte_to measure the match instead
        // of running to the end of the tail token.
        // Tuple: (doc, prev_last_pos, byte_from_first, token_end, byte_from_last, last_ord, first_pos)
        // first_pos is the position of THIS match's first word. It used to be
        // recovered at emit time as the first entry of the doc — the same value
        // for every match in the doc, which the (doc, position) dedup then
        // collapsed to one occurrence per document.
        let mut active: Vec<(DocId, u32, u32, u32, u32, u64, u32)> = first_entries.iter()
            .filter(|e| head_ok(e))
            .map(|e| (e.doc_id, e.last_position, e.byte_from + chain.first_sti as u32,
                      e.byte_to, e.byte_from, 0u64, e.first_position))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() { break; }

            // Resolve from word sfxpost first, fall back to chunk resolver
            // Only active documents can extend a chain — see the note in
            // resolve_chains_impl.
            let active_docs: HashSet<DocId> =
                active.iter().map(|&(doc_id, ..)| doc_id).collect();
            let mut entries: Vec<(WordPostingEntry, u64)> = Vec::new();
            for &ord in chain.ordinals[ord_idx].iter() {
                let word_entries = word_sfxpost.entries(ord as u32);
                if !word_entries.is_empty() {
                    for e in word_entries {
                        if !active_docs.contains(&e.doc_id) { continue; }
                        entries.push((e, ord));
                    }
                } else {
                    // Chunk ordinal — convert PostingEntry to WordPostingEntry
                    let chunk_entries = chunk_resolver.resolve_filtered(ord, &active_docs);
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

            super::profile::bump(|c| &c.n_word_entries, entries.len() as u64);

            // Same quadratic as in resolve_chains_impl, same fix: index by
            // document first. Measured 175 million pair iterations on
            // `uint64_t` relax over 50k documents.
            let mut by_doc: FnvHashMap<DocId, Vec<u32>> = FnvHashMap::default();
            for (i, (e, _)) in entries.iter().enumerate() {
                by_doc.entry(e.doc_id).or_default().push(i as u32);
            }

            let mut new_active: Vec<(DocId, u32, u32, u32, u32, u64, u32)> = Vec::new();

            for &(doc_id, prev_last_pos, byte_from_first, _, _, _, first_pos) in &active {
                let Some(idxs) = by_doc.get(&doc_id) else { continue };
                for &i in idxs {
                    let (e, ord) = &entries[i as usize];
                    // Counted per iteration: the inner loop breaks on the first
                    // valid entry, so active.len() * entries.len() would be an
                    // upper bound, not the scan actually performed.
                    super::profile::bump(|c| &c.n_word_pairs, 1);
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
                        new_active.push((doc_id, e.last_position, byte_from_first,
                                         e.byte_to, e.byte_from, *ord, first_pos));
                        break;
                    }
                }
            }

            active = new_active;
        }

        // Emit matches
        for &(doc_id, last_pos, byte_from, token_end, last_bf, last_ord, position) in &active {
            results.push(MatchV3 {
                doc_id,
                position,
                span: last_pos.saturating_sub(position) + 1,
                byte_from,
                // The key's consumed bytes may run past the word's content end
                // into the next word's first bytes (its content overlap). Those
                // are real text but not contiguous: report the excess and let the
                // orchestrator place it after the separators.
                overlap_overflow: (last_bf + chain.last_consumed as u32).saturating_sub(token_end) as u8,
                byte_to: (last_bf + chain.last_consumed as u32).max(byte_from).min(token_end),
                token_end,
                sti: chain.first_sti,
                ordinal: chain.ordinals[0][0],
                last_ordinal: last_ord,
            });
        }
    }

    results
}

/// Word chains resolved through word_pos_map — the word pipeline's counterpart
/// of `resolve_chains_v3_posmap`.
///
/// Same contract as `resolve_word_chains_v3`: the next word must start right
/// after the previous one, or after intermediate positions that hold nothing
/// but separators. Instead of materialising the postings of every candidate
/// ordinal and pairing them with the active set (57 million pair iterations on
/// `uint64_t` relaxed over 50k files), this walks forward from the previous
/// word's end: at each position it asks word_pos_map which word starts there
/// and posmap which chunk sits there, and stops at the first content.
///
/// Exact: word_pos_map is derived from the same entries as word_sfxpost, posmap
/// from the same entries as sfxpost. Write collisions are counted, and every
/// emitted match is re-read from its real posting.
pub fn resolve_word_chains_v3_wordmap(
    chains: &[TokenChainV3],
    word_sfxpost: &crate::suffix_fst::word_sfxpost::WordSfxPostReader<'_>,
    chunk_resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
    word_posmap: &crate::suffix_fst::word_pos_map::WordPosMapReader<'_>,
    termtexts: Option<&crate::suffix_fst::termtexts_v3::TermTextsReaderV3<'_>>,
) -> Vec<MatchV3> {
    use crate::suffix_fst::word_sfxpost::WordPostingEntry;
    use crate::suffix_fst::word_pos_map::SPAN_OVERFLOW;

    const CONTENT_RANGES: &[(u8, u8)] = &[
        (b'0', b'9'), (b'A', b'Z'), (b'a', b'z'), (0x80, 0xFF),
    ];

    let mut results = Vec::new();

    // Position 0 has no active set to prune against; chains share their first
    // ordinal lists heavily, so resolve each distinct list once.
    let mut first_memo: FnvHashMap<Vec<u64>, std::rc::Rc<Vec<WordPostingEntry>>> =
        FnvHashMap::default();

    for chain in chains {
        if chain.ordinals.is_empty() {
            continue;
        }

        let first_entries = match first_memo.get(chain.ordinals[0].as_slice()) {
            Some(hit) => hit.clone(),
            None => {
                let v: Vec<WordPostingEntry> = chain.ordinals[0].iter()
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
                let v = std::rc::Rc::new(v);
                first_memo.insert(chain.ordinals[0].as_ref().clone(), v.clone());
                v
            }
        };

        // The head must start inside this posting's own word. A 0x02 key does
        // not fix where its word ends ("0ui" is "0"+"ui" or "0u"+"i" under one
        // ordinal); the posting's byte_to is the content end (WSP2).
        // Checked here, not inside the memoised resolution: the memo is keyed
        // by ordinal list alone and is shared across chains with different
        // first_sti.
        let head_ok = |e: &WordPostingEntry| {
            (e.byte_from + chain.first_sti as u32) < e.byte_to
        };

        if chain.ordinals.len() == 1 {
            for e in first_entries.iter().filter(|e| head_ok(e)) {
                let bf = e.byte_from + chain.first_sti as u32;
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.first_position,
                    span: if e.last_position > e.first_position {
                        e.last_position - e.first_position + 1
                    } else { 1 },
                    byte_from: bf,
                    overlap_overflow: 0,
                    byte_to: bf + chain.total_query_consumed as u32,
                    token_end: bf + chain.total_query_consumed as u32,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: chain.ordinals[0][0],
                });
            }
            continue;
        }

        // Active: (doc, last_word_first_pos, last_pos, byte_from_first, last_ord, last_is_word, first_pos)
        let mut active: Vec<(DocId, u32, u32, u32, u64, bool, u32)> = first_entries.iter()
            .filter(|e| head_ok(e))
            .map(|e| (e.doc_id, e.first_position, e.last_position,
                      e.byte_from + chain.first_sti as u32, 0u64, false, e.first_position))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() { break; }
            let wanted: &[u64] = chain.ordinals[ord_idx].as_slice();

            let mut new_active = Vec::new();
            'next_active: for &(doc_id, _, prev_last, byte_from_first, _, _, first_pos) in &active {
                let mut p = prev_last + 1;
                loop {
                    super::profile::bump(|c| &c.n_wordmap_lookups, 1);

                    // A word starting here?
                    if let Some((word_ord, span)) = word_posmap.word_start_at(doc_id, p) {
                        if wanted.binary_search(&(word_ord as u64)).is_ok() {
                            let last = if span >= SPAN_OVERFLOW {
                                // Span did not fit in 8 bits: read the true end.
                                match word_sfxpost.entry_at(word_ord, doc_id, p) {
                                    Some(e) => e.last_position,
                                    None => {
                                        super::profile::bump(|c| &c.n_wordmap_mismatch, 1);
                                        continue 'next_active;
                                    }
                                }
                            } else {
                                p + span
                            };
                            super::profile::bump(|c| &c.n_wordmap_survivors, 1);
                            new_active.push((doc_id, p, last, byte_from_first,
                                             word_ord as u64, true, first_pos));
                            continue 'next_active;
                        }
                    }

                    // A chunk here? Chains may carry chunk ordinals as alternatives.
                    let Some(chunk_ord) = posmap.ordinal_at(doc_id, p) else {
                        continue 'next_active; // end of document
                    };
                    if wanted.binary_search(&(chunk_ord as u64)).is_ok() {
                        super::profile::bump(|c| &c.n_wordmap_survivors, 1);
                        new_active.push((doc_id, p, p, byte_from_first, chunk_ord as u64, false, first_pos));
                        continue 'next_active;
                    }

                    // Neither. Step over it only if it holds no content at all.
                    if bytemap.bytes_in_ranges(chunk_ord, CONTENT_RANGES) {
                        continue 'next_active;
                    }
                    p += 1;
                }
            }
            active = new_active;
        }

        // Emit, reading each survivor's last posting for its byte span.
        for &(doc_id, last_first, last_pos, byte_from, last_ord, last_is_word, position) in &active {
            let (token_end, last_bf) = if last_is_word {
                match word_sfxpost.entry_at(last_ord as u32, doc_id, last_first) {
                    Some(e) => (e.byte_to, e.byte_from),
                    None => {
                        super::profile::bump(|c| &c.n_wordmap_mismatch, 1);
                        continue;
                    }
                }
            } else {
                match chunk_resolver.resolve_doc(last_ord, doc_id)
                    .into_iter().find(|e| e.position == last_pos)
                {
                    Some(e) => (e.byte_to, e.byte_from),
                    None => {
                        super::profile::bump(|c| &c.n_wordmap_mismatch, 1);
                        continue;
                    }
                }
            };
            results.push(MatchV3 {
                doc_id,
                position,
                span: last_pos.saturating_sub(position) + 1,
                byte_from,
                // The key's consumed bytes may run past the word's content end
                // into the next word's first bytes (its content overlap). Those
                // are real text but not contiguous: report the excess and let the
                // orchestrator place it after the separators.
                overlap_overflow: (last_bf + chain.last_consumed as u32).saturating_sub(token_end) as u8,
                byte_to: (last_bf + chain.last_consumed as u32).max(byte_from).min(token_end),
                token_end,
                sti: chain.first_sti,
                ordinal: chain.ordinals[0][0],
                last_ordinal: last_ord,
            });
        }
    }

    results
}

/// Strict resolution through posmap, with chains grouped by their first list.
///
/// Memoising position 0 saved the *resolution* of a shared list, but every
/// chain sharing it still walked the whole list to do its own lookups: 28 261
/// chains over lists of ~16 000 postings made 459 million posmap lookups on
/// `include` over a 32-segment merged index, for 817 310 survivors — fifty
/// times the lookups of the same query over small segments.
///
/// Here the first step is done once per distinct first list: each active
/// element is looked up once, and the ordinal found at the next position is
/// dispatched to every chain of the group that wants it there. From the second
/// step on, each chain continues alone with its own survivors, as before.
///
/// Same results as the per-chain walk: a chain sees exactly the survivors it
/// would have found itself, in the same order.
fn resolve_chains_posmap_grouped(
    chains: &[TokenChainV3],
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    first_memo: &mut FnvHashMap<Vec<u64>, std::rc::Rc<Vec<PostingEntry>>>,
) -> Vec<MatchV3> {
    let mut results = Vec::new();

    // Group chain indices by first list. Keyed on content, like the memo.
    let mut groups: FnvHashMap<Vec<u64>, Vec<usize>> = FnvHashMap::default();
    for (i, chain) in chains.iter().enumerate() {
        if chain.ordinals.is_empty() { continue; }
        groups.entry(chain.ordinals[0].as_ref().clone()).or_default().push(i);
    }

    // Active survivor: (doc, last_pos, byte_from_first, last_ord, first_pos)
    type Active = (DocId, u32, u32, u64, u32);

    super::profile::bump(|c| &c.n_groups_shared, groups.len() as u64);
    for (first_key, members) in groups {
        let first_entries = match first_memo.get(first_key.as_slice()) {
            Some(hit) => hit.clone(),
            None => {
                let v = std::rc::Rc::new(
                    resolve_alternatives(resolver, &first_key, filter_docs));
                super::profile::bump(|c| &c.n_chain_first, v.len() as u64);
                first_memo.insert(first_key.clone(), v.clone());
                v
            }
        };

        // Single-position chains emit straight from the first list.
        // Multi-position chains register what they want at position 1.
        //
        // A dispatch map only pays when several chains share the list. A lone
        // chain keeps a binary search on its own wanted slice: building a map
        // from a tail list of hundreds of ordinals, for each of the 3.4 million
        // single-member groups of `__init`, cost 30 seconds.
        let shared_head = members.len() > 1;
        // Distinct tail lists of the group, with the chains wanting each. Tail
        // lists are Arc-shared and few; the ordinals in them are many (3 000
        // alternatives for `init…`). Keying a map by ordinal made 55 million
        // inserts on `__init`; keying by list and binary-searching makes none.
        let mut tails: Vec<(std::sync::Arc<Vec<u64>>, Vec<usize>)> = Vec::new();
        let mut survivors: FnvHashMap<usize, Vec<Active>> = FnvHashMap::default();
        for &ci in &members {
            let chain = &chains[ci];
            if chain.ordinals.len() == 1 {
                for e in first_entries.iter() {
                    let bf = e.byte_from + chain.first_sti as u32;
                    results.push(MatchV3 {
                        doc_id: e.doc_id,
                        position: e.position,
                        span: 1,
                        byte_from: bf,
                        overlap_overflow: 0,
                        byte_to: bf + chain.total_query_consumed as u32,
                        token_end: e.byte_to,
                        sti: chain.first_sti,
                        ordinal: chain.ordinals[0][0],
                        last_ordinal: chain.ordinals[0][0],
                    });
                }
            } else if shared_head {
                let list = &chain.ordinals[1];
                match tails.iter_mut().find(|(l, _)| std::sync::Arc::ptr_eq(l, list) || **l == **list) {
                    Some((_, cs)) => cs.push(ci),
                    None => {
                        super::profile::bump(|c| &c.n_dispatch_inserts, 1);
                        tails.push((std::sync::Arc::clone(list), vec![ci]));
                    }
                }
            } else {
                // Lone chain: step 1 with a binary search, no map.
                let wanted: &[u64] = chain.ordinals[1].as_slice();
                let mut found: Vec<Active> = Vec::new();
                for e in first_entries.iter() {
                    let next_pos = e.position + 1;
                    super::profile::bump(|c| &c.n_posmap_lookups, 1);
                    let Some(ord) = posmap.ordinal_at(e.doc_id, next_pos) else { continue };
                    let ord = ord as u64;
                    if wanted.binary_search(&ord).is_err() { continue; }
                    super::profile::bump(|c| &c.n_posmap_survivors, 1);
                    found.push((e.doc_id, next_pos, e.byte_from + chain.first_sti as u32, ord, e.position));
                }
                if !found.is_empty() { survivors.insert(ci, found); }
            }
        }

        // Step 1, once for the whole group: one lookup per entry, one binary
        // search per distinct tail list.
        if !tails.is_empty() {
            for e in first_entries.iter() {
                let next_pos = e.position + 1;
                super::profile::bump(|c| &c.n_posmap_lookups, 1);
                let Some(ord) = posmap.ordinal_at(e.doc_id, next_pos) else { continue };
                let ord = ord as u64;
                for (list, wanting) in &tails {
                    if list.binary_search(&ord).is_err() { continue; }
                    super::profile::bump(|c| &c.n_posmap_survivors, wanting.len() as u64);
                    for &ci in wanting {
                        survivors.entry(ci).or_default().push((
                            e.doc_id, next_pos, e.byte_from + chains[ci].first_sti as u32, ord, e.position,
                        ));
                    }
                }
            }
        }

        // Steps 2+, per chain.
        for &ci in &members {
            let chain = &chains[ci];
            if chain.ordinals.len() == 1 { continue; }
            let Some(mut active) = survivors.remove(&ci) else { continue };

            for ord_idx in 2..chain.ordinals.len() {
                if active.is_empty() { break; }
                let wanted: &[u64] = chain.ordinals[ord_idx].as_slice();
                let mut next = Vec::new();
                for &(doc_id, prev_pos, bf_first, _, first_pos) in &active {
                    let next_pos = prev_pos + 1;
                    super::profile::bump(|c| &c.n_posmap_lookups, 1);
                    let Some(ord) = posmap.ordinal_at(doc_id, next_pos) else { continue };
                    let ord = ord as u64;
                    if wanted.binary_search(&ord).is_err() { continue; }
                    super::profile::bump(|c| &c.n_posmap_survivors, 1);
                    next.push((doc_id, next_pos, bf_first, ord, first_pos));
                }
                active = next;
            }

            // Emit: fetch the last token's posting once per match, and let it
            // confirm the position posmap claimed.
            for &(doc_id, last_pos, byte_from, last_ord, position) in &active {
                let Some(e) = resolver.resolve_doc(last_ord, doc_id)
                    .into_iter().find(|e| e.position == last_pos)
                else {
                    super::profile::bump(|c| &c.n_posmap_mismatch, 1);
                    continue;
                };
                results.push(MatchV3 {
                    doc_id,
                    position,
                    span: last_pos.saturating_sub(position) + 1,
                    byte_from,
                    overlap_overflow: 0,
                    byte_to: (e.byte_from + chain.last_consumed as u32).max(byte_from),
                    token_end: e.byte_to,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: last_ord,
                });
            }
        }
    }

    results
}

/// Word chains through word_pos_map, grouped by head.
///
/// `resolve_word_chains_v3_wordmap` walks every chain on its own: the head's
/// posting list is resolved once (memo) but re-scanned per chain, and the
/// forward scan from each posting — over pure-separator chunks, up to the next
/// content — is redone per chain too. Relaxed `uint64_t` over a 32-segment
/// merged index: 1 741 chains, 17.5 million lookups for 62 736 survivors,
/// because hundreds of chains start on the same frequent words (`u`, `ui`,
/// `uint`…).
///
/// Here chains are grouped by (head list, first_sti). The forward scan from
/// each head posting happens once per group and lands on one (ordinal, span);
/// that ordinal is then dispatched to every member whose second list wants it.
/// Members continue alone from their third position, as before. Same results.
pub fn resolve_word_chains_v3_wordmap_grouped(
    chains: &[TokenChainV3],
    word_sfxpost: &crate::suffix_fst::word_sfxpost::WordSfxPostReader<'_>,
    chunk_resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
    posmap: &crate::suffix_fst::posmap::PosMapReader<'_>,
    bytemap: &crate::suffix_fst::bytemap::ByteBitmapReader<'_>,
    word_posmap: &crate::suffix_fst::word_pos_map::WordPosMapReader<'_>,
    termtexts: Option<&crate::suffix_fst::termtexts_v3::TermTextsReaderV3<'_>>,
) -> Vec<MatchV3> {
    use crate::suffix_fst::word_sfxpost::WordPostingEntry;
    use crate::suffix_fst::word_pos_map::SPAN_OVERFLOW;

    const CONTENT_RANGES: &[(u8, u8)] = &[
        (b'0', b'9'), (b'A', b'Z'), (b'a', b'z'), (0x80, 0xFF),
    ];

    // Single-position chains have no step to share: the per-chain walker
    // handles them exactly. Multi-position chains are grouped.
    let (singles, multis): (Vec<TokenChainV3>, Vec<TokenChainV3>) = chains.iter()
        .filter(|c| !c.ordinals.is_empty())
        .cloned()
        .partition(|c| c.ordinals.len() == 1);
    let mut results = resolve_word_chains_v3_wordmap(
        &singles, word_sfxpost, chunk_resolver, filter_docs, posmap, bytemap, word_posmap, termtexts);

    // One forward step from (doc, prev_last): the next content position, the
    // ordinal there (word or chunk), its last position, and whether it is a word.
    let step = |doc_id: DocId, prev_last: u32| -> Option<(u32, u32, u64, bool)> {
        let mut p = prev_last + 1;
        loop {
            super::profile::bump(|c| &c.n_wordmap_lookups, 1);
            if let Some((word_ord, span)) = word_posmap.word_start_at(doc_id, p) {
                let last = if span >= SPAN_OVERFLOW {
                    word_sfxpost.entry_at(word_ord, doc_id, p)?.last_position
                } else { p + span };
                return Some((p, last, word_ord as u64, true));
            }
            let chunk_ord = posmap.ordinal_at(doc_id, p)?;
            if bytemap.bytes_in_ranges(chunk_ord, CONTENT_RANGES) {
                // Content chunk that starts no word: only a chunk alternative can
                // take it. The caller checks membership; report it as a chunk.
                return Some((p, p, chunk_ord as u64, false));
            }
            p += 1;
        }
    };

    let mut groups: FnvHashMap<(Vec<u64>, u16), Vec<usize>> = FnvHashMap::default();
    for (i, c) in multis.iter().enumerate() {
        groups.entry((c.ordinals[0].as_ref().clone(), c.first_sti)).or_default().push(i);
    }
    let mut first_memo: FnvHashMap<Vec<u64>, std::rc::Rc<Vec<WordPostingEntry>>> = FnvHashMap::default();

    // Active: (doc, last_word_first_pos, last_pos, byte_from_first, last_ord, last_is_word, first_pos)
    type Active = (DocId, u32, u32, u32, u64, bool, u32);

    for ((head_key, first_sti), members) in groups {
        let first_entries = match first_memo.get(head_key.as_slice()) {
            Some(hit) => hit.clone(),
            None => {
                let v: Vec<WordPostingEntry> = head_key.iter().flat_map(|&ord| {
                    let we = word_sfxpost.entries(ord as u32);
                    let entries: Vec<WordPostingEntry> = if !we.is_empty() { we } else {
                        let chunk = if let Some(f) = filter_docs {
                            chunk_resolver.resolve_filtered(ord, f)
                        } else { chunk_resolver.resolve(ord) };
                        chunk.into_iter().map(|e| WordPostingEntry {
                            doc_id: e.doc_id, first_position: e.position, last_position: e.position,
                            byte_from: e.byte_from, byte_to: e.byte_to,
                        }).collect()
                    };
                    let mut f = entries;
                    if let Some(filter) = filter_docs { f.retain(|e| filter.contains(&e.doc_id)); }
                    f
                }).collect();
                let v = std::rc::Rc::new(v);
                first_memo.insert(head_key.clone(), v.clone());
                v
            }
        };

        // Distinct second lists of the group, with the members wanting each.
        let mut tails: Vec<(std::sync::Arc<Vec<u64>>, Vec<usize>)> = Vec::new();
        for &ci in &members {
            let list = &multis[ci].ordinals[1];
            match tails.iter_mut().find(|(l, _)| std::sync::Arc::ptr_eq(l, list) || **l == **list) {
                Some((_, cs)) => cs.push(ci),
                None => tails.push((std::sync::Arc::clone(list), vec![ci])),
            }
        }

        // Step 1, once per head posting.
        let mut survivors: FnvHashMap<usize, Vec<Active>> = FnvHashMap::default();
        for e in first_entries.iter() {
            if first_sti > 0 {
                let content_end = e.byte_to;
                if e.byte_from + first_sti as u32 >= content_end { continue; }
            }
            let Some((p, last, ord, is_word)) = step(e.doc_id, e.last_position) else { continue };
            for (list, wanting) in &tails {
                if list.binary_search(&ord).is_err() { continue; }
                super::profile::bump(|c| &c.n_wordmap_survivors, wanting.len() as u64);
                for &ci in wanting {
                    survivors.entry(ci).or_default().push((
                        e.doc_id, p, last, e.byte_from + first_sti as u32, ord, is_word, e.first_position,
                    ));
                }
            }
        }

        // Steps 2+ and emit, per member.
        for &ci in &members {
            let chain = &multis[ci];
            let Some(mut active) = survivors.remove(&ci) else { continue };
            for ord_idx in 2..chain.ordinals.len() {
                if active.is_empty() { break; }
                let wanted: &[u64] = chain.ordinals[ord_idx].as_slice();
                let mut next = Vec::new();
                for &(doc_id, _, prev_last, bf_first, _, _, first_pos) in &active {
                    let Some((p, last, ord, is_word)) = step(doc_id, prev_last) else { continue };
                    if wanted.binary_search(&ord).is_err() { continue; }
                    super::profile::bump(|c| &c.n_wordmap_survivors, 1);
                    next.push((doc_id, p, last, bf_first, ord, is_word, first_pos));
                }
                active = next;
            }
            for &(doc_id, last_first, last_pos, byte_from, last_ord, last_is_word, position) in &active {
                let (token_end, last_bf) = if last_is_word {
                    match word_sfxpost.entry_at(last_ord as u32, doc_id, last_first) {
                        Some(e) => (e.byte_to, e.byte_from),
                        None => { super::profile::bump(|c| &c.n_wordmap_mismatch, 1); continue; }
                    }
                } else {
                    match chunk_resolver.resolve_doc(last_ord, doc_id).into_iter().find(|e| e.position == last_pos) {
                        Some(e) => (e.byte_to, e.byte_from),
                        None => { super::profile::bump(|c| &c.n_wordmap_mismatch, 1); continue; }
                    }
                };
                results.push(MatchV3 {
                    doc_id,
                    position,
                    span: last_pos.saturating_sub(position) + 1,
                    byte_from,
                    overlap_overflow: (last_bf + chain.last_consumed as u32).saturating_sub(token_end) as u8,
                    byte_to: (last_bf + chain.last_consumed as u32).max(byte_from).min(token_end),
                    token_end,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: last_ord,
                });
            }
        }
    }

    results
}

enum AdjacencyMode<'a> {
    /// pos[i+1] == pos[i] + 1
    Strict,
    /// Same contract as Strict, answered the other way round.
    ///
    /// Strict asks the resolver for every posting of the next ordinals, then looks
    /// for one at pos[i] + 1. This asks posmap which ordinal sits at pos[i] + 1 and
    /// checks it against the chain's set — one O(1) lookup per active element, no
    /// posting list materialised. Only the survivors have their bytes fetched.
    ///
    /// Exact, not approximate: posmap is built from the very sfxpost entries the
    /// resolver serves (PosMapIndex::on_posting), and each (doc, position) carries
    /// one content ordinal. Collisions are counted at write time, and every
    /// survivor is re-checked against its real posting here.
    StrictPosmap {
        posmap: &'a crate::suffix_fst::posmap::PosMapReader<'a>,
    },
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

    // Position 0 has no active set to prune against, so its postings are resolved
    // in full — and chains overwhelmingly share their starting ordinals. Caching
    // by that ordinal list turned 39 million first-position postings into the
    // handful of distinct lists actually involved, on `include`.
    let mut first_memo: FnvHashMap<Vec<u64>, std::rc::Rc<Vec<PostingEntry>>> =
        FnvHashMap::default();

    // Chains that share their first list are resolved as a group (one pass over
    // the shared list, see resolve_chains_posmap_grouped). Chains whose first
    // list is unique gain nothing from grouping and must not pay for it: on
    // `__init` 3.4 million lone chains paid a map and a key clone each, for 2x.
    let mut lone: Vec<&TokenChainV3> = Vec::new();
    if let AdjacencyMode::StrictPosmap { posmap } = &adjacency {
        let mut count: FnvHashMap<&[u64], u32> = FnvHashMap::default();
        for chain in chains {
            if chain.ordinals.is_empty() { continue; }
            *count.entry(chain.ordinals[0].as_slice()).or_insert(0) += 1;
        }
        let mut shared: Vec<TokenChainV3> = Vec::new();
        for chain in chains {
            if chain.ordinals.is_empty() { continue; }
            if count[chain.ordinals[0].as_slice()] > 1 {
                shared.push(chain.clone());
            } else {
                lone.push(chain);
            }
        }
        super::profile::bump(|c| &c.n_chains_shared, shared.len() as u64);
        results.extend(resolve_chains_posmap_grouped(
            &shared, resolver, filter_docs, posmap, &mut first_memo));
    } else {
        lone.extend(chains.iter());
    }

    for chain in lone {
        if chain.ordinals.is_empty() {
            continue;
        }

        // Resolve all alternatives at position 0
        let first_entries = match first_memo.get(chain.ordinals[0].as_slice()) {
            Some(hit) => hit.clone(),
            None => {
                let v = std::rc::Rc::new(
                    resolve_alternatives(resolver, &chain.ordinals[0], filter_docs));
                super::profile::bump(|c| &c.n_chain_first, v.len() as u64);
                first_memo.insert(chain.ordinals[0].as_ref().clone(), v.clone());
                v
            }
        };
        let first_entries: &[PostingEntry] = &first_entries;

        if chain.ordinals.len() == 1 {
            for e in first_entries {
                let bf = e.byte_from + chain.first_sti as u32;
                results.push(MatchV3 {
                    doc_id: e.doc_id,
                    position: e.position,
                    span: 1,
                    byte_from: bf,
                    overlap_overflow: 0,
                    byte_to: bf + chain.total_query_consumed as u32,
                    token_end: e.byte_to,
                    sti: chain.first_sti,
                    ordinal: chain.ordinals[0][0],
                    last_ordinal: chain.ordinals[0][0],
                });
            }
            continue;
        }

        // Multi-position chain
        // Active set: (doc_id, prev_position, byte_from_first, token_end_last,
        //              byte_from_last, last_ord_matched)
        // byte_from_last is what lets the emitted byte_to measure the match instead
        // of running to the end of the tail token.
        // (doc, prev_pos, byte_from_first, token_end_last, byte_from_last, last_ord, first_pos)
        let mut active: Vec<(DocId, u32, u32, u32, u32, u64, u32)> = first_entries
            .iter()
            .map(|e| (e.doc_id, e.position, e.byte_from + chain.first_sti as u32,
                      e.byte_to, e.byte_from, 0u64, e.position))
            .collect();

        for ord_idx in 1..chain.ordinals.len() {
            if active.is_empty() {
                break;
            }

            if let AdjacencyMode::StrictPosmap { posmap } = &adjacency {
                // Which ordinals may sit at the next position. Chains carry a
                // handful of alternatives; a sorted Vec beats a set at that size.
                // The memoised lists come out of build_chains_from_splits already
                // sorted and deduplicated; sibling-DFS singletons trivially are.
                let wanted: &[u64] = chain.ordinals[ord_idx].as_slice();
                debug_assert!(wanted.windows(2).all(|w| w[0] < w[1]));

                // Bytes are not fetched here. An intermediate position's span is
                // never used — only the last token's reaches the emitted match —
                // and most survivors of one position die at the next (52 702
                // survivors for 287 matches on `__init`). The tuple keeps
                // placeholders; the emit loop below fetches the real posting for
                // whatever is still active, and validates posmap's claim there.
                let mut new_active: Vec<(DocId, u32, u32, u32, u32, u64, u32)> = Vec::new();
                for &(doc_id, prev_pos, byte_from_first, _, _, _, first_pos) in &active {
                    let next_pos = prev_pos + 1;
                    super::profile::bump(|c| &c.n_posmap_lookups, 1);
                    let Some(ord) = posmap.ordinal_at(doc_id, next_pos) else { continue };
                    let ord = ord as u64;
                    if wanted.binary_search(&ord).is_err() { continue; }
                    if std::env::var("V3_DIAG_RESOLVE").is_ok() {
                        eprintln!("[resolve] doc={doc_id} pos={next_pos} ord={ord} wanted={:?} sorted={}",
                            wanted, wanted.windows(2).all(|w| w[0] < w[1]));
                    }
                    super::profile::bump(|c| &c.n_posmap_survivors, 1);
                    new_active.push((doc_id, next_pos, byte_from_first, 0, 0, ord, first_pos));
                }
                active = new_active;
                continue;
            }

            // Union postings from all alternative ordinals at this position,
            // tagging each with its ordinal for word map verification.
            //
            // Only documents still in the active set can extend a chain, so ask
            // the resolver for those and nothing else. Materialising the full
            // posting list and discarding it in the pairing loop cost 264 million
            // entries on `spin_lock` over a 32-segment merged index, against
            // 1.8 million that were actually paired.
            let active_docs: HashSet<DocId> =
                active.iter().map(|&(doc_id, ..)| doc_id).collect();
            let mut entries: Vec<(PostingEntry, u64)> = Vec::new();
            for &ord in chain.ordinals[ord_idx].iter() {
                for e in resolver.resolve_filtered(ord, &active_docs) {
                    entries.push((e, ord));
                }
            }
            super::profile::bump(|c| &c.n_chain_entries, entries.len() as u64);

            // Index the postings by document before pairing them with the active
            // set. Both lists grow linearly with the segment, so scanning all of
            // `entries` for every active element is quadratic in segment size —
            // invisible across many small segments, dominant on merged ones
            // (measured 98.9% of query time on a 32-segment merged index).
            //
            // Indices are pushed in order and walked in order, so each active
            // element still sees its candidates exactly as before: the `break` on
            // first match picks the same entry it used to.
            let mut by_doc: FnvHashMap<DocId, Vec<u32>> = FnvHashMap::default();
            for (i, (e, _)) in entries.iter().enumerate() {
                by_doc.entry(e.doc_id).or_default().push(i as u32);
            }

            let mut new_active: Vec<(DocId, u32, u32, u32, u32, u64, u32)> = Vec::new();

            for &(doc_id, prev_pos, byte_from_first, byte_to_prev, _, _, first_pos) in &active {
                let Some(idxs) = by_doc.get(&doc_id) else { continue };
                for &i in idxs {
                    super::profile::bump(|c| &c.n_chain_pairs, 1);
                    let (e, ord) = &entries[i as usize];

                    let valid = match &adjacency {
                        AdjacencyMode::Strict | AdjacencyMode::StrictPosmap { .. } => {
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
                        new_active.push((doc_id, e.position, byte_from_first,
                                         e.byte_to, e.byte_from, *ord, first_pos));
                        break;
                    }
                }
            }

            active = new_active;
        }

        // Emit matches
        for &(doc_id, last_pos, byte_from, token_end, last_bf, last_ord, position) in &active {
            // Under posmap resolution the last token's bytes are still unknown
            // (see above). Fetch its posting now — once per emitted match — and
            // let it confirm the position posmap claimed.
            let (token_end, last_bf) = if matches!(adjacency, AdjacencyMode::StrictPosmap { .. })
                && chain.ordinals.len() > 1
            {
                match resolver.resolve_doc(last_ord, doc_id)
                    .into_iter().find(|e| e.position == last_pos)
                {
                    Some(e) => (e.byte_to, e.byte_from),
                    None => {
                        super::profile::bump(|c| &c.n_posmap_mismatch, 1);
                        continue;
                    }
                }
            } else {
                (token_end, last_bf)
            };
            results.push(MatchV3 {
                doc_id,
                position,
                span: last_pos.saturating_sub(position) + 1,
                byte_from,
                overlap_overflow: 0,
                byte_to: (last_bf + chain.last_consumed as u32).max(byte_from),
                token_end,
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

    super::profile::bump(|c| &c.n_puresep_calls, 1);

    for pos in pos_from..pos_to {
        // Counted inside the loop, not from the range: the loop bails on the
        // first content byte, so the range width measures intent, not work.
        super::profile::bump(|c| &c.n_puresep_positions, 1);
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
        let matches = resolve_single_v3(&cands, &resolver, None, "mutex_lo".len() as u32);

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
        let matches = resolve_single_v3(&cands, &resolver, Some(&filter), "mutex_lo".len() as u32);

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
