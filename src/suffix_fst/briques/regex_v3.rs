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
            if automaton.is_match(&state) {
                return Some(state);
            }
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


/// True if every byte present in `ordinal`'s bitmap falls inside one of `ranges`.
///
/// `GapKind::ByteRangeCheck` was analysed by the gap analyzer since day one but
/// never acted upon: regex_v3 only ever tested for DfaValidation and
/// AcceptAnything, so a `[a-z]+` or `\w+` gap fell through to the generic DFA
/// walk. That walk feeds whole tokens through a whole-input automaton and
/// over-rejects badly (measured: 5636 rejections for 40 accepts on
/// `Table\w+Function`). Checking the class directly is both cheaper and closer to
/// what the gap actually means.
/// Exact class membership.
///
/// FST retrieval is case-insensitive (keys are lowercased at build time), so it
/// yields a SUPERSET of candidates. Narrowing on case is the verification pass's
/// job — folding the class here instead would make `[a-z]` and `[A-Z]` mean the
/// same thing, i.e. a pattern that does not do what it says.
fn byte_in_ranges(b: u8, ranges: &[(u8, u8)]) -> bool {
    ranges.iter().any(|&(lo, hi)| b >= lo && b <= hi)
}

fn all_bytes_in_ranges(
    bytemap: &ByteBitmapReader<'_>,
    ordinal: u32,
    ranges: &[(u8, u8)],
) -> bool {
    let Some(bitmap) = bytemap.bitmap(ordinal) else { return false };
    for byte in 0u16..=255 {
        let b = byte as u8;
        if bitmap[(b >> 3) as usize] & (1u8 << (b & 7)) == 0 {
            continue;
        }
        if !byte_in_ranges(b, ranges) {
            return false;
        }
    }
    true
}


/// Rebuild the raw document text covering token positions `[from, to]`.
///
/// Each token contributes `text[..own_len]` — content plus separator. The trailing
/// overlap belongs to the next token and would otherwise be duplicated, so the
/// concatenation of own-parts is exactly the source text over that range.
///
/// termtexts keeps the ORIGINAL case (only the FST keys are lowercased), which is
/// what makes case-sensitive verification possible at all.
fn reconstruct_span(
    posmap: &PosMapReader<'_>,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    own_len_of: &dyn Fn(u64) -> Option<usize>,
    doc_id: DocId,
    from: u32,
    to: u32,
) -> String {
    let mut out = String::new();
    for pos in from..=to {
        let Some(ord) = posmap.ordinal_at(doc_id, pos) else { break };
        let Some(text) = ord_to_term(ord as u64) else { break };
        let end = own_len_of(ord as u64).unwrap_or(text.len()).min(text.len());
        out.push_str(&text[..end]);
    }
    out
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
    sibling_data: Option<&[u8]>,
    termtexts_data: Option<&[u8]>,
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

    let diag = std::env::var("V3_DIAG_REGEX").is_ok();
    if diag {
        eprintln!("[rx] pattern={pattern:?}");
        eprintln!("[rx]   literals={all_literals:?} viable={viable:?}");
        eprintln!("[rx]   gaps={analyzed_gaps:?}");
    }

    if viable.is_empty() {
        if diag { eprintln!("[rx]   ABORT: no viable literal (MIN_LITERAL_LEN={MIN_LITERAL_LEN})"); }
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

    // Build PosMap/ByteMap early — needed for both find_literal (relaxed chains)
    // and DFA gap validation.
    let posmap = posmap_data.and_then(PosMapReader::open);
    let bytemap = bytemap_data.and_then(ByteBitmapReader::open);
    let tt_meta = termtexts_data
        .and_then(crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open);
    let own_len_of = |ord: u64| -> Option<usize> {
        tt_meta.as_ref().and_then(|tt| tt.meta(ord as u32)).map(|m| m.own_len as usize)
    };

    // Verification runs ONLY where it can change the answer.
    //
    // Two accept paths are approximations: `all_accept` takes any ordered pair of
    // literals without looking at what lies between them, and the class path checks
    // a byte class rather than the actual arrangement. Both over-generate (measured:
    // 254 extra docs on `rag3.*ver`, 65 on `Table\w+Function`). The DFA paths are
    // already exact, and a gapless pattern is resolved exactly by the literal walk —
    // in those cases verification would cost a text rebuild for nothing.
    let approximated = analyzed_gaps.iter().any(|g| matches!(g,
        crate::query::phrase_query::regex_gap_analyzer::GapKind::AcceptAnything |
        crate::query::phrase_query::regex_gap_analyzer::GapKind::ByteRangeCheck(_)));
    let verifier = if approximated {
        regex::Regex::new(pattern).ok()
    } else {
        None
    };
    if diag {
        eprintln!("[rx]   approximated={approximated} verifier={}", verifier.is_some());
    }

    let mut all_matches: Vec<Vec<MatchV3>> = vec![Vec::new(); viable.len()];
    let mut doc_filter: Option<HashSet<DocId>> = None;

    for &(lit_idx, _) in &lit_selectivity {
        let ctx = super::context::BriquesContext {
            reader, resolver, filter_docs: doc_filter.as_ref(),
            debug: false,
            trace_id: None,
            // Strict mode: no word pipeline. But chunk chains ARE needed — a v3
            // chunk is at most 8 bytes, so any literal straddling a boundary is
            // only reachable through chain building, and has_sibling_chains()
            // requires sibling_v3 AND termtexts.
            posmap: None, bytemap: None, word_sfxpost: None,
            sibling_v3: sibling_data
                .and_then(crate::suffix_fst::sibling_table::SiblingTableReader::open),
            termtexts: termtexts_data
                .and_then(crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open),
        };
        let matches = composite::find_literal_v3(
            &ctx, viable[lit_idx], anchor_start && lit_idx == 0, strict_sep,
        );

        if doc_filter.is_none() && !matches.is_empty() {
            doc_filter = Some(matches.iter().map(|m| m.doc_id).collect());
        }

        if diag {
            let docs: HashSet<DocId> = matches.iter().map(|m| m.doc_id).collect();
            eprintln!("[rx]   literal {:?} -> {} matches in {} docs",
                viable[lit_idx], matches.len(), docs.len());
        }
        all_matches[lit_idx] = matches;
    }

    // Step 3: intersect by doc (position ordered)
    let start_state = automaton.start();

    let has_any_dfa_gap = analyzed_gaps.iter()
        .any(|g| matches!(g, crate::query::phrase_query::regex_gap_analyzer::GapKind::DfaValidation));
    let all_accept = !has_any_dfa_gap && analyzed_gaps.iter()
        .all(|g| matches!(g, crate::query::phrase_query::regex_gap_analyzer::GapKind::AcceptAnything));

    // Single character-class gap between exactly two literals: validate the gap by
    // its byte class instead of walking a whole-input DFA over full tokens.
    let class_ranges: Option<Vec<(u8, u8)>> = if !has_any_dfa_gap && analyzed_gaps.len() == 1 {
        match &analyzed_gaps[0] {
            crate::query::phrase_query::regex_gap_analyzer::GapKind::ByteRangeCheck(r) =>
                Some(r.clone()),
            _ => None,
        }
    } else { None };

    if diag {
        eprintln!("[rx]   has_any_dfa_gap={has_any_dfa_gap} all_accept={all_accept} n_literals={}",
            all_matches.len());
    }

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
                        // Check BEFORE consuming more: tantivy_fst::Regex is a
                        // whole-input automaton, so a match that ends mid-token is
                        // destroyed by the bytes that follow it. Substring search
                        // needs the leftmost accepting prefix, not the full token.
                        if automaton.is_match(&state) { break; }
                        state = automaton.accept(&state, byte);
                        if !automaton.can_match(&state) { alive = false; break; }
                    }
                    if !alive && !automaton.is_match(&state) { continue; }
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
        if diag {
            eprintln!("[rx]   ordered intersection -> {} candidate docs", ordered.len());
        }
        let mut rej_no_posmap = 0usize;
        let mut rej_first_token = 0usize;
        let mut rej_walk = 0usize;
        let mut acc_first = 0usize;
        let mut acc_walk = 0usize;
        let mut acc_class = 0usize;
        let mut rej_class = 0usize;

        if all_accept {
            // `.*` gaps: ordering alone says nothing about what lies between the
            // literals, so this path is a pure over-approximation. Verify when we
            // can rebuild the text; without a posmap there is nothing to check
            // against and the old permissive behaviour stands.
            for &(doc_id, first_bf, last_bt, _) in &ordered {
                if let (Some(rx), Some(pm)) = (&verifier, &posmap) {
                    let first_pos = grouped[0].get(&doc_id)
                        .and_then(|v| v.iter().find(|&&(_, bf, _, _)| bf == first_bf))
                        .map(|&(p, _, _, _)| p);
                    let last_pos = grouped.last().unwrap().get(&doc_id)
                        .and_then(|v| v.iter().find(|&&(_, _, bt, _)| bt == last_bt))
                        .map(|&(p, _, _, _)| p);
                    if let (Some(fp), Some(lp)) = (first_pos, last_pos) {
                        let span = reconstruct_span(pm, ord_to_term, &own_len_of,
                            doc_id, fp, lp);
                        if !rx.is_match(&span) { continue; }
                    }
                }
                doc_bitset.insert(doc_id);
                highlights.push((doc_id, first_bf as usize, last_bt as usize));
            }
        } else if let (Some(ranges), Some(pm), Some(bm)) =
            (class_ranges.as_ref(), &posmap, &bytemap)
        {
            let first_lit_len = viable[0].len();
            for &(doc_id, first_bf, last_bt, first_si) in &ordered {
                let first_entry = grouped[0].get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, bf, _, _)| bf == first_bf));
                let Some(&(first_pos, _, _, _)) = first_entry else { continue; };
                let last_entry = grouped.last().unwrap().get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, _, bt, _)| bt == last_bt));
                let (last_pos, last_si) = last_entry
                    .map(|&(p, _, _, si)| (p, si))
                    .unwrap_or((first_pos, 0));

                // The gap runs from the end of the first literal to the start of the
                // last one. Checking only the tokens strictly between them leaves the
                // bytes inside the two boundary tokens unverified — and when the
                // literals are adjacent or share a token, nothing is checked at all.
                let in_class = |txt: &str| txt.bytes().all(|b| byte_in_ranges(b, ranges));
                let slice_ok = |ord: u32, from: usize, to: usize| -> bool {
                    match ord_to_term(ord as u64) {
                        Some(t) => {
                            let end = to.min(t.len());
                            from >= end || in_class(&t[from.min(end)..end])
                        }
                        None => false,
                    }
                };

                let mut ok = true;
                if first_pos == last_pos {
                    // Both literals inside one token: check the span between them.
                    if let Some(ord) = pm.ordinal_at(doc_id, first_pos) {
                        ok = slice_ok(ord, first_si as usize + first_lit_len, last_si as usize);
                    } else { ok = false; }
                } else {
                    // Tail of the first token, after the literal, up to own_len.
                    if let Some(ord) = pm.ordinal_at(doc_id, first_pos) {
                        let stop = own_len_of(ord as u64).unwrap_or(usize::MAX);
                        ok = slice_ok(ord, first_si as usize + first_lit_len, stop);
                    } else { ok = false; }
                    // Head of the last token, before the literal.
                    if ok {
                        if let Some(ord) = pm.ordinal_at(doc_id, last_pos) {
                            ok = slice_ok(ord, 0, last_si as usize);
                        } else { ok = false; }
                    }
                }

                // Whole tokens strictly between: the bitmap answers without reading
                // the text.
                if ok {
                    for pos in (first_pos + 1)..last_pos {
                        match pm.ordinal_at(doc_id, pos) {
                            Some(ord) if all_bytes_in_ranges(bm, ord, ranges) => {}
                            _ => { ok = false; break; }
                        }
                    }
                }
                if ok {
                    if let Some(rx) = &verifier {
                        let span = reconstruct_span(pm, ord_to_term, &own_len_of,
                            doc_id, first_pos, last_pos);
                        ok = rx.is_match(&span);
                    }
                }
                if ok {
                    acc_class += 1;
                    doc_bitset.insert(doc_id);
                    highlights.push((doc_id, first_bf as usize, last_bt as usize));
                } else {
                    rej_class += 1;
                }
            }
        } else if let Some(pm) = &posmap {
            for &(doc_id, first_bf, last_bt, first_si) in &ordered {
                let first_entry = grouped[0].get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, bf, _, _)| bf == first_bf));
                let Some(&(first_pos, _, _, _)) = first_entry else { rej_no_posmap += 1; continue; };

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
                            if automaton.is_match(&state) { break; }
                            state = automaton.accept(&state, byte);
                            if !automaton.can_match(&state) { alive = false; break; }
                        }
                    } else { rej_no_posmap += 1; continue; }
                } else { rej_no_posmap += 1; continue; }

                if !alive && !automaton.is_match(&state) { rej_first_token += 1; continue; }

                if automaton.is_match(&state) {
                    acc_first += 1;
                    doc_bitset.insert(doc_id);
                    highlights.push((doc_id, first_bf as usize, last_bt as usize));
                    continue;
                }

                // Walk DFA through remaining tokens
                if last_pos > first_pos {
                    match validate_path_v3(
                        automaton, &state, pm, ord_to_term,
                        doc_id, first_pos, last_pos,
                        bytemap.as_ref(),
                    ) {
                        Some(final_state) if automaton.is_match(&final_state) => {
                            acc_walk += 1;
                            doc_bitset.insert(doc_id);
                            highlights.push((doc_id, first_bf as usize, last_bt as usize));
                        }
                        _ => { rej_walk += 1; }
                    }
                } else {
                    rej_walk += 1;
                }
            }
        }
        if diag {
            eprintln!("[rx]   accepted: first_token={acc_first} walk={acc_walk} class={acc_class} | \
rejected: no_posmap={rej_no_posmap} first_token_dfa={rej_first_token} walk={rej_walk} class={rej_class}");
        }
    }

    if diag {
        eprintln!("[rx]   FINAL -> {} docs\n", doc_bitset.len());
    }

    (doc_bitset, highlights)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::briques::context::BriquesContext;
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
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(text, content_ord as u64, meta.own_len, meta.sep_len,
                meta.overlap_len, meta.is_word_start);
        }
        let (fst_data, parent_data) = builder.build().unwrap();
        let num_terms = data.num_content_ords;
        let mut post_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (content_ord, postings) in data.content_postings.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in postings {
                post_writer.add_entry(content_ord as u32, doc_id, ti, bf, bt);
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
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // Verify literals are findable
        let matches = composite::find_literal_v3(&ctx, "mutex", false, true);
        assert!(!matches.is_empty(), "literal 'mutex' should be found");
        assert_eq!(matches[0].doc_id, 0);

        let matches = composite::find_literal_v3(&ctx, "lock", false, true);
        assert!(!matches.is_empty(), "literal 'lock' should be found");
    }

    #[test]
    fn test_intersect_ordered() {
        let (sfx, post) = build_index(&["mutex_lock_init"]);
        let reader = SfxFileReaderV3::open(&sfx).unwrap();
        let resolver = MockResolver::new(&post);
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        let m1 = composite::find_literal_v3(&ctx, "mutex", false, true);
        let m2 = composite::find_literal_v3(&ctx, "init", false, true);

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
        let ctx = BriquesContext {
            reader: &reader, resolver: &resolver, filter_docs: None,
            debug: false,
            trace_id: None,
            posmap: None, bytemap: None, word_sfxpost: None, sibling_v3: None, termtexts: None,
        };

        // "mutex" in doc 0, "world" in doc 1 → no intersection
        let m1 = composite::find_literal_v3(&ctx, "mutex", false, true);
        let m2 = composite::find_literal_v3(&ctx, "world", false, true);

        let g1 = group_by_doc_v3(&m1);
        let g2 = group_by_doc_v3(&m2);

        let ordered = intersect_ordered_v3(&[g1, g2]);
        assert!(ordered.is_empty(), "mutex and world are in different docs");
    }
}
