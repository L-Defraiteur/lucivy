//! Suffix FST contains search — substring matching via suffix FST.
//!
//! Uses the .sfx file (suffix FST with posting lists) to resolve
//! contains queries without stored text verification.

use crate::docset::{DocSet, TERMINATED};
use crate::index::InvertedIndexReader;
use crate::postings::Postings;
use crate::schema::IndexRecordOption;
use crate::suffix_fst::builder::ParentEntry;
use crate::suffix_fst::file::SfxFileReader;

/// A single contains match result.
#[derive(Debug, Clone)]
pub struct SuffixContainsMatch {
    /// Document ID within the segment.
    pub doc_id: u32,
    /// Token index of the matched token in the document.
    #[allow(dead_code)]
    pub token_index: u32,
    /// Byte offset in the original text where the match starts.
    pub byte_from: usize,
    /// Byte offset where the match ends (byte_from + query_len).
    pub byte_to: usize,
    /// The parent token text (from ._raw FST). Empty if not resolved.
    #[allow(dead_code)]
    pub parent_term: String,
    /// Suffix index (0 = full token match, >0 = substring).
    #[allow(dead_code)]
    pub si: u16,
}

/// Search for single-token contains matches using the suffix FST.
///
/// This is the core search function — walks the .sfx FST with a prefix query,
/// resolves parents to ._raw posting lists, and returns matches.
///
/// `raw_term_resolver` is a callback that maps a raw_ordinal to its posting list
/// entries: Vec<(doc_id, token_index, byte_from, byte_to)>.
/// This decouples the search from the posting list implementation.
pub fn suffix_contains_single_token<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_single_token_with_terms(sfx_reader, query, &raw_term_resolver, None)
}

/// Like suffix_contains_single_token but with optional ord_to_term for sibling-link cross-token.
pub fn suffix_contains_single_token_with_terms<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: &F,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    let matches = suffix_contains_single_token_inner(sfx_reader, query, raw_term_resolver, false, false, None);
    if !matches.is_empty() {
        return matches;
    }
    // Fallback: query may span across token boundaries.
    cross_token_search_with_terms(sfx_reader, query, raw_term_resolver, 0, ord_to_term)
}

pub fn suffix_contains_single_token_continuation<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_single_token_inner(sfx_reader, query, &raw_term_resolver, false, true, None)
}

/// Like `suffix_contains_single_token_continuation` but with a stored text verifier
/// for deep continuation (depth 3+). The verifier receives (doc_id, byte_from, remaining_query)
/// and returns true if the stored text confirms the match.
#[allow(dead_code)]
pub fn suffix_contains_single_token_continuation_with_store<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
    store_verifier: &dyn Fn(u32, usize, &str) -> bool,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_single_token_inner(sfx_reader, query, &raw_term_resolver, false, true, Some(store_verifier))
}

/// Like `suffix_contains_single_token` but only matches tokens that START
/// with the query (SI=0 filter). Used for prefix/startsWith queries.
pub fn suffix_contains_single_token_prefix<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_single_token_inner(sfx_reader, query, &raw_term_resolver, true, false, None)
}

fn suffix_contains_single_token_inner<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: &F,
    prefix_only: bool,
    continuation: bool,
    store_verifier: Option<&dyn Fn(u32, usize, &str) -> bool>,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    let query_lower = query.to_lowercase();
    let query_len = query_lower.len();

    // Use the partitioned FST: si0 for startsWith, all for contains.
    let walk_results = if prefix_only {
        sfx_reader.prefix_walk_si0(&query_lower)
    } else {
        sfx_reader.prefix_walk(&query_lower)
    };

    let mut matches: Vec<SuffixContainsMatch> = Vec::new();

    for (_suffix_term, parents) in &walk_results {
        for parent in parents {
            // Resolve parent ordinal to posting list entries
            let postings = raw_term_resolver(parent.raw_ordinal);

            crate::diag_emit!(crate::diag::DiagEvent::SfxResolve {
                query: query.to_string(),
                segment_id: String::new(),
                ordinal: parent.raw_ordinal as u32,
                token: _suffix_term.clone(),
                si: parent.si,
                doc_count: postings.len(),
            });

            for entry in &postings {
                matches.push(SuffixContainsMatch {
                    doc_id: entry.doc_id,
                    token_index: entry.token_index,
                    byte_from: entry.byte_from as usize + parent.si as usize,
                    byte_to: entry.byte_from as usize + parent.si as usize + query_len,
                    parent_term: String::new(), // resolved later if needed
                    si: parent.si,
                });
            }
        }
    }

    // Cross-token continuation: detect partial matches where a TOKEN is a
    // suffix-prefix of the query. Two sources of candidates:
    // 1. Walk 1 entries where match extends past token end (si + query_len > token_len)
    // 2. Tokens that END with a prefix of the query (token is shorter than query)
    //    → found by walking prefixes of the query: "sched" → check "sche", "sch", etc.
    if continuation && !prefix_only && query_len >= 2 {
        let gapmap = sfx_reader.gapmap();

        let mut candidates: std::collections::HashMap<
            usize, Vec<(u32, u32, usize)> // consumed → (doc_id, token_index, byte_from)
        > = std::collections::HashMap::new();

        // Source 1: walk 1 entries that extend past token boundary
        for (_suffix_term, parents) in &walk_results {
            for parent in parents {
                let postings = raw_term_resolver(parent.raw_ordinal);
                for entry in &postings {
                    let token_byte_len = (entry.byte_to - entry.byte_from) as usize;
                    let match_end = parent.si as usize + query_len;
                    if match_end > token_byte_len {
                        let consumed = token_byte_len - parent.si as usize;
                        if consumed > 0 && consumed < query_len {
                            candidates.entry(consumed).or_default().push((
                                entry.doc_id,
                                entry.token_index,
                                entry.byte_from as usize + parent.si as usize,
                            ));
                        }
                    }
                }
            }
        }

        // Source 2: tokens ending with a prefix of the query.
        // For each prefix length k (1..query_len), check if a token ENDS with query[..k].
        // "sched" → check tokens ending with "s", "sc", "sch", "sche"
        // A token ends with X if X is a suffix of the token → prefix_walk(X) with
        // si + len(X) == token_len.
        for k in 1..query_len {
            if !query_lower.is_char_boundary(k) { continue; }
            let prefix = &query_lower[..k];
            // Find tokens that have `prefix` as a suffix (at end of token)
            let prefix_walk = sfx_reader.prefix_walk(prefix);
            for (_key, parents) in &prefix_walk {
                for parent in parents {
                    let postings = raw_term_resolver(parent.raw_ordinal);
                    for entry in &postings {
                        let token_byte_len = (entry.byte_to - entry.byte_from) as usize;
                        // Check: prefix at the END of the token
                        if parent.si as usize + k == token_byte_len {
                            candidates.entry(k).or_default().push((
                                entry.doc_id,
                                entry.token_index,
                                entry.byte_from as usize + parent.si as usize,
                            ));
                        }
                    }
                }
            }
        }

        // Continuation loop (supports N-depth token chains)
        let mut depth_candidates = candidates;
        for depth in 0..8 {
            if depth_candidates.is_empty() { break; }

            // Depth 3+: fallback to stored text verification if available.
            // At this point we've done 2-3 selective FST walks, few candidates remain.
            // Reading stored text is O(1) per candidate (mmap), much cheaper than more walks.
            if depth >= 3 {
                if let Some(verify) = &store_verifier {
                    for (&consumed, entries) in &depth_candidates {
                        if consumed >= query_len { continue; }
                        let remaining = &query_lower[consumed..];
                        for &(doc_id, _ti, byte_from) in entries {
                            if verify(doc_id, byte_from, remaining) {
                                matches.push(SuffixContainsMatch {
                                    doc_id,
                                    token_index: 0,
                                    byte_from,
                                    byte_to: byte_from + query_len,
                                    parent_term: String::new(),
                                    si: 0,
                                });
                            }
                        }
                    }
                    break;
                }
            }
            let mut next_candidates: std::collections::HashMap<
                usize, Vec<(u32, u32, usize)>
            > = std::collections::HashMap::new();

            for (&consumed, entries) in &depth_candidates {
                if consumed >= query_len { continue; }
                let remaining = &query_lower[consumed..];

                // Walk 2: tokens starting with `remaining` (or a prefix of it)
                let cont_walk = sfx_reader.prefix_walk_si0(remaining);
                let mut full_match: std::collections::HashMap<(u32, u32), usize> =
                    std::collections::HashMap::new();
                let mut partial_match: std::collections::HashMap<(u32, u32), usize> =
                    std::collections::HashMap::new();

                for (_key, parents) in &cont_walk {
                    for p in parents {
                        if p.si != 0 { continue; }
                        for e in raw_term_resolver(p.raw_ordinal) {
                            let token_len = (e.byte_to - e.byte_from) as usize;
                            if token_len >= remaining.len() {
                                // Token covers all of remaining → full match
                                full_match.insert(
                                    (e.doc_id, e.token_index),
                                    e.byte_from as usize + remaining.len(),
                                );
                            } else {
                                // Token shorter than remaining → needs more continuation
                                partial_match.insert(
                                    (e.doc_id, e.token_index),
                                    token_len,
                                );
                            }
                        }
                    }
                }

                // Join candidates with continuation results
                for &(doc_id, left_ti, byte_from) in entries {
                    let right_ti = left_ti + 1;
                    let gap_ok = gapmap.read_separator(doc_id, left_ti, right_ti)
                        .map_or(false, |sep| sep.is_empty());
                    if !gap_ok { continue; }

                    if let Some(&byte_to) = full_match.get(&(doc_id, right_ti)) {
                        matches.push(SuffixContainsMatch {
                            doc_id, token_index: left_ti,
                            byte_from, byte_to,
                            parent_term: String::new(), si: 0,
                        });
                    } else if let Some(&tok_len) = partial_match.get(&(doc_id, right_ti)) {
                        next_candidates.entry(consumed + tok_len).or_default().push((
                            doc_id, right_ti, byte_from,
                        ));
                    }
                }
            }
            depth_candidates = next_candidates;
        }
    }

    // Sort by (doc_id, byte_from) for consistent output
    matches.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.byte_from.cmp(&b.byte_from)));

    // Deduplicate same (doc_id, byte_from) matches
    matches.dedup_by(|a, b| a.doc_id == b.doc_id && a.byte_from == b.byte_from);

    matches
}

/// Search for single-token fuzzy contains matches using the suffix FST.
///
/// Like `suffix_contains_single_token` but uses Levenshtein DFA with the given
/// edit distance. Matches suffix terms within `distance` edits of the query.
pub fn suffix_contains_single_token_fuzzy<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    distance: u8,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    // Cross-token search is a superset: fuzzy_falling_walk finds both single-token
    // matches (prefix_len == query.len, no sibling chain) and cross-token matches.
    cross_token_search(sfx_reader, query, &raw_term_resolver, distance)
}

/// Like fuzzy but prefix_only (SI=0 filter).
pub fn suffix_contains_single_token_fuzzy_prefix<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    distance: u8,
    raw_term_resolver: F,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_single_token_fuzzy_inner(sfx_reader, query, distance, &raw_term_resolver, true)
}

fn suffix_contains_single_token_fuzzy_inner<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    distance: u8,
    raw_term_resolver: &F,
    prefix_only: bool,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    if distance == 0 {
        return suffix_contains_single_token_inner(sfx_reader, query, &raw_term_resolver, prefix_only, false, None);
    }

    let query_lower = query.to_lowercase();
    let query_len = query_lower.len();

    // Fuzzy walk: partitioned by prefix byte.
    let walk_results = if prefix_only {
        sfx_reader.fuzzy_walk_si0(&query_lower, distance)
    } else {
        sfx_reader.fuzzy_walk(&query_lower, distance)
    };

    let mut matches: Vec<SuffixContainsMatch> = Vec::new();

    for (_suffix_term, parents) in &walk_results {
        for parent in parents {
            let postings = raw_term_resolver(parent.raw_ordinal);

            for entry in &postings {
                matches.push(SuffixContainsMatch {
                    doc_id: entry.doc_id,
                    token_index: entry.token_index,
                    byte_from: entry.byte_from as usize + parent.si as usize,
                    byte_to: entry.byte_from as usize + parent.si as usize + query_len,
                    parent_term: String::new(),
                    si: parent.si,
                });
            }
        }
    }

    matches.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.byte_from.cmp(&b.byte_from)));
    matches.dedup_by(|a, b| a.doc_id == b.doc_id && a.byte_from == b.byte_from);

    matches
}

/// Check if two separator byte slices are within Levenshtein distance `max_distance`.
fn separator_matches_fuzzy(actual: &[u8], expected: &[u8], max_distance: u8) -> bool {
    if actual == expected {
        return true;
    }
    if max_distance == 0 {
        return false;
    }
    // Simple byte-level edit distance for short separators
    let a = actual;
    let b = expected;
    let len_a = a.len();
    let len_b = b.len();
    // Quick reject: if length difference > max_distance, can't match
    if (len_a as isize - len_b as isize).unsigned_abs() > max_distance as usize {
        return false;
    }
    // For very short separators (typical case), compute exact edit distance
    if len_a <= 8 && len_b <= 8 {
        let mut dp = [[0u8; 9]; 9];
        for i in 0..=len_a { dp[i][0] = i as u8; }
        for j in 0..=len_b { dp[0][j] = j as u8; }
        for i in 1..=len_a {
            for j in 1..=len_b {
                let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
                dp[i][j] = dp[i-1][j-1].saturating_add(cost)
                    .min(dp[i-1][j].saturating_add(1))
                    .min(dp[i][j-1].saturating_add(1));
            }
        }
        dp[len_a][len_b] <= max_distance
    } else {
        // Long separators: just check exact match (rare case)
        false
    }
}

/// Check if `query` fuzzy-matches a PREFIX of `text` within edit distance `max_distance`.
///
/// Returns true if there exists a prefix `text[0..k]` such that
/// `levenshtein(query, text[0..k]) <= max_distance`.
///
/// Used for fuzzy terminal matching in cross-token search: the query remainder
/// is compared against the beginning of the next token.
fn levenshtein_prefix_match(query: &str, text: &str, max_distance: u8) -> bool {
    if max_distance == 0 {
        return text.starts_with(query);
    }
    let q = query.as_bytes();
    let t = text.as_bytes();
    let qlen = q.len();
    let tlen = t.len();

    // DP matrix: dp[i][j] = edit distance between query[0..i] and text[0..j]
    // We want min(dp[qlen][0..=tlen]) <= max_distance
    // Optimization: only keep two rows (previous and current)
    let max_j = tlen.min(qlen + max_distance as usize); // no need to go further
    let mut prev = vec![0u16; max_j + 1];
    let mut curr = vec![0u16; max_j + 1];

    // Row 0: dp[0][j] = 0 for all j (empty query matches any prefix with 0 deletions)
    // Wait — dp[0][j] = j (need j deletions to match empty query against text[0..j])
    // Actually for PREFIX match: dp[0][j] = 0 for all j? No.
    // Standard Levenshtein: dp[0][j] = j. But for prefix match, we want the minimum
    // of the LAST row, not dp[qlen][tlen]. So we use standard DP.
    for j in 0..=max_j { prev[j] = j as u16; }

    for i in 1..=qlen {
        curr[0] = i as u16;
        let mut row_min = curr[0];
        for j in 1..=max_j {
            let cost = if q[i - 1] == t[j - 1] { 0u16 } else { 1 };
            curr[j] = (prev[j - 1] + cost)
                .min(prev[j] + 1)
                .min(curr[j - 1] + 1);
            row_min = row_min.min(curr[j]);
        }
        // Early termination: if the entire row is > max_distance, no prefix can match
        if row_min > max_distance as u16 {
            return false;
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    // Check: min(dp[qlen][0..=max_j]) <= max_distance
    prev[..=max_j].iter().any(|&d| d <= max_distance as u16)
}

/// A posting list entry from the ._raw field.
#[derive(Debug, Clone)]
pub struct RawPostingEntry {
    pub doc_id: u32,
    pub token_index: u32,
    pub byte_from: u32,
    pub byte_to: u32,
}

/// Resolve a raw_ordinal to its posting entries using the real inverted index.
///
/// This reads the ._raw posting list for the term at the given ordinal,
/// extracting (doc_id, position, byte_from, byte_to) for each occurrence.
#[allow(dead_code)]
pub fn resolve_raw_ordinal(
    inv_idx_reader: &InvertedIndexReader,
    raw_ordinal: u64,
) -> Vec<RawPostingEntry> {
    let term_dict = inv_idx_reader.terms();
    let term_info = term_dict.term_info_from_ord(raw_ordinal);

    let mut postings = match inv_idx_reader.read_postings_from_terminfo(
        &term_info,
        IndexRecordOption::WithFreqsAndPositionsAndOffsets,
    ) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut entries = Vec::new();
    loop {
        let doc_id = postings.doc();
        if doc_id == TERMINATED {
            break;
        }
        let mut pos_offsets = Vec::new();
        postings.append_positions_and_offsets(0, &mut pos_offsets);
        for (position, byte_from, byte_to) in pos_offsets {
            entries.push(RawPostingEntry {
                doc_id,
                token_index: position,
                byte_from,
                byte_to,
            });
        }
        postings.advance();
    }
    entries
}

/// A resolved posting for multi-token search, with span for cross-token chains.
#[derive(Debug, Clone)]
struct MultiTokenPosting {
    doc_id: u32,
    token_index: u32,  // position of the FIRST token in the chain
    span: u32,         // number of indexed positions occupied (1 = single token, N = chain)
    byte_from: u32,
    byte_to: u32,
}

/// Resolve a sub-token via falling_walk + sibling chain for multi-token search.
///
/// Returns all valid postings with their spans. This covers both single-token
/// matches (span=1) and cross-token chains (span=N) in one pass.
fn cross_token_resolve_for_multi<F>(
    sfx_reader: &SfxFileReader<'_>,
    sub_token: &str,
    raw_term_resolver: &F,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
    is_first: bool,
    is_last: bool,
    prefix_only: bool,
    fuzzy_distance: u8,
) -> Vec<MultiTokenPosting>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    let query_lower = sub_token.to_lowercase();
    let use_si0 = prefix_only || !is_first;

    let sibling_table = sfx_reader.sibling_table();
    let mut results: Vec<MultiTokenPosting> = Vec::new();

    // For last token (or any token): prefix_walk finds tokens that START with the query.
    // This handles "plan" matching "planner", "planning", etc.
    // For non-last tokens, resolve_suffix finds exact matches.
    if is_last {
        let prefix_results = if fuzzy_distance > 0 {
            if use_si0 { sfx_reader.fuzzy_walk_si0(&query_lower, fuzzy_distance) }
            else { sfx_reader.fuzzy_walk(&query_lower, fuzzy_distance) }
        } else {
            if use_si0 { sfx_reader.prefix_walk_si0(&query_lower) }
            else { sfx_reader.prefix_walk(&query_lower) }
        };
        for (_suffix, parents) in &prefix_results {
            for parent in parents {
                if use_si0 && parent.si != 0 { continue; }
                let postings = raw_term_resolver(parent.raw_ordinal);
                for p in &postings {
                    let bf = p.byte_from + parent.si as u32;
                    results.push(MultiTokenPosting {
                        doc_id: p.doc_id,
                        token_index: p.token_index,
                        span: 1,
                        byte_from: bf,
                        byte_to: bf + query_lower.len() as u32,
                    });
                }
            }
        }
    } else if fuzzy_distance > 0 {
        // Non-last fuzzy: fuzzy_walk finds tokens within edit distance
        let fuzzy_results = if use_si0 {
            sfx_reader.fuzzy_walk_si0(&query_lower, fuzzy_distance)
        } else {
            sfx_reader.fuzzy_walk(&query_lower, fuzzy_distance)
        };
        for (_suffix, parents) in &fuzzy_results {
            for parent in parents {
                if use_si0 && parent.si != 0 { continue; }
                // Must be a full-token match (SI=0, covers entire token)
                if parent.si != 0 { continue; }
                let postings = raw_term_resolver(parent.raw_ordinal);
                for p in &postings {
                    results.push(MultiTokenPosting {
                        doc_id: p.doc_id,
                        token_index: p.token_index,
                        span: 1,
                        byte_from: p.byte_from,
                        byte_to: p.byte_to,
                    });
                }
            }
        }
    } else {
        // Non-last exact: resolve_suffix — the query must match a full indexed token
        let resolved = if use_si0 {
            sfx_reader.resolve_suffix_si0(&query_lower)
        } else {
            sfx_reader.resolve_suffix(&query_lower)
        };
        for parent in &resolved {
            // Only exact full-token match: si + query_len == token_len
            if parent.si as usize + query_lower.len() != parent.token_len as usize {
                continue;
            }
            let postings = raw_term_resolver(parent.raw_ordinal);
            for p in &postings {
                results.push(MultiTokenPosting {
                    doc_id: p.doc_id,
                    token_index: p.token_index,
                    span: 1,
                    byte_from: p.byte_from + parent.si as u32,
                    byte_to: p.byte_to,
                });
            }
        }
    }

    // falling_walk + sibling chain: find cross-token matches.
    // Fuzzy falling_walk finds the first split despite typos in the left part.
    // The sibling chain after handles the rest efficiently (O(1) per step).
    let candidates = if fuzzy_distance > 0 {
        sfx_reader.fuzzy_falling_walk(&query_lower, fuzzy_distance)
    } else {
        sfx_reader.falling_walk(&query_lower)
    };

    for cand in &candidates {
        // Skip non-SI=0 candidates when required (middle/last tokens)
        if use_si0 && cand.parent.si != 0 {
            continue;
        }

        // In fuzzy mode, fst_depth (prefix_len) can exceed query length due to insertions.
        let split_at = cand.prefix_len.min(query_lower.len());
        let remainder = &query_lower[split_at..];

        if remainder.is_empty() {
            // Already handled above (exact match or prefix match)
            continue;
        }

        // remainder is non-empty → need sibling chain
        if let (Some(sib_table), Some(get_term)) = (sibling_table, &ord_to_term) {
            // Follow sibling chain
            let mut current_ord = cand.parent.raw_ordinal;
            let mut rem = remainder;
            let mut chain_ords: Vec<u64> = vec![current_ord];

            const MAX_CHAIN: usize = 8;
            let mut found = false;

            for _ in 0..MAX_CHAIN {
                if rem.is_empty() { found = true; break; }

                let siblings = sib_table.contiguous_siblings(current_ord as u32);
                let mut matched = false;

                for next_ord in &siblings {
                    let next_text = match get_term(*next_ord as u64) {
                        Some(t) => t,
                        None => continue,
                    };

                    if rem == next_text {
                        // Exact full token match
                        chain_ords.push(*next_ord as u64);
                        rem = "";
                        found = true;
                        matched = true;
                        break;
                    } else if rem.starts_with(&next_text) {
                        // Full token consumed → continue chain
                        chain_ords.push(*next_ord as u64);
                        rem = &rem[next_text.len()..];
                        current_ord = *next_ord as u64;
                        matched = true;
                        break;
                    } else if is_last && (
                        next_text.starts_with(rem) ||
                        (fuzzy_distance > 0 && levenshtein_prefix_match(rem, &next_text, fuzzy_distance))
                    ) {
                        // Last token: remainder matches prefix of next token (exact or fuzzy)
                        chain_ords.push(*next_ord as u64);
                        found = true;
                        matched = true;
                        break;
                    }
                }

                if !matched || found { break; }
            }

            if found {
                let span = chain_ords.len() as u32;

                // Resolve chain: verify adjacency + byte continuity
                let first_postings = raw_term_resolver(chain_ords[0]);
                let mut active: Vec<(u32, u32, u32, u32, u32)> = Vec::new(); // (doc, next_pos, next_byte, byte_from, first_ti)
                for p in &first_postings {
                    let bf = p.byte_from + cand.parent.si as u32;
                    active.push((p.doc_id, p.token_index + 1, p.byte_to, bf, p.token_index));
                }

                for &ord in &chain_ords[1..] {
                    if active.is_empty() { break; }
                    let postings = raw_term_resolver(ord);
                    active.sort_by_key(|a| (a.0, a.1));
                    let mut next_active: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
                    for p in &postings {
                        let idx = active.partition_point(|a| (a.0, a.1) < (p.doc_id, p.token_index));
                        let mut i = idx;
                        while i < active.len() && active[i].0 == p.doc_id && active[i].1 == p.token_index {
                            if p.byte_from == active[i].2 {
                                next_active.push((p.doc_id, p.token_index + 1, p.byte_to, active[i].3, active[i].4));
                            }
                            i += 1;
                        }
                    }
                    active = next_active;
                }

                for &(doc_id, _, byte_to, byte_from, first_ti) in &active {
                    results.push(MultiTokenPosting {
                        doc_id,
                        token_index: first_ti,
                        span,
                        byte_from,
                        byte_to,
                    });
                }
            }
        } else if is_last {
            // No sibling table → fallback: prefix_walk for last token remainder
            let right_walks = if use_si0 {
                sfx_reader.prefix_walk_si0(remainder)
            } else {
                sfx_reader.prefix_walk(remainder)
            };
            for (_suffix, parents) in &right_walks {
                for rp in parents {
                    if use_si0 && rp.si != 0 { continue; }
                    // Two-token chain: cand + rp
                    let left_postings = raw_term_resolver(cand.parent.raw_ordinal);
                    let right_postings = raw_term_resolver(rp.raw_ordinal);
                    for lp in &left_postings {
                        for rp_entry in &right_postings {
                            if lp.doc_id == rp_entry.doc_id
                                && lp.token_index + 1 == rp_entry.token_index
                                && lp.byte_to == rp_entry.byte_from
                            {
                                results.push(MultiTokenPosting {
                                    doc_id: lp.doc_id,
                                    token_index: lp.token_index,
                                    span: 2,
                                    byte_from: lp.byte_from + cand.parent.si as u32,
                                    byte_to: rp_entry.byte_from + remainder.len() as u32,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Deduplicate
    results.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.token_index.cmp(&b.token_index)));
    results.dedup_by(|a, b| a.doc_id == b.doc_id && a.token_index == b.token_index);
    results
}

/// Search for multi-token contains matches.
///
/// Each sub-token is resolved via falling_walk + sibling chain (covers both
/// single-token and cross-token matches). Adjacency uses spans.
///
/// `raw_ordinal_resolver` maps a raw_ordinal to its posting entries.
/// `sfx_reader` provides suffix FST access + GapMap + sibling table.
pub fn suffix_contains_multi_token<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: F,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_multi_token_impl(sfx_reader, query_tokens, query_separators, &raw_ordinal_resolver, 0, false, None)
}

/// Multi-token prefix search (startsWith). All tokens must be SI=0.
pub fn suffix_contains_multi_token_prefix<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: F,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_multi_token_impl(sfx_reader, query_tokens, query_separators, &raw_ordinal_resolver, 0, true, None)
}

/// Multi-token contains search with optional fuzzy distance.
pub fn suffix_contains_multi_token_fuzzy<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: F,
    fuzzy_distance: u8,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_multi_token_impl(sfx_reader, query_tokens, query_separators, &raw_ordinal_resolver, fuzzy_distance, false, None)
}

/// Multi-token fuzzy prefix search.
pub fn suffix_contains_multi_token_fuzzy_prefix<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: F,
    fuzzy_distance: u8,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_multi_token_impl(sfx_reader, query_tokens, query_separators, &raw_ordinal_resolver, fuzzy_distance, true, None)
}

/// Multi-token search with ord_to_term for sibling-link cross-token on sub-tokens.
pub fn suffix_contains_multi_token_impl_pub<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: F,
    fuzzy_distance: u8,
    prefix_only: bool,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    suffix_contains_multi_token_impl(sfx_reader, query_tokens, query_separators, &raw_ordinal_resolver, fuzzy_distance, prefix_only, ord_to_term)
}

fn suffix_contains_multi_token_impl<F>(
    sfx_reader: &SfxFileReader<'_>,
    query_tokens: &[&str],
    query_separators: &[&str],
    raw_ordinal_resolver: &F,
    fuzzy_distance: u8,
    prefix_only: bool,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
) -> Vec<SuffixContainsMultiMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    if query_tokens.is_empty() {
        return Vec::new();
    }
    if query_tokens.len() == 1 {
        let results = if fuzzy_distance > 0 {
            suffix_contains_single_token_fuzzy_inner(sfx_reader, query_tokens[0], fuzzy_distance, &raw_ordinal_resolver, prefix_only)
        } else {
            suffix_contains_single_token_inner(sfx_reader, query_tokens[0], &raw_ordinal_resolver, prefix_only, false, None)
        };
        return results
            .into_iter()
            .map(|m| SuffixContainsMultiMatch {
                doc_id: m.doc_id,
                byte_from: m.byte_from,
                byte_to: m.byte_to,
                token_matches: vec![m],
            })
            .collect();
    }

    assert_eq!(
        query_separators.len(),
        query_tokens.len() - 1,
        "separators must be one less than tokens"
    );

    let n = query_tokens.len();

    // Step 1: Resolve ALL sub-tokens via falling_walk + sibling chain.
    // Each sub-token produces MultiTokenPostings with spans.
    // This covers both single-token matches (span=1) and cross-token chains (span=N).

    let mut per_token_postings: Vec<Vec<MultiTokenPosting>> = Vec::with_capacity(n);

    for (i, &token) in query_tokens.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == n - 1;

        let postings = cross_token_resolve_for_multi(
            sfx_reader, token, raw_ordinal_resolver, ord_to_term,
            is_first, is_last, prefix_only, fuzzy_distance,
        );

        if postings.is_empty() {
            return Vec::new();
        }

        per_token_postings.push(postings);
    }

    // Step 2: Pick pivot = sub-token with fewest postings (most selective).
    let pivot_idx = per_token_postings
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| p.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let pivot_doc_ids: std::collections::HashSet<u32> = per_token_postings[pivot_idx]
        .iter()
        .map(|e| e.doc_id)
        .collect();

    // Filter non-pivot postings by pivot doc_ids
    for i in 0..n {
        if i == pivot_idx { continue; }
        per_token_postings[i].retain(|e| pivot_doc_ids.contains(&e.doc_id));
        if per_token_postings[i].is_empty() {
            return Vec::new();
        }
    }

    // Sort all by (doc_id, token_index) for binary search
    for postings in &mut per_token_postings {
        postings.sort_by_key(|e| (e.doc_id, e.token_index));
    }

    // Step 3: Find chains of consecutive positions with spans.
    //
    // For each pivot posting, compute cumulative offsets using spans,
    // then verify that each sub-token has a posting at the expected position.

    let mut matches: Vec<SuffixContainsMultiMatch> = Vec::new();
    let gapmap = sfx_reader.gapmap();

    for pivot_entry in &per_token_postings[pivot_idx] {
        let doc_id = pivot_entry.doc_id;
        let pivot_ti = pivot_entry.token_index;

        // We need to find first_ti such that the cumulative span from token 0
        // to pivot_idx lands on pivot_ti.
        // Try: assume all tokens before pivot have span from their posting.
        // We search backward from pivot to find first_ti.

        // Collect chain: for each sub-token, find a posting at the expected position.
        // Start from pivot and extend in both directions.
        let mut chain: Vec<Option<MultiTokenPosting>> = vec![None; n];
        chain[pivot_idx] = Some(pivot_entry.clone());
        let mut valid = true;

        // Backward: find postings for tokens before pivot
        let mut expected_ti = pivot_ti;
        for step in (0..pivot_idx).rev() {
            // The token at `step` must end at expected_ti (its span reaches expected_ti)
            // So we look for a posting where token_index + span == expected_ti
            let found = per_token_postings[step].iter().find(|e| {
                e.doc_id == doc_id && e.token_index + e.span == expected_ti
            });
            match found {
                Some(entry) => {
                    expected_ti = entry.token_index;
                    chain[step] = Some(entry.clone());
                }
                None => { valid = false; break; }
            }
        }

        if !valid { continue; }

        // Forward: find postings for tokens after pivot
        expected_ti = pivot_ti + pivot_entry.span;
        for step in (pivot_idx + 1)..n {
            let found = per_token_postings[step].iter().find(|e| {
                e.doc_id == doc_id && e.token_index == expected_ti
            });
            match found {
                Some(entry) => {
                    expected_ti = entry.token_index + entry.span;
                    chain[step] = Some(entry.clone());
                }
                None => { valid = false; break; }
            }
        }

        if !valid { continue; }

        // Validate separators via GapMap
        let all_seps_empty = query_separators.iter().all(|s| s.is_empty());
        if !all_seps_empty {
            let mut seps_valid = true;
            for sep_idx in 0..query_separators.len() {
                let left = chain[sep_idx].as_ref().unwrap();
                let right = chain[sep_idx + 1].as_ref().unwrap();
                // Separator is between the LAST position of left and FIRST position of right
                let ti_a = left.token_index + left.span - 1;
                let ti_b = right.token_index;
                let expected_sep = query_separators[sep_idx].as_bytes();

                match gapmap.read_separator(doc_id, ti_a, ti_b) {
                    Some(actual_sep) => {
                        if !separator_matches_fuzzy(actual_sep, expected_sep, fuzzy_distance) {
                            seps_valid = false;
                            break;
                        }
                    }
                    None => { seps_valid = false; break; }
                }
            }
            if !seps_valid { continue; }
        }

        // Build result
        let chain: Vec<MultiTokenPosting> = chain.into_iter().map(|c| c.unwrap()).collect();
        let first = &chain[0];
        let last = &chain[n - 1];
        let byte_from = first.byte_from as usize;
        let last_query_token = query_tokens[n - 1].to_lowercase();
        let byte_to = last.byte_from as usize + last_query_token.len();

        let token_matches = chain.iter().map(|entry| {
            SuffixContainsMatch {
                doc_id: entry.doc_id,
                token_index: entry.token_index,
                byte_from: entry.byte_from as usize,
                byte_to: entry.byte_to as usize,
                parent_term: String::new(),
                si: 0,
            }
        }).collect();

        matches.push(SuffixContainsMultiMatch {
            doc_id,
            byte_from,
            byte_to,
            token_matches,
        });
    }

    // Deduplicate by (doc_id, byte_from)
    matches.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.byte_from.cmp(&b.byte_from)));
    matches.dedup_by(|a, b| a.doc_id == b.doc_id && a.byte_from == b.byte_from);

    matches
}

/// A multi-token contains match.
#[derive(Debug, Clone)]
pub struct SuffixContainsMultiMatch {
    pub doc_id: u32,
    pub byte_from: usize,
    pub byte_to: usize,
    #[allow(dead_code)]
    pub token_matches: Vec<SuffixContainsMatch>,
}

// ── Cross-token search ───────────────────────────────────────────────────────

/// Search for a query that may span across indexed token boundaries.
///
/// Uses sibling links (pre-computed at indexation) to follow token chains.
/// Falls back to single-split prefix_walk if no sibling table or no ord_to_term.
///
/// Algorithm:
/// 1. falling_walk(query) → first-split candidates (any SI)
/// 2. For each candidate at a token boundary, follow sibling links:
///    - Get next_ordinal from sibling table (O(1) lookup)
///    - Get token text via ord_to_term (O(log N) term dict)
///    - Check if remainder starts with that text → chain or terminal match
/// 3. Resolve only the ordinals in valid chains → verify adjacency
pub fn cross_token_search<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: &F,
    fuzzy_distance: u8,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    cross_token_search_with_terms(sfx_reader, query, raw_term_resolver, fuzzy_distance, None)
}

/// Cross-token search with optional ord_to_term for sibling link chain walk.
pub fn cross_token_search_with_terms<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: &F,
    fuzzy_distance: u8,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
) -> Vec<SuffixContainsMatch>
where
    F: Fn(u64) -> Vec<RawPostingEntry>,
{
    let query_lower = query.to_lowercase();
    let query_bytes = query_lower.as_bytes();

    // Step 1: falling_walk → first-split candidates.
    // Fuzzy falling_walk finds splits despite typos in the left part.
    let candidates = if fuzzy_distance > 0 {
        sfx_reader.fuzzy_falling_walk(&query_lower, fuzzy_distance)
    } else {
        sfx_reader.falling_walk(&query_lower)
    };
    eprintln!("[ct_wt] query='{}' d={} candidates={}", query_lower, fuzzy_distance, candidates.len());

    if candidates.is_empty() {
        return Vec::new();
    }

    let sibling_table = sfx_reader.sibling_table();
    let has_siblings = sibling_table.is_some() && ord_to_term.is_some();

    // Step 2: For each candidate, try to cover the remainder via sibling chain
    // A valid chain: sequence of ordinals [first, ..., terminal] where each
    // token's text exactly covers a slice of the query, and the terminal
    // token's text starts with the remaining query bytes.

    const MAX_CHAIN_DEPTH: usize = 8;

    // Each valid chain: (first_candidate, chain_ordinals: Vec<u64>)
    // chain_ordinals includes the first candidate's ordinal + all chained ordinals
    let mut valid_chains: Vec<(usize, Vec<u64>)> = Vec::new(); // (cand_idx, ordinals)

    for (cand_idx, cand) in candidates.iter().enumerate() {
        let split_at = cand.prefix_len.min(query_lower.len());
        let remainder = &query_lower[split_at..];
        eprintln!("[ct_wt]   cand[{}] prefix_len={} si={} token_len={} split_at={} rem='{}'",
            cand_idx, cand.prefix_len, cand.parent.si, cand.parent.token_len, split_at, remainder);
        if remainder.is_empty() {
            // The entire query was consumed by this single token (possibly fuzzy).
            valid_chains.push((cand_idx, vec![cand.parent.raw_ordinal]));
            continue;
        }

        if has_siblings {
            let sib_table = sibling_table.unwrap();
            let get_term = ord_to_term.unwrap();

            // Follow sibling chain
            let mut current_ord = cand.parent.raw_ordinal;
            let mut rem = remainder;
            let mut chain = vec![current_ord];
            let mut found = false;

            for _ in 0..MAX_CHAIN_DEPTH {
                if rem.is_empty() { break; }

                // Get contiguous siblings of current token
                let siblings = sib_table.contiguous_siblings(current_ord as u32);
                let mut matched = false;

                for next_ord in &siblings {
                    let next_text = match get_term(*next_ord as u64) {
                        Some(t) => t,
                        None => continue,
                    };

                    if rem == next_text {
                        // Exact full token match
                        chain.push(*next_ord as u64);
                        rem = "";
                        found = true;
                        matched = true;
                        break;
                    } else if rem.starts_with(&next_text) {
                        // Full token consumed → continue chain
                        chain.push(*next_ord as u64);
                        rem = &rem[next_text.len()..];
                        current_ord = *next_ord as u64;
                        matched = true;
                        break;
                    } else if next_text.starts_with(rem) || (fuzzy_distance > 0 && levenshtein_prefix_match(rem, &next_text, fuzzy_distance)) {
                        // Remainder matches prefix of next token (exact or fuzzy) → terminal
                        chain.push(*next_ord as u64);
                        found = true;
                        matched = true;
                        break;
                    }
                }

                if !matched { break; }
                if found { break; }
            }

            if found || rem.is_empty() {
                valid_chains.push((cand_idx, chain));
            }
        } else {
            // Fallback: no sibling table → single-split with prefix_walk or fuzzy_walk
            let right_walks = if fuzzy_distance > 0 {
                sfx_reader.fuzzy_walk_si0(remainder, fuzzy_distance)
            } else {
                sfx_reader.prefix_walk_si0(remainder)
            };
            if !right_walks.is_empty() {
                for (_suffix, parents) in &right_walks {
                    for rp in parents {
                        if rp.si == 0 {
                            valid_chains.push((cand_idx, vec![cand.parent.raw_ordinal, rp.raw_ordinal]));
                        }
                    }
                }
            }
        }
    }

    // Fuzzy left-side typos are handled by fuzzy_falling_walk above.
    // No need for sibling-first iteration — the DFA finds the split despite typos,
    if valid_chains.is_empty() {
        return Vec::new();
    }

    // Step 3: Resolve only the ordinals in valid chains
    let mut ordinal_cache: std::collections::HashMap<u64, Vec<RawPostingEntry>> = std::collections::HashMap::new();
    for (_, chain) in &valid_chains {
        for &ord in chain {
            ordinal_cache.entry(ord).or_insert_with(|| raw_term_resolver(ord));
        }
    }

    // Step 4: Verify adjacency + byte continuity for each chain
    let mut results: Vec<SuffixContainsMatch> = Vec::new();

    for (cand_idx, chain) in &valid_chains {
        // For fuzzy sibling-first chains, cand_idx is usize::MAX (no falling_walk candidate).
        // The first token is SI=0 (full token). For normal chains, use the candidate's SI.
        let first_si = if *cand_idx < candidates.len() {
            candidates[*cand_idx].parent.si as usize
        } else {
            0 // fuzzy sibling-first: always SI=0
        };

        // Seed from first ordinal's postings
        let first_postings = match ordinal_cache.get(&chain[0]) {
            Some(p) => p,
            None => continue,
        };

        // active: Vec<(doc_id, next_position, next_byte_from, highlight_byte_from, first_ti)>
        let mut active: Vec<(u32, u32, u32, usize, u32)> = Vec::new();
        for p in first_postings {
            let byte_from = p.byte_from as usize + first_si;
            active.push((p.doc_id, p.token_index + 1, p.byte_to, byte_from, p.token_index));
        }

        // Chain through remaining ordinals
        for &ord in &chain[1..] {
            if active.is_empty() { break; }
            let postings = match ordinal_cache.get(&ord) {
                Some(p) => p,
                None => { active.clear(); break; }
            };

            active.sort_by_key(|a| (a.0, a.1));
            let mut next_active: Vec<(u32, u32, u32, usize, u32)> = Vec::new();
            for p in postings {
                let target = (p.doc_id, p.token_index);
                let idx = active.partition_point(|a| (a.0, a.1) < target);
                let mut i = idx;
                while i < active.len() && active[i].0 == p.doc_id && active[i].1 == p.token_index {
                    if p.byte_from == active[i].2 {
                        next_active.push((p.doc_id, p.token_index + 1, p.byte_to, active[i].3, active[i].4));
                    }
                    i += 1;
                }
            }
            active = next_active;
        }

        // Emit results
        for &(doc_id, _, _, byte_from, first_ti) in &active {
            results.push(SuffixContainsMatch {
                doc_id,
                token_index: first_ti,
                byte_from,
                byte_to: byte_from + query_bytes.len(),
                parent_term: String::new(),
                si: first_si as u16,
            });
        }
    }

    // Deduplicate
    results.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.byte_from.cmp(&b.byte_from)));
    results.dedup_by(|a, b| a.doc_id == b.doc_id && a.byte_from == b.byte_from);

    results
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::suffix_fst::SfxCollector;
    use crate::suffix_fst::file::SfxFileReader;

    /// Build a test .sfx and a fake ._raw posting resolver.
    fn build_test_index() -> (Vec<u8>, HashMap<u64, Vec<RawPostingEntry>>) {
        let mut collector = SfxCollector::new();

        // Doc 0: "import rag3db from 'rag3db_core';"
        collector.begin_doc();
        collector.begin_value("import rag3db from 'rag3db_core';");
        collector.add_token("import", 0, 6);
        collector.add_token("rag3db", 7, 13);
        collector.add_token("from", 14, 18);
        collector.add_token("rag3db", 20, 26);
        collector.add_token("core", 27, 31);
        collector.end_value();
        collector.end_doc();

        // Doc 1: "rag3db is cool"
        collector.begin_doc();
        collector.begin_value("rag3db is cool");
        collector.add_token("rag3db", 0, 6);
        collector.add_token("is", 7, 9);
        collector.add_token("cool", 10, 14);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();

        // Build fake ._raw posting lists indexed by raw_ordinal.
        // Sorted unique tokens: cool(0), core(1), from(2), import(3), is(4), rag3db(5)
        let mut raw_postings: HashMap<u64, Vec<RawPostingEntry>> = HashMap::new();

        // cool(0) → doc=0 Ti=4 (wait, not in doc0... let me recalculate)
        // Doc 0 tokens: import(Ti=0), rag3db(Ti=1), from(Ti=2), rag3db(Ti=3), core(Ti=4)
        // Doc 1 tokens: rag3db(Ti=0), is(Ti=1), cool(Ti=2)

        raw_postings.insert(0, vec![ // "cool"
            RawPostingEntry { doc_id: 1, token_index: 2, byte_from: 10, byte_to: 14 },
        ]);
        raw_postings.insert(1, vec![ // "core"
            RawPostingEntry { doc_id: 0, token_index: 4, byte_from: 27, byte_to: 31 },
        ]);
        raw_postings.insert(2, vec![ // "from"
            RawPostingEntry { doc_id: 0, token_index: 2, byte_from: 14, byte_to: 18 },
        ]);
        raw_postings.insert(3, vec![ // "import"
            RawPostingEntry { doc_id: 0, token_index: 0, byte_from: 0, byte_to: 6 },
        ]);
        raw_postings.insert(4, vec![ // "is"
            RawPostingEntry { doc_id: 1, token_index: 1, byte_from: 7, byte_to: 9 },
        ]);
        raw_postings.insert(5, vec![ // "rag3db"
            RawPostingEntry { doc_id: 0, token_index: 1, byte_from: 7, byte_to: 13 },
            RawPostingEntry { doc_id: 0, token_index: 3, byte_from: 20, byte_to: 26 },
            RawPostingEntry { doc_id: 1, token_index: 0, byte_from: 0, byte_to: 6 },
        ]);

        (sfx_bytes, raw_postings)
    }

    #[test]
    fn test_single_token_exact() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_single_token(&reader, "rag3db", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        // "rag3db" should match at SI=0 in both docs
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 7);  // first occurrence
        assert_eq!(results[0].si, 0);
        assert_eq!(results[1].doc_id, 0);
        assert_eq!(results[1].byte_from, 20); // second occurrence
        assert_eq!(results[2].doc_id, 1);
        assert_eq!(results[2].byte_from, 0);
    }

    #[test]
    fn test_single_token_substring() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "g3db" is a suffix of "rag3db" at SI=2
        let results = suffix_contains_single_token(&reader, "g3db", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 3);
        // byte_from = original byte_from + SI
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 9);   // 7 + 2
        assert_eq!(results[0].si, 2);
        assert_eq!(results[1].doc_id, 0);
        assert_eq!(results[1].byte_from, 22);  // 20 + 2
        assert_eq!(results[2].doc_id, 1);
        assert_eq!(results[2].byte_from, 2);   // 0 + 2
    }

    #[test]
    fn test_single_token_prefix_match() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "rag" is a prefix of suffix "rag3db" → prefix walk finds it
        let results = suffix_contains_single_token(&reader, "rag", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 3); // same 3 occurrences of "rag3db"
        assert_eq!(results[0].byte_from, 7);
        assert_eq!(results[0].byte_to, 10);  // 7 + len("rag")=3
    }

    #[test]
    fn test_single_token_mid_word() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "port" is a suffix of "import" at SI=2, prefix walk finds "port" in suffix FST
        let results = suffix_contains_single_token(&reader, "port", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 2);  // 0 + 2
        assert_eq!(results[0].byte_to, 6);    // 2 + len("port")=4
    }

    #[test]
    fn test_single_token_no_match() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_single_token(&reader, "xyz", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert!(results.is_empty());
    }

    #[test]
    fn test_single_token_highlights() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "ore" is suffix of "core" at SI=1
        let results = suffix_contains_single_token(&reader, "ore", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 28); // 27 + 1
        assert_eq!(results[0].byte_to, 31);   // 28 + 3
        assert_eq!(results[0].si, 1);
    }

    // ─── Multi-token tests ──────────────────────────────────────

    #[test]
    fn test_multi_token_exact() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Search "rag3db from" — two consecutive tokens in doc 0
        // Doc 0: "import rag3db from 'rag3db_core';"
        // Tokens: import(Ti=0) rag3db(Ti=1) from(Ti=2) rag3db(Ti=3) core(Ti=4)
        // Separator between Ti=1 and Ti=2 = " "
        let results = suffix_contains_multi_token(
            &reader,
            &["rag3db", "from"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert_eq!(results.len(), 1, "should find 'rag3db from' in doc 0");
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 7);  // "rag3db" starts at byte 7
        assert_eq!(results[0].byte_to, 18);   // "from" ends at byte 18
    }

    #[test]
    fn test_multi_token_wrong_separator() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Search "rag3db_from" — separator "_" doesn't match " " in the doc
        let results = suffix_contains_multi_token(
            &reader,
            &["rag3db", "from"],
            &["_"],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert!(results.is_empty(), "separator '_' doesn't match ' '");
    }

    #[test]
    fn test_multi_token_no_match() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "rag3db xyz" — "xyz" doesn't exist
        let results = suffix_contains_multi_token(
            &reader,
            &["rag3db", "xyz"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert!(results.is_empty());
    }

    #[test]
    fn test_multi_token_three_tokens() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "rag3db is cool" — all three in doc 1
        // Doc 1: "rag3db is cool"
        // Tokens: rag3db(Ti=0) is(Ti=1) cool(Ti=2)
        let results = suffix_contains_multi_token(
            &reader,
            &["rag3db", "is", "cool"],
            &[" ", " "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
        assert_eq!(results[0].byte_from, 0);
        assert_eq!(results[0].byte_to, 14); // "cool" = 4 bytes, starts at 10
    }

    #[test]
    fn test_multi_token_not_consecutive() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "import from" — these exist in doc 0 but NOT consecutive (Ti=0, Ti=2)
        let results = suffix_contains_multi_token(
            &reader,
            &["import", "from"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert!(results.is_empty(), "import and from are not consecutive tokens");
    }

    /// Fuzzy d=3: "is 3db cool" on "is rag3db cool" — middle token "3db" fuzzy
    /// matches "rag3db" (Levenshtein distance 3: insert r,a,g). Validates that
    /// fuzzy on middle tokens works with SI=0 filtering and pivot selection.
    #[test]
    fn test_multi_token_fuzzy_d3_middle_token() {
        // Build a standalone index with "is rag3db cool"
        let mut collector = SfxCollector::new();
        collector.begin_doc();
        collector.begin_value("is rag3db cool");
        collector.add_token("is", 0, 2);
        collector.add_token("rag3db", 3, 9);
        collector.add_token("cool", 10, 14);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Sorted unique tokens: cool(0), is(1), rag3db(2)
        let mut raw_postings: HashMap<u64, Vec<RawPostingEntry>> = HashMap::new();
        raw_postings.insert(0, vec![
            RawPostingEntry { doc_id: 0, token_index: 2, byte_from: 10, byte_to: 14 },
        ]);
        raw_postings.insert(1, vec![
            RawPostingEntry { doc_id: 0, token_index: 0, byte_from: 0, byte_to: 2 },
        ]);
        raw_postings.insert(2, vec![
            RawPostingEntry { doc_id: 0, token_index: 1, byte_from: 3, byte_to: 9 },
        ]);

        // "is 3db cool" with d=3 — "3db" should fuzzy match "rag3db" (distance 3)
        let results = suffix_contains_multi_token_fuzzy(
            &reader,
            &["is", "3db", "cool"],
            &[" ", " "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
            3,
        );

        assert_eq!(results.len(), 1, "should find 'is rag3db cool' via fuzzy d=3 on middle token '3db'");
        assert_eq!(results[0].doc_id, 0);
    }

    /// Pivot optimization: "is cool" — "is" has few matches but is short,
    /// "cool" is longer and also has few matches. Pivot should pick the
    /// most selective token. Result should still find doc 1.
    #[test]
    fn test_multi_token_pivot_short_first_token() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // "is cool" in doc 1: "rag3db is cool"
        // "is" (Ti=1), "cool" (Ti=2), separator " "
        let results = suffix_contains_multi_token(
            &reader,
            &["is", "cool"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    /// Pivot with 3 tokens: "rag3db from core" — pivot should be "rag3db"
    /// (most selective? actually all have 1-3 postings, but validates
    /// bidirectional chain building).
    #[test]
    fn test_multi_token_pivot_middle_longest() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Not a match: rag3db(Ti=1) from(Ti=2) but core is at Ti=4 not Ti=3
        // Wait — doc 0: import(0) rag3db(1) from(2) rag3db(3) core(4)
        // "rag3db from" → Ti=1,2 with sep " " ✓
        // But "from rag3db" → Ti=2,3 → sep "'" not " "... actually sep between Ti=2 and Ti=3 is " '"
        // Let's search "from rag3db" with sep " '"... no let's keep it simple
        // "import rag3db" → Ti=0,1 with sep " " ✓
        let results = suffix_contains_multi_token(
            &reader,
            &["import", "rag3db"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 0);
    }

    /// Fuzzy multi-token with pivot: "iz cool" (d=1) — "iz" fuzzy matches "is",
    /// pivot should pick "cool" (exact, fewer candidates) and validate backward.
    #[test]
    fn test_multi_token_fuzzy_pivot() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_multi_token_fuzzy(
            &reader,
            &["iz", "cool"],
            &[" "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
            1,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    /// Fuzzy multi-token: "rag3db iz kool" (d=1) — 3 tokens, fuzzy on middle and last.
    /// Pivot should pick the most selective, validate bidirectionally.
    #[test]
    fn test_multi_token_fuzzy_three_tokens() {
        let (sfx_bytes, raw_postings) = build_test_index();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        let results = suffix_contains_multi_token_fuzzy(
            &reader,
            &["rag3db", "iz", "kool"],
            &[" ", " "],
            |ord| raw_postings.get(&ord).cloned().unwrap_or_default(),
            1,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    /// Integration test: real Index with real ._raw posting lists.
    /// Verifies that resolve_raw_ordinal reads actual postings correctly.
    #[test]
    fn test_resolve_raw_ordinal_real_index() {
        use crate::schema::{SchemaBuilder, TextFieldIndexing, TextOptions};
        use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
        use crate::{Index, LucivyDocument, Term};

        // Build schema with a ._raw field (lowercase only, positions + offsets)
        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let body_raw = schema_builder.add_text_field("body_raw", raw_opts);
        let schema = schema_builder.build();

        // Build index in RAM
        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        // Doc 0: "import rag3db from core"
        let mut doc0 = LucivyDocument::new();
        doc0.add_text(body_raw, "import rag3db from core");
        writer.add_document(doc0).unwrap();

        // Doc 1: "rag3db is cool"
        let mut doc1 = LucivyDocument::new();
        doc1.add_text(body_raw, "rag3db is cool");
        writer.add_document(doc1).unwrap();

        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Get the single segment
        assert_eq!(searcher.segment_readers().len(), 1);
        let seg_reader = &searcher.segment_readers()[0];
        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();

        // Find ordinal for "rag3db"
        let term = Term::from_field_text(body_raw, "rag3db");
        let term_dict = inv_idx.terms();
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();

        // Resolve using our function
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);

        // "rag3db" appears in doc 0 (position 1) and doc 1 (position 0)
        assert_eq!(entries.len(), 2, "rag3db should appear in 2 docs");
        assert_eq!(entries[0].doc_id, 0);
        assert_eq!(entries[0].token_index, 1); // "import" is at pos 0, "rag3db" at pos 1
        assert_eq!(entries[0].byte_from, 7);   // "import " = 7 bytes
        assert_eq!(entries[0].byte_to, 13);    // "rag3db" = 6 bytes
        assert_eq!(entries[1].doc_id, 1);
        assert_eq!(entries[1].token_index, 0); // first token
        assert_eq!(entries[1].byte_from, 0);
        assert_eq!(entries[1].byte_to, 6);
    }

    /// E2E test with Unicode characters: accents, CJK, emoji in identifiers.
    /// Verifies that byte offsets are correct for multi-byte UTF-8 chars.
    #[test]
    fn test_e2e_unicode_characters() {
        use crate::schema::{SchemaBuilder, TextFieldIndexing, TextOptions};
        use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
        use crate::{Index, LucivyDocument, Term};

        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let body = schema_builder.add_text_field("body_raw", raw_opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        // Doc 0: French accents — "é" is 2 bytes, "ç" is 2 bytes
        let mut doc0 = LucivyDocument::new();
        doc0.add_text(body, "résumé café François");
        writer.add_document(doc0).unwrap();

        // Doc 1: CJK characters — each is 3 bytes
        let mut doc1 = LucivyDocument::new();
        doc1.add_text(body, "東京タワー hello 世界");
        writer.add_document(doc1).unwrap();

        // Doc 2: emoji + mixed — "🦀" is 4 bytes
        let mut doc2 = LucivyDocument::new();
        doc2.add_text(body, "rust🦀lang crème brûlée");
        writer.add_document(doc2).unwrap();

        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        let seg_reader = &searcher.segment_readers()[0];
        let inv_idx = seg_reader.inverted_index(body).unwrap();

        // Test 1: "résumé" — 8 bytes (r=1, é=2, s=1, u=1, m=1, é=2)
        let term = Term::from_field_text(body, "résumé");
        let term_dict = inv_idx.terms();
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 0);
        assert_eq!(entries[0].byte_from, 0);
        assert_eq!(entries[0].byte_to, 8); // "résumé" = 8 bytes

        // Test 2: "café" — 5 bytes (c=1, a=1, f=1, é=2)
        let term = Term::from_field_text(body, "café");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 0);
        assert_eq!(entries[0].byte_from, 9); // "résumé " = 8 + 1 space
        assert_eq!(entries[0].byte_to, 14);  // "café" = 5 bytes

        // Test 3: "françois" (lowercased from "François") — ç is 2 bytes
        let term = Term::from_field_text(body, "françois");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 0);
        assert_eq!(entries[0].byte_from, 15); // "résumé café " = 8 + 1 + 5 + 1
        assert_eq!(entries[0].byte_to, 24);   // "françois" = 9 bytes (ç=2)

        // Test 4: CJK — "東京タワー" = 15 bytes (5 chars × 3 bytes)
        let term = Term::from_field_text(body, "東京タワー");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 1);
        assert_eq!(entries[0].byte_from, 0);
        assert_eq!(entries[0].byte_to, 15);

        // Test 5: "hello" in CJK doc — byte_from after CJK chars
        let term = Term::from_field_text(body, "hello");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 1);
        assert_eq!(entries[0].byte_from, 16); // "東京タワー " = 15 + 1
        assert_eq!(entries[0].byte_to, 21);   // "hello" = 5

        // Test 6: "世界" — after "hello " in CJK doc
        let term = Term::from_field_text(body, "世界");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 1);
        assert_eq!(entries[0].byte_from, 22); // 15 + 1 + 5 + 1
        assert_eq!(entries[0].byte_to, 28);   // "世界" = 6 bytes

        // Test 7: "rust🦀lang" — 🦀 is 4 bytes, total = 4 + 4 + 4 = 12
        let term = Term::from_field_text(body, "rust🦀lang");
        // SimpleTokenizer splits on whitespace, so "rust🦀lang" is one token
        if let Some(ordinal) = term_dict.term_ord(term.serialized_value_bytes()).unwrap() {
            let entries = resolve_raw_ordinal(&inv_idx, ordinal);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].doc_id, 2);
            assert_eq!(entries[0].byte_from, 0);
            assert_eq!(entries[0].byte_to, 12); // "rust"=4 + "🦀"=4 + "lang"=4
        }

        // Test 8: "brûlée" — û=2 bytes, é=2 bytes → 8 bytes
        let term = Term::from_field_text(body, "brûlée");
        let ordinal = term_dict.term_ord(term.serialized_value_bytes()).unwrap().unwrap();
        let entries = resolve_raw_ordinal(&inv_idx, ordinal);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id, 2);
        // "rust🦀lang crème brûlée"
        // "rust🦀lang"=12, " "=1, "crème"=6, " "=1 → 20
        assert_eq!(entries[0].byte_from, 20);
        assert_eq!(entries[0].byte_to, 28); // "brûlée" = 8 bytes
    }

    /// Regression test: "contains 'function'" on text with "function" and "disjunction".
    /// The suffix FST must NOT produce parasitic matches from "disjunction" suffixes
    /// like "junction" or "unction" when searching for "function".
    #[test]
    fn test_no_parasitic_matches_function_disjunction() {
        // Text: "the function foo() calls disjunction bar()"
        // Tokens: the(0) function(1) foo(2) calls(3) disjunction(4) bar(5)
        let mut collector = SfxCollector::new();
        collector.begin_doc();
        collector.begin_value("the function foo() calls disjunction bar()");
        collector.add_token("the", 0, 3);
        collector.add_token("function", 4, 12);
        collector.add_token("foo", 13, 16);
        collector.add_token("calls", 20, 25);
        collector.add_token("disjunction", 26, 37);
        collector.add_token("bar", 38, 41);
        collector.end_value();
        collector.end_doc();

        let (sfx_bytes, _sfxpost_bytes) = collector.build().unwrap();
        let reader = SfxFileReader::open(&sfx_bytes).unwrap();

        // Sorted unique tokens: bar(0), calls(1), disjunction(2), foo(3), function(4), the(5)
        let mut raw_postings: HashMap<u64, Vec<RawPostingEntry>> = HashMap::new();
        raw_postings.insert(0, vec![
            RawPostingEntry { doc_id: 0, token_index: 5, byte_from: 38, byte_to: 41 },
        ]);
        raw_postings.insert(1, vec![
            RawPostingEntry { doc_id: 0, token_index: 3, byte_from: 20, byte_to: 25 },
        ]);
        raw_postings.insert(2, vec![
            RawPostingEntry { doc_id: 0, token_index: 4, byte_from: 26, byte_to: 37 },
        ]);
        raw_postings.insert(3, vec![
            RawPostingEntry { doc_id: 0, token_index: 2, byte_from: 13, byte_to: 16 },
        ]);
        raw_postings.insert(4, vec![
            RawPostingEntry { doc_id: 0, token_index: 1, byte_from: 4, byte_to: 12 },
        ]);
        raw_postings.insert(5, vec![
            RawPostingEntry { doc_id: 0, token_index: 0, byte_from: 0, byte_to: 3 },
        ]);

        let results = suffix_contains_single_token(&reader, "function", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });

        // Must find exactly ONE match: "function" at byte 4..12, SI=0
        eprintln!("results for 'function': {:?}", results);
        assert_eq!(results.len(), 1, "should find exactly 1 match, no parasites");
        assert_eq!(results[0].doc_id, 0);
        assert_eq!(results[0].byte_from, 4);
        assert_eq!(results[0].byte_to, 12);
        assert_eq!(results[0].si, 0);

        // Also verify: "unction" search should find BOTH (from function SI=1 and disjunction SI=4)
        let results_unction = suffix_contains_single_token(&reader, "unction", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });
        eprintln!("results for 'unction': {:?}", results_unction);
        // "unction" in "function" at SI=1: byte_from=4+1=5, byte_to=5+7=12
        // "unction" in "disjunction" at SI=4: byte_from=26+4=30, byte_to=30+7=37
        assert_eq!(results_unction.len(), 2, "unction should match both function and disjunction");
        assert_eq!(results_unction[0].byte_from, 5);  // function[1..]
        assert_eq!(results_unction[1].byte_from, 30); // disjunction[4..]

        // And "junction" should find only disjunction
        let results_junction = suffix_contains_single_token(&reader, "junction", |ord| {
            raw_postings.get(&ord).cloned().unwrap_or_default()
        });
        eprintln!("results for 'junction': {:?}", results_junction);
        // "junction" in "disjunction" at SI=3: byte_from=26+3=29, byte_to=29+8=37
        assert_eq!(results_junction.len(), 1, "junction should only match disjunction");
        assert_eq!(results_junction[0].byte_from, 29);
    }
}
