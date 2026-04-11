//! Composable pipeline for literal resolution.
//!
//! Breaks `find_literal` into independent briques that can be composed
//! with filtering and selectivity ordering:
//!
//! 1. `fst_candidates`         — FST walk, no resolve, O(FST range scan)
//! 2. `resolve_candidates`     — resolve postings, filterable by doc_ids
//! 3. `cross_token_falling_walk` — falling walk + sibling chain DFS, no resolve
//! 4. `resolve_chains`         — resolve + adjacency check, filterable by doc_ids
//!
//! The existing `find_literal` / `suffix_contains_*` functions are NOT modified.

use std::collections::{HashMap, HashSet};

use crate::suffix_fst::builder::ParentEntry;
use crate::suffix_fst::file::SfxFileReader;
use crate::query::posting_resolver::{PostingResolver, PostingEntry};
use crate::DocId;

use super::literal_resolve::LiteralMatch;

// ─────────────────────────────────────────────────────────────────────
// Brique 1 : fst_candidates — FST walk without posting resolve
// ─────────────────────────────────────────────────────────────────────

/// A candidate from the FST walk: an ordinal + its suffix position.
/// No posting data — just what the FST tells us.
#[derive(Debug, Clone)]
pub struct FstCandidate {
    pub raw_ordinal: u64,
    pub si: u16,
    pub token_len: u16,
}

/// Walk the SFX FST for a literal query string. Returns all parent entries
/// that match (ordinal, si, token_len) WITHOUT resolving any postings.
///
/// The number of results is a selectivity estimate: fewer = more selective.
///
/// Cost: O(FST range scan). Typically < 0.01ms.
pub fn fst_candidates(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
) -> Vec<FstCandidate> {
    let query_lower = literal.to_lowercase();
    let walk_results = sfx_reader.prefix_walk(&query_lower);

    let mut candidates = Vec::new();
    for (_suffix_term, parents) in &walk_results {
        for parent in parents {
            candidates.push(FstCandidate {
                raw_ordinal: parent.raw_ordinal,
                si: parent.si,
                token_len: parent.token_len,
            });
        }
    }
    candidates
}

// ─────────────────────────────────────────────────────────────────────
// Brique 2 : resolve_candidates — resolve postings with doc_id filter
// ─────────────────────────────────────────────────────────────────────

/// Resolve posting entries for a set of FstCandidates.
///
/// If `filter_docs` is Some, only returns entries for docs in the set
/// (uses `resolve_filtered` for efficiency).
///
/// `literal_len` is the byte length of the literal being searched — needed
/// to compute byte_to = byte_from + si + literal_len.
pub fn resolve_candidates(
    candidates: &[FstCandidate],
    literal_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<LiteralMatch> {
    let mut matches = Vec::new();

    for cand in candidates {
        let postings = if let Some(doc_set) = filter_docs {
            resolver.resolve_filtered(cand.raw_ordinal, doc_set)
        } else {
            resolver.resolve(cand.raw_ordinal)
        };

        for entry in &postings {
            matches.push(LiteralMatch {
                doc_id: entry.doc_id,
                position: entry.position,
                byte_from: entry.byte_from + cand.si as u32,
                byte_to: entry.byte_from + cand.si as u32 + literal_len as u32,
                si: cand.si,
                token_len: cand.token_len,
                ordinal: cand.raw_ordinal as u32,
            });
        }
    }

    matches
}

// ─────────────────────────────────────────────────────────────────────
// Brique 3 : cross_token_falling_walk — falling walk + sibling DFS
// ─────────────────────────────────────────────────────────────────────

/// A validated cross-token chain: sequence of ordinals that cover the query
/// across token boundaries.
#[derive(Debug, Clone)]
pub struct CrossTokenChain {
    /// Ordinals in chain order (first token, second token, ...).
    pub ordinals: Vec<u64>,
    /// SI of the first candidate (where the suffix starts in the first token).
    pub first_si: u16,
    /// How many bytes of the query the first token consumes.
    pub prefix_len: usize,
}

/// Perform falling walk + sibling chain DFS for a literal.
/// Returns validated chains WITHOUT resolving any postings.
/// Only uses contiguous siblings (gap_len == 0, e.g. CamelCase).
///
/// For fuzzy_distance > 0, uses fuzzy_falling_walk (Levenshtein DFA in FST).
///
/// Cost: O(n_split_candidates × sibling_branching × chain_depth).
/// No posting resolution — only FST walk + TermTexts lookups.
pub fn cross_token_falling_walk(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    fuzzy_distance: u8,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<CrossTokenChain> {
    cross_token_falling_walk_inner(sfx_reader, literal, fuzzy_distance, ord_to_term, false)
}

/// Same as `cross_token_falling_walk` but allows siblings with gaps
/// (separators between tokens). Used by fuzzy contains to find trigrams
/// that span word boundaries like "bva" in "rag3db_value".
pub fn cross_token_falling_walk_any_gap(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    fuzzy_distance: u8,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<CrossTokenChain> {
    cross_token_falling_walk_inner(sfx_reader, literal, fuzzy_distance, ord_to_term, true)
}

fn cross_token_falling_walk_inner(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    fuzzy_distance: u8,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    allow_gaps: bool,
) -> Vec<CrossTokenChain> {
    let query_lower = literal.to_lowercase();

    // Step 1: falling_walk → split candidates
    let candidates = if fuzzy_distance > 0 {
        sfx_reader.fuzzy_falling_walk(&query_lower, fuzzy_distance)
    } else {
        sfx_reader.falling_walk(&query_lower)
    };

    if candidates.is_empty() {
        return Vec::new();
    }

    let sibling_table = sfx_reader.sibling_table();
    let has_siblings = sibling_table.is_some();

    // Filter: keep candidates that have siblings (for cross-token) or consume all (single-token fuzzy)
    let candidates: Vec<_> = if has_siblings {
        let sib = sibling_table.unwrap();
        candidates.into_iter().filter(|c| {
            let consumes_all = c.prefix_len >= query_lower.len();
            let has_sibling = if allow_gaps {
                !sib.siblings(c.parent.raw_ordinal as u32).is_empty()
            } else {
                !sib.contiguous_siblings(c.parent.raw_ordinal as u32).is_empty()
            };
            consumes_all || has_sibling
        }).collect()
    } else {
        candidates
    };

    // Step 2: sibling chain DFS for each candidate
    const MAX_CHAIN_DEPTH: usize = 8;
    let mut chains: Vec<CrossTokenChain> = Vec::new();

    for cand in &candidates {
        let split_at = cand.prefix_len.min(query_lower.len());
        let remainder = &query_lower[split_at..];

        if remainder.is_empty() {
            // Entire query consumed by single token (possibly fuzzy)
            chains.push(CrossTokenChain {
                ordinals: vec![cand.parent.raw_ordinal],
                first_si: cand.parent.si,
                prefix_len: cand.prefix_len,
            });
            continue;
        }

        if let Some(sib_table) = sibling_table {
            // DFS worklist: (current_ord, remainder, chain_so_far, depth)
            let mut stack: Vec<(u64, &str, Vec<u64>, usize)> = vec![
                (cand.parent.raw_ordinal, remainder, vec![cand.parent.raw_ordinal], 0)
            ];

            while let Some((cur_ord, rem, chain, depth)) = stack.pop() {
                if rem.is_empty() || depth >= MAX_CHAIN_DEPTH {
                    if rem.is_empty() {
                        chains.push(CrossTokenChain {
                            ordinals: chain,
                            first_si: cand.parent.si,
                            prefix_len: cand.prefix_len,
                        });
                    }
                    continue;
                }

                let sibling_ords: Vec<u32> = if allow_gaps {
                    sib_table.siblings(cur_ord as u32).into_iter().map(|s| s.next_ordinal).collect()
                } else {
                    sib_table.contiguous_siblings(cur_ord as u32)
                };
                let first_byte = rem.as_bytes()[0];
                for &next_ord in &sibling_ords {
                    let next_text = match ord_to_term(next_ord as u64) {
                        Some(t) => t,
                        None => continue,
                    };

                    // Fast skip: if the sibling token doesn't start with
                    // the first byte of the remainder, it can't match.
                    if next_text.as_bytes().first().copied() != Some(first_byte) {
                        continue;
                    }

                    if rem == next_text {
                        // Exact match → terminal
                        let mut c = chain.clone();
                        c.push(next_ord as u64);
                        chains.push(CrossTokenChain {
                            ordinals: c,
                            first_si: cand.parent.si,
                            prefix_len: cand.prefix_len,
                        });
                    } else if next_text.starts_with(rem) {
                        // Token covers remainder → terminal
                        let mut c = chain.clone();
                        c.push(next_ord as u64);
                        chains.push(CrossTokenChain {
                            ordinals: c,
                            first_si: cand.parent.si,
                            prefix_len: cand.prefix_len,
                        });
                    } else if rem.starts_with(&next_text) {
                        // Partial consumption → continue DFS
                        let mut c = chain.clone();
                        c.push(next_ord as u64);
                        stack.push((next_ord as u64, &rem[next_text.len()..], c, depth + 1));
                    }
                }
            }
        } else {
            // No sibling table → fallback: prefix_walk_si0 on remainder
            let right_walks = if fuzzy_distance > 0 {
                sfx_reader.fuzzy_walk_si0(remainder, fuzzy_distance)
            } else {
                sfx_reader.prefix_walk_si0(remainder)
            };
            for (_suffix, parents) in &right_walks {
                for rp in parents {
                    if rp.si == 0 {
                        chains.push(CrossTokenChain {
                            ordinals: vec![cand.parent.raw_ordinal, rp.raw_ordinal],
                            first_si: cand.parent.si,
                            prefix_len: cand.prefix_len,
                        });
                    }
                }
            }
        }
    }

    chains
}

// ─────────────────────────────────────────────────────────────────────
// Brique 4 : resolve_chains — resolve + adjacency check with filter
// ─────────────────────────────────────────────────────────────────────

/// Resolve postings for cross-token chains and verify adjacency + byte continuity.
///
/// If `filter_docs` is Some, only resolves postings for docs in the set.
///
/// Returns LiteralMatch entries for validated matches (correct position
/// ordering and byte continuity between chained tokens).
pub fn resolve_chains(
    chains: &[CrossTokenChain],
    literal_len: usize,
    resolver: &dyn PostingResolver,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<LiteralMatch> {
    if chains.is_empty() {
        return Vec::new();
    }

    // Build ordinal cache: resolve each unique ordinal once
    let mut ordinal_cache: HashMap<u64, Vec<PostingEntry>> = HashMap::new();
    for chain in chains {
        for &ord in &chain.ordinals {
            ordinal_cache.entry(ord).or_insert_with(|| {
                if let Some(doc_set) = filter_docs {
                    resolver.resolve_filtered(ord, doc_set)
                } else {
                    resolver.resolve(ord)
                }
            });
        }
    }

    let mut results: Vec<LiteralMatch> = Vec::new();

    for chain in chains {
        let first_si = chain.first_si as usize;

        let first_postings = match ordinal_cache.get(&chain.ordinals[0]) {
            Some(p) => p,
            None => continue,
        };

        if chain.ordinals.len() == 1 {
            // Single-ordinal chain (single-token fuzzy match)
            for p in first_postings {
                results.push(LiteralMatch {
                    doc_id: p.doc_id,
                    position: p.position,
                    byte_from: p.byte_from + first_si as u32,
                    byte_to: p.byte_from + first_si as u32 + literal_len as u32,
                    si: chain.first_si,
                    token_len: (p.byte_to - p.byte_from) as u16,
                    ordinal: chain.ordinals[0] as u32,
                });
            }
            continue;
        }

        // Multi-ordinal chain: verify adjacency + byte continuity
        // active = (doc_id, expected_next_position, expected_byte_from, match_byte_from, first_position)
        let mut active: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
        for p in first_postings {
            let bf = p.byte_from + first_si as u32;
            active.push((p.doc_id, p.position + 1, p.byte_to, bf, p.position));
        }

        for &ord in &chain.ordinals[1..] {
            if active.is_empty() { break; }
            let postings = match ordinal_cache.get(&ord) {
                Some(p) => p,
                None => { active.clear(); break; }
            };

            active.sort_by_key(|a| (a.0, a.1));
            let mut next_active: Vec<(u32, u32, u32, u32, u32)> = Vec::new();

            for p in postings {
                let target = (p.doc_id, p.position);
                let idx = active.partition_point(|a| (a.0, a.1) < target);
                let mut i = idx;
                while i < active.len() && active[i].0 == p.doc_id && active[i].1 == p.position {
                    // Check byte continuity
                    if p.byte_from == active[i].2 {
                        next_active.push((p.doc_id, p.position + 1, p.byte_to, active[i].3, active[i].4));
                    }
                    i += 1;
                }
            }
            active = next_active;
        }

        // Emit results for surviving chains
        for (doc_id, _, last_byte_to, match_bf, first_pos) in &active {
            results.push(LiteralMatch {
                doc_id: *doc_id,
                position: *first_pos,
                byte_from: *match_bf,
                byte_to: *match_bf + literal_len as u32,
                si: chain.first_si,
                token_len: 0,
                ordinal: chain.ordinals[0] as u32,
            });
        }
    }

    results
}

// ─────────────────────────────────────────────────────────────────────
// Convenience: find_literal_pipeline — all 4 briques composed
// ─────────────────────────────────────────────────────────────────────

/// Equivalent to `find_literal` but using the pipeline briques.
/// Supports optional doc_id filtering.
///
/// This is NOT used by the existing code — it's here to validate
/// that the briques produce the same results as `find_literal`.
#[allow(dead_code)]
pub fn find_literal_pipeline(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    filter_docs: Option<&HashSet<DocId>>,
) -> Vec<LiteralMatch> {
    let literal_len = literal.to_lowercase().len();

    // Brique 1: FST candidates
    let fst_cands = fst_candidates(sfx_reader, literal);

    // Brique 2: resolve single-token matches
    let mut matches = resolve_candidates(&fst_cands, literal_len, resolver, filter_docs);

    // Brique 3: cross-token falling walk (if single-token found nothing)
    if matches.is_empty() {
        let chains = cross_token_falling_walk(sfx_reader, literal, 0, ord_to_term);

        // Brique 4: resolve chains
        let cross_matches = resolve_chains(&chains, literal_len, resolver, filter_docs);
        matches.extend(cross_matches);
    }

    matches
}

// ─────────────────────────────────────────────────────────────────────
// resolve_token_for_multi — unified per-token resolution for multi-token
// ─────────────────────────────────────────────────────────────────────

/// A resolved posting for multi-token search, with span for cross-token chains.
#[derive(Debug, Clone)]
pub struct MultiTokenMatch {
    pub doc_id: u32,
    pub token_index: u32,
    pub span: u32,
    pub byte_from: u32,
    pub byte_to: u32,
}

/// Resolve one query token for multi-token search using the pipeline briques.
///
/// Single source of truth: the same code path as single-token contains,
/// with additional filters for multi-token constraints:
/// - `is_first`: if false, only SI=0 matches (token must start at indexed token boundary)
/// - `is_last`: if true, prefix matches allowed (query can match start of token)
///              if false, query must cover token to the end (si + query_len == token_len)
///
/// `resolve_fn` maps raw ordinal → posting entries (doc_id, position, byte_from, byte_to).
///
/// Returns postings with span (1 for single-token, N for cross-token chains).
pub fn resolve_token_for_multi<R>(
    sfx_reader: &SfxFileReader<'_>,
    query_token: &str,
    resolve_fn: &R,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
    is_first: bool,
    is_last: bool,
    fuzzy_distance: u8,
) -> Vec<MultiTokenMatch>
where
    R: Fn(u64) -> Vec<(u32, u32, u32, u32)>, // (doc_id, position, byte_from, byte_to)
{
    let query_lower = query_token.to_lowercase();
    let query_len = query_lower.len();
    let require_si0 = !is_first;

    let mut results: Vec<MultiTokenMatch> = Vec::new();

    // ── Single-token path ──
    // d=0: fst_candidates (prefix_walk) → filter
    // d>0: fuzzy_walk → filter (SI=0 only for non-last, full token match)

    let filtered_cands: Vec<FstCandidate> = if fuzzy_distance > 0 {
        // Fuzzy: use fuzzy_walk which returns (suffix, parents) with edit distance
        let fuzzy_results = if require_si0 {
            sfx_reader.fuzzy_walk_si0(&query_lower, fuzzy_distance)
        } else {
            sfx_reader.fuzzy_walk(&query_lower, fuzzy_distance)
        };
        let mut cands = Vec::new();
        for (_suffix, parents) in &fuzzy_results {
            for parent in parents {
                if require_si0 && parent.si != 0 { continue; }
                // Non-last fuzzy: must be full-token match (SI=0)
                if !is_last && parent.si != 0 { continue; }
                cands.push(FstCandidate {
                    raw_ordinal: parent.raw_ordinal,
                    si: parent.si,
                    token_len: parent.token_len,
                });
            }
        }
        cands
    } else {
        // Exact: prefix_walk → filter
        let fst_cands = fst_candidates(sfx_reader, &query_lower);
        fst_cands.into_iter().filter(|c| {
            if require_si0 && c.si != 0 { return false; }
            if !is_last && (c.si as usize + query_len != c.token_len as usize) { return false; }
            true
        }).collect()
    };

    for cand in &filtered_cands {
        let postings = resolve_fn(cand.raw_ordinal);
        for (doc_id, position, byte_from, byte_to) in &postings {
            results.push(MultiTokenMatch {
                doc_id: *doc_id,
                token_index: *position,
                span: 1,
                byte_from: *byte_from + cand.si as u32,
                byte_to: if is_last {
                    *byte_from + cand.si as u32 + query_len as u32
                } else {
                    *byte_to
                },
            });
        }
    }

    // ── Cross-token path: falling_walk → sibling chain → resolve ──

    if let Some(get_term) = ord_to_term {
        let chains = cross_token_falling_walk(sfx_reader, &query_lower, fuzzy_distance, get_term);

        let chains: Vec<_> = if require_si0 {
            chains.into_iter().filter(|c| c.first_si == 0).collect()
        } else {
            chains
        };

        for chain in &chains {
            let span = chain.ordinals.len() as u32;
            if span < 2 { continue; }

            let first_postings = resolve_fn(chain.ordinals[0]);
            let mut active: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
            for (doc_id, position, byte_from, byte_to) in &first_postings {
                let bf = *byte_from + chain.first_si as u32;
                active.push((*doc_id, *position + 1, *byte_to, bf, *position));
            }

            for &ord in &chain.ordinals[1..] {
                if active.is_empty() { break; }
                let postings = resolve_fn(ord);

                active.sort_by_key(|a| (a.0, a.1));
                let mut next_active: Vec<(u32, u32, u32, u32, u32)> = Vec::new();

                for (doc_id, position, byte_from, _byte_to) in &postings {
                    let target = (*doc_id, *position);
                    let idx = active.partition_point(|a| (a.0, a.1) < target);
                    let mut i = idx;
                    while i < active.len() && active[i].0 == *doc_id && active[i].1 == *position {
                        if *byte_from == active[i].2 {
                            next_active.push((*doc_id, *position + 1, _byte_to.clone(), active[i].3, active[i].4));
                        }
                        i += 1;
                    }
                }
                active = next_active;
            }

            for &(doc_id, _, byte_to, match_bf, first_pos) in &active {
                results.push(MultiTokenMatch {
                    doc_id,
                    token_index: first_pos,
                    span,
                    byte_from: match_bf,
                    byte_to,
                });
            }
        }
    }

    results.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.token_index.cmp(&b.token_index)));
    results.dedup_by(|a, b| a.doc_id == b.doc_id && a.token_index == b.token_index);
    results
}

/// Estimate the selectivity of a literal: how many FST entries + cross-token
/// chains match. Lower = more selective. Very cheap (no posting resolve).
#[allow(dead_code)]
pub fn estimate_selectivity(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> usize {
    let fst_count = fst_candidates(sfx_reader, literal).len();
    let ct_count = cross_token_falling_walk(sfx_reader, literal, 0, ord_to_term).len();
    fst_count + ct_count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fst_candidates_empty() {
        // Smoke test — no real SFX reader, just verify types compile
    }
}
