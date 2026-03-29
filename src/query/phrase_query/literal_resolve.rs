//! Literal resolution primitives: find a literal string in the index,
//! intersect multiple literals, validate DFA paths between positions.
//!
//! These are the building blocks used by regex contains (and potentially
//! by exact contains in the future). Each function is self-contained
//! and works at the (doc_id, position, byte_from, byte_to) level.

use std::collections::{HashMap, HashSet};

use crate::suffix_fst::file::SfxFileReader;
use crate::suffix_fst::gapmap::is_value_boundary;
use crate::suffix_fst::posmap::PosMapReader;
use crate::query::posting_resolver::PostingResolver;
use crate::DocId;

use super::suffix_contains;

// ─────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────

/// A match for a literal string in the index.
#[derive(Debug, Clone)]
pub struct LiteralMatch {
    pub doc_id: DocId,
    pub position: u32,
    pub byte_from: u32,
    pub byte_to: u32,
    /// Suffix index: byte offset within the parent token where the match starts.
    /// `byte_from - si` gives the content byte start of the parent token.
    pub si: u16,
}

// ─────────────────────────────────────────────────────────────────────
// 1. find_literal — resolve a literal using exact contains logic
// ─────────────────────────────────────────────────────────────────────

/// Find all occurrences of `literal` in the index, including cross-token
/// matches via sibling links. Uses the same code path as exact contains.
///
/// Returns matches with (doc_id, position, byte_from, byte_to).
pub fn find_literal(
    sfx_reader: &SfxFileReader<'_>,
    literal: &str,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<LiteralMatch> {
    // Build the raw_term_resolver closure from PostingResolver.
    let raw_resolver = |raw_ordinal: u64| -> Vec<suffix_contains::RawPostingEntry> {
        resolver.resolve(raw_ordinal).into_iter().map(|e| {
            suffix_contains::RawPostingEntry {
                doc_id: e.doc_id,
                token_index: e.position,
                byte_from: e.byte_from,
                byte_to: e.byte_to,
            }
        }).collect()
    };

    // Use the exact same function as contains search.
    let matches = suffix_contains::suffix_contains_single_token_with_terms(
        sfx_reader,
        literal,
        &raw_resolver,
        Some(&ord_to_term),
    );

    matches.into_iter().map(|m| LiteralMatch {
        doc_id: m.doc_id,
        position: m.token_index,
        byte_from: m.byte_from as u32,
        byte_to: m.byte_to as u32,
        si: m.si,
    }).collect()
}

// ─────────────────────────────────────────────────────────────────────
// 2. intersect_literals — intersect matches from multiple literals
// ─────────────────────────────────────────────────────────────────────

/// Per-literal matches grouped by doc_id.
pub type MatchesByDoc = HashMap<DocId, Vec<(u32, u32, u32, u16)>>; // (position, byte_from, byte_to, si)

/// Group literal matches by doc_id.
pub fn group_by_doc(matches: &[LiteralMatch]) -> MatchesByDoc {
    let mut by_doc: MatchesByDoc = HashMap::new();
    for m in matches {
        by_doc.entry(m.doc_id).or_default().push((m.position, m.byte_from, m.byte_to, m.si));
    }
    by_doc
}

/// Intersect multiple literal match sets: keep only doc_ids present in ALL sets,
/// where the literals appear in order (each literal's byte_from >= previous literal's byte_to).
///
/// `literals_by_doc[i]` = matches for literal i, grouped by doc.
/// Literals are in regex order.
///
/// Returns: set of doc_ids that survive + per-doc matched ranges
/// `(doc_id, first_byte_from, last_byte_to, first_si)`.
pub fn intersect_literals_ordered(
    literals_by_doc: &[MatchesByDoc],
) -> Vec<(DocId, u32, u32, u16)> {
    if literals_by_doc.is_empty() {
        return Vec::new();
    }

    // Start with the smallest set for efficiency.
    let smallest_idx = literals_by_doc.iter()
        .enumerate()
        .min_by_key(|(_, m)| m.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut results = Vec::new();

    for (&doc_id, _) in &literals_by_doc[smallest_idx] {
        // Check all other literals have this doc.
        let all_present = literals_by_doc.iter().all(|m| m.contains_key(&doc_id));
        if !all_present {
            continue;
        }

        // Check position ordering: find a chain where each literal appears after the previous.
        let first_matches = &literals_by_doc[0][&doc_id];
        for &(_, first_bf, first_bt, first_si) in first_matches {
            let mut min_byte = first_bt;
            let mut all_ok = true;
            let mut last_bt = first_bt;

            for lit_matches in &literals_by_doc[1..] {
                let positions = &lit_matches[&doc_id];
                if let Some(&(_, _bf, bt, _si)) = positions.iter()
                    .filter(|&&(_, bf, _, _)| bf >= min_byte)
                    .min_by_key(|&&(_, bf, _, _)| bf)
                {
                    min_byte = bt;
                    last_bt = bt;
                } else {
                    all_ok = false;
                    break;
                }
            }

            if all_ok {
                results.push((doc_id, first_bf, last_bt, first_si));
                // Don't break — collect ALL matches per doc
            }
        }
    }

    results
}

// ─────────────────────────────────────────────────────────────────────
// 2b. intersect_trigrams — fuzzy match via trigram pigeonhole
// ─────────────────────────────────────────────────────────────────────

/// Intersect trigram matches for fuzzy search: find docs where at least
/// `threshold` trigrams appear in order with byte span consistent with
/// the query (±distance tolerance).
///
/// `trigrams_by_doc[i]` = matches for trigram i (in query order).
/// `query_positions[i]` = byte position of trigram i in the query string.
///
/// Returns: `(doc_id, first_byte_from, last_byte_to, first_tri_idx, first_si, trigram_proven)`.
/// `first_tri_idx` is the index of the first trigram in the best chain — needed
/// to compute the actual match start position (`first_bf - query_positions[first_tri_idx]`).
/// `first_si` is the suffix index of the first trigram match — `first_bf - first_si`
/// gives the content byte start of the parent token.
/// `trigram_proven` is true when ALL trigrams matched with consistent byte span —
/// the match is guaranteed correct and DFA validation can be skipped.
pub fn intersect_trigrams_with_threshold(
    trigrams_by_doc: &[MatchesByDoc],
    query_positions: &[usize],
    threshold: usize,
    distance: u8,
) -> Vec<(DocId, u32, u32, usize, u16, bool)> {
    if trigrams_by_doc.is_empty() || threshold == 0 {
        return Vec::new();
    }

    // Collect all doc_ids that appear in at least one trigram
    let mut all_docs: HashSet<DocId> = HashSet::new();
    for tri_matches in trigrams_by_doc {
        for &doc_id in tri_matches.keys() {
            all_docs.insert(doc_id);
        }
    }

    let num_trigrams = trigrams_by_doc.len();
    let mut results: Vec<(DocId, u32, u32, usize, u16, bool)> = Vec::new();

    for &doc_id in &all_docs {
        // Collect all (tri_index, byte_from, byte_to, si) for this doc, sorted by byte_from
        let mut entries: Vec<(usize, u32, u32, u16)> = Vec::new();
        for (tri_idx, tri_matches) in trigrams_by_doc.iter().enumerate() {
            if let Some(positions) = tri_matches.get(&doc_id) {
                for &(_pos, bf, bt, si) in positions {
                    entries.push((tri_idx, bf, bt, si));
                }
            }
        }
        entries.sort_by_key(|&(_, bf, _, _)| bf);

        // Greedy scan: find ALL chains with increasing tri_index.
        // Each chain that meets the threshold + byte span check becomes a result.
        // Cap at MAX_CHAINS_PER_DOC to avoid O(N²) DFA validations on common bigrams.
        const MAX_CHAINS_PER_DOC: usize = 20;
        let mut current_chain: Vec<(usize, u32, u32, u16)> = Vec::new();
        let results_before = results.len();

        let mut check_chain = |chain: &[(usize, u32, u32, u16)], results: &mut Vec<(DocId, u32, u32, usize, u16, bool)>| -> bool {
            if chain.len() < threshold { return false; }
            if results.len() - results_before >= MAX_CHAINS_PER_DOC { return true; } // cap reached
            let first = &chain[0];
            let last = &chain[chain.len() - 1];
            let text_span = last.1 as i64 - first.1 as i64;
            let query_span = query_positions[last.0] as i64 - query_positions[first.0] as i64;
            let span_diff = (text_span - query_span).unsigned_abs();
            if span_diff > distance as u64 { return false; }
            // If ALL trigrams matched with consistent span, the match is
            // proven by pigeonhole — DFA validation can be skipped.
            let proven = chain.len() == num_trigrams && span_diff <= distance as u64;
            results.push((doc_id, first.1, last.2, first.0, first.3, proven));
            false
        };

        let mut capped = false;
        for &(tri_idx, bf, bt, si) in &entries {
            if capped { break; }
            if current_chain.is_empty()
                || tri_idx > current_chain.last().unwrap().0
            {
                current_chain.push((tri_idx, bf, bt, si));
            } else {
                capped = check_chain(&current_chain, &mut results);
                current_chain.clear();
                current_chain.push((tri_idx, bf, bt, si));
            }
        }
        if !capped { check_chain(&current_chain, &mut results); }
    }

    results
}

// ─────────────────────────────────────────────────────────────────────
// 3. validate_path — DFA validation between two known positions
// ─────────────────────────────────────────────────────────────────────

/// Validate that the DFA accepts the token sequence between pos_from and pos_to
/// (inclusive) in a document. Uses PosMap to read ordinals and GapMap for gaps.
/// If a ByteMap is provided, tokens whose bytes are incompatible with the DFA
/// are detected early without feeding individual bytes.
///
/// Returns the DFA state at the first accepting point, or None if the DFA
/// never accepts (dies or reaches pos_to without accepting).
pub fn validate_path<A: lucivy_fst::Automaton>(
    automaton: &A,
    dfa_state: &A::State,
    posmap: &PosMapReader<'_>,
    sfx_reader: &SfxFileReader<'_>,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    doc_id: DocId,
    pos_from: u32, // exclusive: start feeding AFTER this position
    pos_to: u32,   // inclusive: feed up to and including this position
    bytemap: Option<&crate::suffix_fst::bytemap::ByteBitmapReader<'_>>,
) -> Option<A::State>
where
    A::State: Clone,
{
    let gapmap = sfx_reader.gapmap();
    let mut state = dfa_state.clone();

    for pos in (pos_from + 1)..=pos_to {
        // Feed gap bytes between previous position and this one.
        let gap = gapmap.read_separator(doc_id, pos - 1, pos);
        if let Some(gap_bytes) = gap {
            if is_value_boundary(gap_bytes) {
                return None;
            }
            for &byte in gap_bytes {
                state = automaton.accept(&state, byte);
                if !automaton.can_match(&state) {
                    return None;
                }
            }
        }

        // Feed token text at this position.
        if let Some(tok_ord) = posmap.ordinal_at(doc_id, pos) {
            // ByteMap pre-filter: skip token if no byte can advance the DFA
            if let Some(bm) = bytemap {
                if !super::dfa_byte_filter::can_token_advance_dfa(automaton, &state, bm, tok_ord) {
                    return None;
                }
            }
            if let Some(text) = ord_to_term(tok_ord as u64) {
                for &byte in text.as_bytes() {
                    state = automaton.accept(&state, byte);
                    if !automaton.can_match(&state) {
                        return None;
                    }
                }
            }
        }

        // Early return: if DFA accepts after this token, we have a match.
        // Don't continue feeding — the regex is satisfied.
        if automaton.is_match(&state) {
            return Some(state);
        }
    }

    Some(state)
}

/// Quick check: does the DFA accept ANY path between two positions?
/// For `.*` patterns this always returns true without needing PosMap.
pub fn dfa_accepts_anything<A: lucivy_fst::Automaton>(
    automaton: &A,
    state: &A::State,
) -> bool
where
    A::State: Clone,
{
    // A DFA that accepts anything: is_match AND can_match from current state.
    // This is the case for `.*` — the DFA is already accepting and can continue.
    automaton.is_match(state) && automaton.can_match(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intersect_empty() {
        let result = intersect_literals_ordered(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_single() {
        let mut by_doc = HashMap::new();
        by_doc.insert(1, vec![(0, 0, 5, 0)]);
        by_doc.insert(2, vec![(0, 0, 3, 0)]);

        let result = intersect_literals_ordered(&[by_doc]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_intersect_two_ordered() {
        let mut lit_a = HashMap::new();
        lit_a.insert(1, vec![(0, 0, 5, 0)]); // doc1: "hello" at pos 0
        lit_a.insert(2, vec![(0, 0, 3, 0)]); // doc2: "foo" at pos 0

        let mut lit_b = HashMap::new();
        lit_b.insert(1, vec![(2, 10, 15, 0)]); // doc1: "world" at pos 2, byte 10-15
        // doc2 doesn't have lit_b → eliminated

        let result = intersect_literals_ordered(&[lit_a, lit_b]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 1); // doc1
        assert_eq!(result[0].1, 0); // first_byte_from
        assert_eq!(result[0].2, 15); // last_byte_to
    }

    #[test]
    fn test_intersect_wrong_order() {
        let mut lit_a = HashMap::new();
        lit_a.insert(1, vec![(5, 20, 25, 0)]); // doc1: lit_a at byte 20-25

        let mut lit_b = HashMap::new();
        lit_b.insert(1, vec![(2, 5, 10, 0)]); // doc1: lit_b at byte 5-10 (BEFORE lit_a)

        let result = intersect_literals_ordered(&[lit_a, lit_b]);
        assert!(result.is_empty()); // wrong order → eliminated
    }
}
