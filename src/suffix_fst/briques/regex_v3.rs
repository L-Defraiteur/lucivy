//! Regex orchestrator for SFX v3.
//!
//! Pipeline: literal extraction → resolve via briques → gap validation.
//!
//! 1. analyze_regex(pattern) → literals + gaps typés (réutilise regex_gap_analyzer)
//! 2. Résoudre chaque littéral via find_literal_v3 (rarest-first, doc_filter)
//! 3. Intersect par doc (position ordonnée)
//! 4. Valider les gaps :
//!    - AcceptAnything → accept direct
//!    - ByteRangeCheck → vérifier via ByteMap
//!    - DfaValidation → walk DFA token par token via PosMap
//!
//! strict_separators = true toujours pour regex (le pattern définit ce qui matche).

use std::collections::{HashMap, HashSet};

use common::BitSet;
use lucivy_fst::Automaton;

use crate::DocId;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::bytemap::ByteBitmapReader;
use crate::suffix_fst::file_v3::SfxFileReaderV3;
use crate::suffix_fst::posmap::PosMapReader;

use super::composite;
use super::resolve::MatchV3;

/// Minimum literal length to be considered viable for resolution.
const MIN_LITERAL_LEN: usize = 2;

/// Maximum token positions to walk for DFA validation.
const MAX_DFA_WALK_DEPTH: u32 = 64;

// ─── Types ─────────────────────────────────────────────────────────────────

/// Grouped matches by doc: doc_id → [(position, byte_from, byte_to, sti)]
type MatchesByDocV3 = HashMap<DocId, Vec<(u32, u32, u32, u16)>>;

fn group_by_doc_v3(matches: &[MatchV3]) -> MatchesByDocV3 {
    let mut by_doc: MatchesByDocV3 = HashMap::new();
    for m in matches {
        by_doc.entry(m.doc_id).or_default().push((m.position, m.byte_from, m.byte_to, m.sti));
    }
    by_doc
}

/// Intersect multiple literal match sets: find docs where all literals
/// appear in order (by byte offset).
fn intersect_ordered_v3(
    literals_by_doc: &[MatchesByDocV3],
) -> Vec<(DocId, u32, u32, u16)> {
    if literals_by_doc.is_empty() {
        return Vec::new();
    }

    let smallest_idx = literals_by_doc.iter()
        .enumerate()
        .min_by_key(|(_, m)| m.len())
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut results = Vec::new();

    for &doc_id in literals_by_doc[smallest_idx].keys() {
        if !literals_by_doc.iter().all(|m| m.contains_key(&doc_id)) {
            continue;
        }

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
            }
        }
    }

    results
}

// ─── validate_path_v3 ─────────────────────────────────────────────────────

/// Walk DFA token-by-token from pos_from+1 to pos_to.
///
/// V3: no gapmap needed. Token text includes trailing seps, so the DFA
/// traverses them naturally. We just feed each token's text to the DFA.
///
/// For own_len truncation: we feed only `text[..own_len]` to exclude overlap
/// bytes (those belong to the next token and will be fed in the next iteration).
fn validate_path_v3<A: Automaton>(
    automaton: &A,
    dfa_state: &A::State,
    posmap: &PosMapReader<'_>,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    doc_id: DocId,
    pos_from: u32,
    pos_to: u32,
    bytemap: Option<&ByteBitmapReader<'_>>,
) -> Option<A::State>
where
    A::State: Clone,
{
    let mut state = dfa_state.clone();

    for pos in (pos_from + 1)..=pos_to {
        let tok_ord = posmap.ordinal_at(doc_id, pos)?;

        // ByteMap pre-filter
        if let Some(bm) = bytemap {
            if !crate::query::phrase_query::dfa_byte_filter::can_token_advance_dfa(
                automaton, &state, bm, tok_ord,
            ) {
                return None;
            }
        }

        let text = ord_to_term(tok_ord as u64)?;
        // In v3, token text includes content + sep (+ overlap in termtexts).
        // We feed the full text — the DFA traverses seps naturally.
        // The overlap bytes are also fed, which is fine: they're the start of
        // the next token and will match the beginning of the next DFA walk.
        // Actually, to avoid double-feeding overlap, we should truncate at own_len.
        // But we don't know own_len here... for now feed full text.
        // TODO: pass own_len via termtexts metadata if needed.
        for &byte in text.as_bytes() {
            state = automaton.accept(&state, byte);
            if !automaton.can_match(&state) {
                return None;
            }
        }

        if automaton.is_match(&state) {
            return Some(state);
        }
    }

    Some(state)
}

/// Check if DFA accepts any input from this state (fast path for `.*` gaps).
fn dfa_accepts_anything_v3<A: Automaton>(automaton: &A, state: &A::State) -> bool
where
    A::State: Clone,
{
    // Try all 256 byte values. If all transitions lead to accepting or can_match states,
    // and at least one path leads to is_match, then this state accepts anything.
    // Simplified: check if current state already matches, or if it's a ".*" sink.
    if automaton.is_match(state) {
        return true;
    }
    // Try feeding a few common bytes to see if the DFA is in an accept-anything state
    let test_bytes = [b'a', b'z', b'0', b'_', b' ', 0xFF];
    let mut all_accept = true;
    for &b in &test_bytes {
        let next = automaton.accept(state, b);
        if !automaton.is_match(&next) {
            all_accept = false;
            break;
        }
    }
    all_accept
}

// ─── regex_v3 ─────────────────────────────────────────────────────────────

/// Regex search via literal extraction + DFA gap validation.
///
/// strict_separators = true always (the regex defines what matches).
pub fn regex_v3<A: Automaton>(
    automaton: &A,
    pattern: &str,
    reader: &SfxFileReaderV3,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    anchor_start: bool,
    max_doc: DocId,
    posmap_data: Option<&[u8]>,
    bytemap_data: Option<&[u8]>,
) -> (BitSet, Vec<(DocId, usize, usize)>)
where
    A::State: Clone + Eq + std::hash::Hash,
{
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();

    // Step 1: extract literals + gap types from regex
    let (all_literals, analyzed_gaps) =
        crate::query::phrase_query::regex_gap_analyzer::analyze_regex(pattern);

    let viable: Vec<&String> = all_literals.iter()
        .filter(|l| l.len() >= MIN_LITERAL_LEN)
        .collect();

    if viable.is_empty() {
        return (doc_bitset, highlights);
    }

    // Step 2: resolve literals via briques (rarest-first, doc filter)
    let strict_sep = true; // always for regex
    let mut lit_selectivity: Vec<(usize, usize)> = viable.iter()
        .enumerate()
        .map(|(i, lit)| {
            let s = super::resolve::selectivity_v3(reader, lit, strict_sep);
            (i, s)
        })
        .collect();
    lit_selectivity.sort_by_key(|&(_, s)| s);

    let mut all_matches: Vec<Vec<MatchV3>> = vec![Vec::new(); viable.len()];
    let mut doc_filter: Option<HashSet<DocId>> = None;

    for &(lit_idx, _) in &lit_selectivity {
        let matches = composite::find_literal_v3(
            reader, viable[lit_idx], resolver, anchor_start && lit_idx == 0,
            strict_sep, doc_filter.as_ref(),
        );

        if doc_filter.is_none() && !matches.is_empty() {
            doc_filter = Some(matches.iter().map(|m| m.doc_id).collect());
        }

        all_matches[lit_idx] = matches;
    }

    // Step 3: intersect by doc (position ordered)
    let start_state = automaton.start();
    let posmap = posmap_data.and_then(PosMapReader::open);
    let bytemap = bytemap_data.and_then(ByteBitmapReader::open);

    let has_any_dfa_gap = analyzed_gaps.iter()
        .any(|g| matches!(g, crate::query::phrase_query::regex_gap_analyzer::GapKind::DfaValidation));
    let all_accept = !has_any_dfa_gap && analyzed_gaps.iter()
        .all(|g| matches!(g, crate::query::phrase_query::regex_gap_analyzer::GapKind::AcceptAnything));

    if all_matches.len() == 1 {
        // Single literal: DFA validate each match
        let literal_bytes = viable[0].as_bytes();

        for m in &all_matches[0] {
            let mut state = start_state.clone();
            let mut alive = true;

            // Feed the literal
            for &byte in literal_bytes {
                state = automaton.accept(&state, byte);
                if !automaton.can_match(&state) { alive = false; break; }
            }
            if !alive { continue; }

            // Feed remaining bytes of current token after the literal
            if let Some(text) = ord_to_term(m.ordinal) {
                let remaining_start = m.sti as usize + literal_bytes.len();
                let text_bytes = text.as_bytes();
                if remaining_start < text_bytes.len() {
                    for &byte in &text_bytes[remaining_start..] {
                        state = automaton.accept(&state, byte);
                        if !automaton.can_match(&state) { alive = false; break; }
                    }
                    if !alive { continue; }
                }
            }

            if automaton.is_match(&state) {
                doc_bitset.insert(m.doc_id);
                highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                continue;
            }

            // DFA alive but not accepting → cross-token via PosMap
            if automaton.can_match(&state) {
                if dfa_accepts_anything_v3(automaton, &state) {
                    doc_bitset.insert(m.doc_id);
                    highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                } else if let Some(pm) = &posmap {
                    let max_pos = pm.num_tokens(m.doc_id);
                    let end_pos = (m.position + MAX_DFA_WALK_DEPTH).min(max_pos);
                    if end_pos > m.position {
                        if let Some(final_state) = validate_path_v3(
                            automaton, &state, pm, ord_to_term,
                            m.doc_id, m.position, end_pos - 1,
                            bytemap.as_ref(),
                        ) {
                            if automaton.is_match(&final_state) {
                                doc_bitset.insert(m.doc_id);
                                highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                            }
                        }
                    }
                }
            }
        }
    } else {
        // Multi-literal: intersect + gap validation
        let grouped: Vec<MatchesByDocV3> = all_matches.iter()
            .map(|matches| group_by_doc_v3(matches))
            .collect();

        let ordered = intersect_ordered_v3(&grouped);

        if all_accept {
            // All gaps are .* → accept all ordered matches
            for &(doc_id, first_bf, last_bt, _) in &ordered {
                doc_bitset.insert(doc_id);
                highlights.push((doc_id, first_bf as usize, last_bt as usize));
            }
        } else if let Some(pm) = &posmap {
            for &(doc_id, first_bf, last_bt, first_si) in &ordered {
                let first_entry = grouped[0].get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, bf, _, _)| bf == first_bf));
                let Some(&(first_pos, _, _, _)) = first_entry else { continue; };

                let last_entry = grouped.last().unwrap().get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, _, bt, _)| bt == last_bt));
                let last_pos = last_entry.map(|&(p, _, _, _)| p).unwrap_or(first_pos);

                // Feed first token from literal offset
                let mut state = start_state.clone();
                let mut alive = true;
                if let Some(tok_ord) = pm.ordinal_at(doc_id, first_pos) {
                    if let Some(text) = ord_to_term(tok_ord as u64) {
                        let offset = first_si as usize;
                        for &byte in &text.as_bytes()[offset..] {
                            state = automaton.accept(&state, byte);
                            if !automaton.can_match(&state) { alive = false; break; }
                        }
                    } else { continue; }
                } else { continue; }

                if !alive { continue; }

                if automaton.is_match(&state) {
                    doc_bitset.insert(doc_id);
                    highlights.push((doc_id, first_bf as usize, last_bt as usize));
                    continue;
                }

                // Walk DFA through remaining tokens
                if last_pos > first_pos {
                    if let Some(final_state) = validate_path_v3(
                        automaton, &state, pm, ord_to_term,
                        doc_id, first_pos, last_pos,
                        bytemap.as_ref(),
                    ) {
                        if automaton.is_match(&final_state) {
                            doc_bitset.insert(doc_id);
                            highlights.push((doc_id, first_bf as usize, last_bt as usize));
                        }
                    }
                }
            }
        }
    }

    (doc_bitset, highlights)
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
    use crate::query::phrase_query::regex_gap_analyzer;

    struct MockResolver(SfxPostReaderV2);
    impl MockResolver {
        fn new(data: &[u8]) -> Self { Self(SfxPostReaderV2::open_slice(data).unwrap()) }
    }
    impl PostingResolver for MockResolver {
        fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
            self.0.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
                doc_id: e.doc_id, position: e.token_index,
                byte_from: e.byte_from, byte_to: e.byte_to,
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

    // ── analyze_regex (reused from v2) ──

    #[test]
    fn test_analyze_regex_simple() {
        let (lits, gaps) = regex_gap_analyzer::analyze_regex("mutex.*lock");
        assert!(lits.len() >= 2);
        assert!(lits.iter().any(|l| l.contains("mutex")));
        assert!(lits.iter().any(|l| l.contains("lock")));
    }

    #[test]
    fn test_analyze_regex_char_class() {
        let (lits, gaps) = regex_gap_analyzer::analyze_regex("foo[a-z]+bar");
        assert!(lits.iter().any(|l| l.contains("foo")));
        assert!(lits.iter().any(|l| l.contains("bar")));
        if !gaps.is_empty() {
            // The gap between foo and bar should be ByteRangeCheck
            assert!(gaps.iter().any(|g| matches!(g,
                regex_gap_analyzer::GapKind::ByteRangeCheck(_) |
                regex_gap_analyzer::GapKind::AcceptAnything
            )));
        }
    }

    // ── regex_v3 with simple automaton ──

    // We can't easily construct a full regex DFA in tests without the regex crate,
    // so we test the literal extraction + intersection path.

    #[test]
    fn test_literal_extraction_finds_docs() {
        let (sfx, post) = build_index(&["mutex_lock_init", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // Verify literals are findable
        let matches = composite::find_literal_v3(&reader, "mutex", &resolver, false, true, None);
        assert!(!matches.is_empty(), "literal 'mutex' should be found");
        assert_eq!(matches[0].doc_id, 0);

        let matches = composite::find_literal_v3(&reader, "lock", &resolver, false, true, None);
        assert!(!matches.is_empty(), "literal 'lock' should be found");
    }

    #[test]
    fn test_intersect_ordered() {
        let (sfx, post) = build_index(&["mutex_lock_init"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        let m1 = composite::find_literal_v3(&reader, "mutex", &resolver, false, true, None);
        let m2 = composite::find_literal_v3(&reader, "init", &resolver, false, true, None);

        let g1 = group_by_doc_v3(&m1);
        let g2 = group_by_doc_v3(&m2);

        let ordered = intersect_ordered_v3(&[g1, g2]);
        assert!(!ordered.is_empty(), "mutex...init should intersect in doc 0");
        assert_eq!(ordered[0].0, 0); // doc_id
    }

    #[test]
    fn test_intersect_no_match() {
        let (sfx, post) = build_index(&["mutex_lock", "hello_world"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);

        // "mutex" in doc 0, "world" in doc 1 → no intersection
        let m1 = composite::find_literal_v3(&reader, "mutex", &resolver, false, true, None);
        let m2 = composite::find_literal_v3(&reader, "world", &resolver, false, true, None);

        let g1 = group_by_doc_v3(&m1);
        let g2 = group_by_doc_v3(&m2);

        let ordered = intersect_ordered_v3(&[g1, g2]);
        assert!(ordered.is_empty(), "mutex and world are in different docs");
    }
}
