//! RegexContinuationQuery — regex/fuzzy search across token boundaries via
//! chained DFA walks through the suffix FST and GapMap.
//!
//! Instead of matching a regex against individual tokens, this query walks
//! the DFA through token → gap → token → gap chains, finding matches that
//! span multiple tokens without ever touching stored text.

use std::collections::HashMap;
use std::sync::Arc;

use common::BitSet;
use levenshtein_automata::LevenshteinAutomatonBuilder;
use lucivy_fst::Automaton;
use once_cell::sync::OnceCell;
use tantivy_fst::Regex;

use crate::index::SegmentReader;
use crate::query::automaton_weight::SfxAutomatonAdapter;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::{BitSetDocSet, ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use crate::schema::Field;
use crate::suffix_fst::file::{SfxDfaWrapper, SfxFileReader};
use crate::suffix_fst::gapmap::is_value_boundary;
use crate::store::StoreReader;
use crate::suffix_fst::SfxTermDictionary;
use crate::{DocId, LucivyError, Score};

/// Mode controls where the regex can match relative to the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMode {
    /// Regex can match anywhere (any SI for initial walk).
    Contains,
    /// Regex must match from the start of the first token (SI=0 only).
    StartsWith,
}

/// Maximum continuation depth (token boundaries to traverse).
const MAX_CONTINUATION_DEPTH: usize = 64;

/// Cached LevenshteinAutomatonBuilder.
fn get_builder(distance: u8) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [OnceCell<LevenshteinAutomatonBuilder>; 4] = [
        OnceCell::new(),
        OnceCell::new(),
        OnceCell::new(),
        OnceCell::new(),
    ];
    BUILDERS[distance as usize].get_or_init(|| LevenshteinAutomatonBuilder::new(distance, true))
}

/// What kind of DFA to build for the continuation walk.
#[derive(Debug, Clone)]
enum DfaKind {
    /// Levenshtein DFA: exact or fuzzy match on a literal string.
    Fuzzy { text: String, distance: u8, prefix: bool },
    /// Regex DFA: compile a regex pattern into an automaton.
    Regex { pattern: String },
}

/// A query that matches a DFA (Levenshtein or regex) across token boundaries
/// by chaining suffix FST walks through GapMap separators.
#[derive(Debug, Clone)]
pub struct RegexContinuationQuery {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl RegexContinuationQuery {
    /// Continuation with Levenshtein DFA (exact or fuzzy).
    pub fn new(field: Field, query_text: String, mode: ContinuationMode) -> Self {
        Self {
            field,
            dfa_kind: DfaKind::Fuzzy { text: query_text, distance: 0, prefix: false },
            mode,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Set fuzzy distance for Levenshtein mode.
    pub fn with_fuzzy_distance(mut self, dist: u8) -> Self {
        if let DfaKind::Fuzzy { ref mut distance, .. } = self.dfa_kind {
            *distance = dist;
        }
        self
    }

    /// Use prefix DFA (accepts when target is consumed, ignores remaining input).
    /// Needed for startsWith queries.
    pub fn with_prefix(mut self) -> Self {
        if let DfaKind::Fuzzy { ref mut prefix, .. } = self.dfa_kind {
            *prefix = true;
        }
        self
    }

    /// Continuation with a regex pattern DFA.
    pub fn from_regex(field: Field, pattern: String, mode: ContinuationMode) -> Self {
        Self {
            field,
            dfa_kind: DfaKind::Regex { pattern },
            mode,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Attach a highlight sink for collecting byte offsets of matches.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
}

impl Query for RegexContinuationQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        Ok(Box::new(RegexContinuationWeight {
            field: self.field,
            dfa_kind: self.dfa_kind.clone(),
            mode: self.mode,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
        }))
    }
}

struct RegexContinuationWeight {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

/// Candidate state: DFA end state + byte_from of match start for highlights.
#[derive(Clone)]
struct CandidateState<S> {
    dfa_state: S,
    byte_from: u32,
}

use crate::query::posting_resolver::{self, PostingResolver};

/// Run the continuation algorithm with a given automaton on a segment.
/// Returns (doc_bitset, highlights) where highlights = Vec<(doc_id, byte_from, byte_to)>.
///
/// Verify a DFA match by reading the stored text from `byte_from` onwards.
/// Returns `Some(byte_to)` if the DFA accepts, `None` otherwise.
#[allow(dead_code)]
fn store_verify_dfa<A: Automaton>(
    store: &StoreReader,
    field: Field,
    doc_id: DocId,
    byte_from: usize,
    automaton: &A,
    start_state: &A::State,
) -> Option<usize>
where
    A::State: Clone,
{
    let doc = store.get::<crate::LucivyDocument>(doc_id).ok()?;
    for (f, val) in doc.field_values() {
        if f == field {
            use crate::schema::document::Value;
            if let Some(text) = val.as_value().as_str() {
                let text_lower = text.to_lowercase();
                if byte_from >= text_lower.len() { continue; }
                let bytes = text_lower[byte_from..].as_bytes();
                let mut state = start_state.clone();
                for (i, &byte) in bytes.iter().enumerate() {
                    state = automaton.accept(&state, byte);
                    if automaton.is_match(&state) {
                        return Some(byte_from + i + 1);
                    }
                    if !automaton.can_match(&state) {
                        break;
                    }
                }
            }
        }
    }
    None
}

/// Candidate for sibling-based continuation: tracks the current token's ordinal
/// so we can follow sibling links instead of re-scanning the SFX FST.
#[derive(Clone)]
struct SiblingCandidateState<S> {
    dfa_state: S,
    byte_from: u32,
    raw_ordinal: u64,
}

/// Minimum literal length to use prefix_walk optimization.
/// Below this threshold, prefix_walk returns too many candidates.
const MIN_LITERAL_LEN: usize = 3;

/// Extract ALL literal fragments from a regex pattern, in order.
/// Splits on metacharacters, skips character classes, handles escapes.
fn extract_all_literals(pattern: &str) -> Vec<String> {
    let mut fragments: Vec<String> = Vec::new();
    let mut current = String::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' if i + 1 < bytes.len() => {
                let next = bytes[i + 1];
                if matches!(next, b'.' | b'[' | b']' | b'(' | b')' | b'{' | b'}'
                    | b'|' | b'^' | b'$' | b'\\' | b'*' | b'+' | b'?') {
                    current.push(next as char);
                    i += 2;
                } else {
                    if !current.is_empty() {
                        fragments.push(std::mem::take(&mut current));
                    }
                    i += 2;
                }
            }
            b'[' => {
                if !current.is_empty() {
                    fragments.push(std::mem::take(&mut current));
                }
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
            }
            b'.' | b'*' | b'+' | b'?' | b'(' | b')'
            | b'{' | b'}' | b'|' | b'^' | b'$'
            | b' ' | b'\t' | b'\n' | b'\r' => {
                // Spaces/whitespace are not regex metacharacters but they ARE
                // token separators — no single SFX entry spans across spaces.
                if !current.is_empty() {
                    fragments.push(std::mem::take(&mut current));
                }
                i += 1;
            }
            _ => {
                current.push(b as char);
                i += 1;
            }
        }
    }
    if !current.is_empty() {
        fragments.push(current);
    }

    fragments.into_iter().map(|f| f.to_lowercase()).collect()
}

/// Pick the best literal for the primary prefix_walk.
/// Prefer the prefix (first fragment at start of pattern), fall back to longest.
/// Returns (literal, is_prefix).
fn pick_best_literal(pattern: &str, fragments: &[String]) -> (String, bool) {
    if fragments.is_empty() {
        return (String::new(), false);
    }
    // Prefer first fragment if it's at the pattern start and long enough.
    let first = &fragments[0];
    let first_is_prefix = pattern.to_lowercase().starts_with(first.as_str());
    if first_is_prefix && first.len() >= MIN_LITERAL_LEN {
        return (first.clone(), true);
    }
    // Fall back to longest.
    let best = fragments.iter().max_by_key(|f| f.len()).unwrap();
    (best.clone(), false)
}

/// Fast regex contains via literal extraction + prefix_walk + DFA validation.
///
/// Resolve-last architecture (same pattern as cross_token_search_with_terms):
/// 1. Extract longest literal from regex → prefix_walk (targeted, not full FST scan)
/// 2. DFA validation at ordinal level (no posting resolve) — full entry text from start state
/// 3. Gap=0 sibling chain at ordinal level (no posting resolve)
/// 4. Resolve postings only for validated matches, verify adjacency + byte continuity
/// 5. Gap>0 cross-token: resolve first ordinal, read GapMap, validate DFA, check adjacency
///
/// Falls back to continuation_score_sibling when no usable literal found.
pub(crate) fn regex_contains_via_literal<A: Automaton>(
    automaton: &A,
    pattern: &str,
    sfx_dict: &SfxTermDictionary,
    resolver: &dyn PostingResolver,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    use std::time::Instant;

    let t_total = Instant::now();
    let all_literals = extract_all_literals(pattern);
    let si_zero_only = mode != ContinuationMode::Contains;

    // Viable literals for prefix_walk.
    let viable: Vec<&String> = all_literals.iter()
        .filter(|l| l.len() >= MIN_LITERAL_LEN)
        .collect();

    if viable.is_empty() {
        eprintln!("[regex-timer] FALLBACK no viable literal for '{}'", pattern);
        return continuation_score_sibling(
            automaton, sfx_dict, resolver, sfx_reader, mode, max_doc, ord_to_term,
        );
    }

    // ── Optim 1: pick primary by doc_freq (most selective literal) ──
    let t0 = Instant::now();

    // prefix_walk each viable literal, compute estimated doc_freq.
    struct LiteralInfo {
        literal: String,
        is_prefix: bool,
        walk: Vec<(String, Vec<crate::suffix_fst::builder::ParentEntry>)>,
        ordinals: Vec<u64>,
        est_freq: u32,
    }
    let mut literal_infos: Vec<LiteralInfo> = Vec::new();
    for lit in &viable {
        let walk = if si_zero_only {
            sfx_reader.prefix_walk_si0(lit)
        } else {
            sfx_reader.prefix_walk(lit)
        };
        let ordinals: Vec<u64> = walk.iter()
            .flat_map(|(_, parents)| parents.iter().map(|p| p.raw_ordinal))
            .collect();
        let est_freq: u32 = ordinals.iter()
            .map(|&ord| resolver.doc_freq(ord))
            .sum();
        let is_prefix = pattern.to_lowercase().starts_with(lit.as_str());
        literal_infos.push(LiteralInfo {
            literal: (*lit).clone(), is_prefix, walk, ordinals, est_freq,
        });
    }

    // The DFA walk literal must be the FIRST literal in regex order (or a prefix).
    // The most selective literal is used for intersection pre-filtering only.
    // Walk literal = first viable literal in regex order.
    // Filter literal = most selective (lowest doc_freq).
    let walk_idx = 0; // first literal in regex order
    let filter_idx = literal_infos.iter()
        .enumerate()
        .min_by_key(|(_, li)| li.est_freq)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let walk_info = &literal_infos[walk_idx];
    let literal = walk_info.literal.clone();
    let is_prefix = walk_info.is_prefix;
    let walk_results = walk_info.walk.clone();
    let walk_us = t0.elapsed().as_micros();

    if walk_results.is_empty() {
        eprintln!("[regex-timer] FALLBACK prefix_walk empty for '{}' (literal='{}')", pattern, literal);
        return continuation_score_sibling(
            automaton, sfx_dict, resolver, sfx_reader, mode, max_doc, ord_to_term,
        );
    }

    eprintln!("[regex-timer] walk='{}' freq={} is_prefix={}, filter='{}' freq={} (from {} viable)",
        literal, walk_info.est_freq, is_prefix,
        literal_infos[filter_idx].literal, literal_infos[filter_idx].est_freq,
        literal_infos.len());

    // ── Optim 2: multi-literal intersection via has_doc (no full resolve) ──
    // Use the most selective literal as the base set, then has_doc filter with all others.
    let has_multi_literal = literal_infos.len() > 1;
    let mut allowed_docs: Option<std::collections::HashSet<DocId>> = None;
    let mut other_postings_by_doc: Vec<HashMap<DocId, Vec<(u32, u32, u32)>>> = Vec::new();

    if has_multi_literal {
        let filter_info = &literal_infos[filter_idx];

        // Resolve the most selective literal to get the smallest doc set.
        let mut base_docs: std::collections::HashSet<DocId> = std::collections::HashSet::new();
        for &ord in &filter_info.ordinals {
            for e in &resolver.resolve(ord) {
                base_docs.insert(e.doc_id);
            }
        }

        // has_doc filter: for each other literal, retain only docs that contain it.
        let mut survivors = base_docs.clone();
        for (i, li) in literal_infos.iter().enumerate() {
            if i == filter_idx { continue; }
            survivors.retain(|&doc_id| {
                li.ordinals.iter().any(|&ord| resolver.has_doc(ord, doc_id))
            });
        }

        // Resolve postings of non-walk literals for position ordering check.
        // Use resolve_filtered to skip eliminated docs.
        // Collect in REGEX ORDER (all literals except the walk literal).
        for (i, li) in literal_infos.iter().enumerate() {
            if i == walk_idx { continue; }
            let mut by_doc: HashMap<DocId, Vec<(u32, u32, u32)>> = HashMap::new();
            for &ord in &li.ordinals {
                for e in &resolver.resolve_filtered(ord, &survivors) {
                    by_doc.entry(e.doc_id).or_default().push((
                        e.position, e.byte_from, e.byte_to,
                    ));
                }
            }
            other_postings_by_doc.push(by_doc);
        }

        eprintln!("[regex-timer] multi-literal: base_docs({})={} → survivors={} (has_doc filter)",
            filter_info.literal, base_docs.len(), survivors.len());
        allowed_docs = Some(survivors);
    }

    let walk_entries = walk_results.len();
    let walk_parents: usize = walk_results.iter().map(|(_, p)| p.len()).sum();

    // ═══════════════════════════════════════════════════════════════════
    // Phase 1: DFA validation at ORDINAL level — zero posting resolves.
    // ═══════════════════════════════════════════════════════════════════
    let t1 = Instant::now();

    // Single-token matches: DFA accepted within the token.
    let mut accepted: Vec<(u64, u16)> = Vec::new(); // (raw_ordinal, si)

    // Cross-token candidates: DFA alive but NOT accepting at token boundary.
    // Keyed by raw_ordinal (deduped) since DFA state is deterministic per entry_text.
    let mut cross_token: Vec<(u64, u16, A::State)> = Vec::new(); // (raw_ordinal, si, dfa_state)
    let mut cross_token_seen: std::collections::HashSet<u64> = std::collections::HashSet::new();

    let start_state = automaton.start();

    // When literal is the regex prefix, we can pre-compute the DFA state after
    // consuming the literal and only feed the remaining entry bytes.
    let prefix_state = if is_prefix {
        let mut s = start_state.clone();
        for &byte in literal.as_bytes() {
            s = automaton.accept(&s, byte);
        }
        Some(s)
    } else {
        None
    };

    // Dedup: avoid processing the same ordinal multiple times (different SIs share same token).
    let mut validated_ordinals: HashMap<u64, Option<(A::State, bool)>> = HashMap::new();

    for (entry_text, parents) in &walk_results {
        for parent in parents {
            // Check cache first
            if let Some(cached) = validated_ordinals.get(&parent.raw_ordinal) {
                if let Some((state, is_acc)) = cached {
                    if *is_acc {
                        accepted.push((parent.raw_ordinal, parent.si));
                    }
                    if !*is_acc && automaton.can_match(state) {
                        if cross_token_seen.insert(parent.raw_ordinal) {
                            cross_token.push((parent.raw_ordinal, parent.si, state.clone()));
                        }
                    }
                }
                continue;
            }

            // Validate: feed text through DFA.
            let (state, alive) = if let Some(ref ps) = prefix_state {
                // Prefix literal: skip literal bytes, feed remaining entry_text bytes.
                let remaining = &entry_text[literal.len()..];
                let mut s = ps.clone();
                let mut ok = true;
                for &byte in remaining.as_bytes() {
                    s = automaton.accept(&s, byte);
                    if !automaton.can_match(&s) { ok = false; break; }
                }
                (s, ok)
            } else {
                // Non-prefix literal: feed FULL token text from start state.
                let full_text = match ord_to_term(parent.raw_ordinal) {
                    Some(t) => t,
                    None => {
                        validated_ordinals.insert(parent.raw_ordinal, None);
                        continue;
                    }
                };
                let mut s = start_state.clone();
                let mut ok = true;
                for &byte in full_text.as_bytes() {
                    s = automaton.accept(&s, byte);
                    if !automaton.can_match(&s) { ok = false; break; }
                }
                (s, ok)
            };

            if !alive {
                validated_ordinals.insert(parent.raw_ordinal, None);
                continue;
            }

            let is_accepting = automaton.is_match(&state);
            validated_ordinals.insert(parent.raw_ordinal, Some((state.clone(), is_accepting)));

            if is_accepting {
                accepted.push((parent.raw_ordinal, parent.si));
            }
            if !is_accepting && automaton.can_match(&state) {
                if cross_token_seen.insert(parent.raw_ordinal) {
                    cross_token.push((parent.raw_ordinal, parent.si, state));
                }
            }
        }
    }

    let phase1_us = t1.elapsed().as_micros();

    // ═══════════════════════════════════════════════════════════════════
    // Phase 2: Gap=0 sibling chain at ORDINAL level — zero posting resolves.
    // ═══════════════════════════════════════════════════════════════════
    let t2 = Instant::now();

    // A valid chain: sequence of ordinals where gap=0 sibling links connect tokens
    // and the DFA accepts at the end of the chain.
    // Format: (first_ordinal, first_si, chain_ordinals_including_first)
    let mut valid_gap0_chains: Vec<(u64, u16, Vec<u64>)> = Vec::new();

    // Gap>0 candidates: ordinals needing per-document GapMap reads.
    let mut gap_candidates: Vec<(u64, u16, A::State)> = Vec::new();

    if let Some(sib_table) = sfx_reader.sibling_table() {
        for &(ord, si, ref dfa_state) in &cross_token {
            let has_gap_siblings = sib_table
                .siblings(ord as u32)
                .iter()
                .any(|s| s.gap_len > 0);
            if has_gap_siblings {
                gap_candidates.push((ord, si, dfa_state.clone()));
            }

            // Follow gap=0 chain
            let mut current_ord = ord;
            let mut state = dfa_state.clone();
            let mut chain = vec![current_ord];

            for _depth in 0..MAX_CONTINUATION_DEPTH {
                let siblings = sib_table.contiguous_siblings(current_ord as u32);
                let mut advanced = false;

                for next_ord in &siblings {
                    let next_text = match ord_to_term(*next_ord as u64) {
                        Some(t) => t,
                        None => continue,
                    };

                    let mut new_state = state.clone();
                    let mut alive = true;
                    for &byte in next_text.as_bytes() {
                        new_state = automaton.accept(&new_state, byte);
                        if !automaton.can_match(&new_state) {
                            alive = false;
                            break;
                        }
                    }
                    if !alive {
                        continue;
                    }

                    chain.push(*next_ord as u64);

                    if automaton.is_match(&new_state) {
                        valid_gap0_chains.push((ord, si, chain.clone()));
                        // Don't continue past acceptance
                        advanced = false;
                        break;
                    }

                    if automaton.can_match(&new_state) {
                        current_ord = *next_ord as u64;
                        state = new_state;
                        advanced = true;
                        break; // take first viable sibling, continue chain
                    }
                }

                if !advanced {
                    break;
                }
            }
        }
    }

    let phase2_us = t2.elapsed().as_micros();

    // ═══════════════════════════════════════════════════════════════════
    // Phase 3: Resolve postings for validated matches only.
    // ═══════════════════════════════════════════════════════════════════
    let t3a = Instant::now();

    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();

    // ── Optim 3: resolve_filtered when allowed_docs is set ──
    // 3a. Single-token accepted ordinals
    accepted.sort_by_key(|a| a.0);
    accepted.dedup();
    for &(ord, si) in &accepted {
        let entries = if let Some(ref allowed) = allowed_docs {
            resolver.resolve_filtered(ord, allowed)
        } else {
            resolver.resolve(ord)
        };
        for e in &entries {
            doc_bitset.insert(e.doc_id);
            let byte_from = (e.byte_from + si as u32) as usize;
            highlights.push((e.doc_id, byte_from, e.byte_to as usize));
        }
    }

    let phase3a_us = t3a.elapsed().as_micros();
    let t3b = Instant::now();

    // 3b. Gap=0 cross-token chains — resolve + adjacency + byte continuity
    if !valid_gap0_chains.is_empty() {
        let mut ordinal_cache: HashMap<u64, Vec<posting_resolver::PostingEntry>> = HashMap::new();
        for (_, _, chain) in &valid_gap0_chains {
            for &ord in chain {
                ordinal_cache
                    .entry(ord)
                    .or_insert_with(|| resolver.resolve(ord));
            }
        }

        for (_, first_si, chain) in &valid_gap0_chains {
            let first_postings = match ordinal_cache.get(&chain[0]) {
                Some(p) => p,
                None => continue,
            };

            // active: (doc_id, next_expected_pos, next_byte_from, highlight_byte_from)
            let mut active: Vec<(DocId, u32, u32, u32)> = Vec::new();
            for p in first_postings {
                let byte_from = p.byte_from + *first_si as u32;
                active.push((p.doc_id, p.position + 1, p.byte_to, byte_from));
            }

            for &ord in &chain[1..] {
                if active.is_empty() {
                    break;
                }
                let postings = match ordinal_cache.get(&ord) {
                    Some(p) => p,
                    None => {
                        active.clear();
                        break;
                    }
                };

                active.sort_by_key(|a| (a.0, a.1));
                let mut next_active: Vec<(DocId, u32, u32, u32)> = Vec::new();
                for p in postings {
                    let target = (p.doc_id, p.position);
                    let idx = active.partition_point(|a| (a.0, a.1) < target);
                    let mut i = idx;
                    while i < active.len() && active[i].0 == p.doc_id && active[i].1 == p.position
                    {
                        // Byte continuity: next token starts where previous ended
                        if p.byte_from == active[i].2 {
                            next_active.push((
                                p.doc_id,
                                p.position + 1,
                                p.byte_to,
                                active[i].3,
                            ));
                        }
                        i += 1;
                    }
                }
                active = next_active;
            }

            for &(doc_id, _, byte_to, byte_from) in &active {
                doc_bitset.insert(doc_id);
                highlights.push((doc_id, byte_from as usize, byte_to as usize));
            }
        }
    }

    let phase3b_us = t3b.elapsed().as_micros();
    let t3c = Instant::now();

    // 3c-pre: Position ordering pre-filter for multi-literal.
    // Keep only docs where ALL literals appear in regex order.
    // The primary literal is first (or best). Other literals must appear after it in order.
    //
    // For "rag.*ver.*end" with literals ["rag","ver","end"]:
    //   primary = "rag", others = ["ver", "end"] (in regex order)
    //   A doc matches only if ∃ P_rag < P_ver < P_end.
    if has_multi_literal && !other_postings_by_doc.is_empty() {
        // Resolve primary literal positions (from both accepted + cross_token ordinals).
        let mut primary_by_doc: HashMap<DocId, Vec<u32>> = HashMap::new();
        let mut all_primary_ords: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &(ord, _, _) in &cross_token {
            all_primary_ords.insert(ord);
        }
        for &(ord, _) in &accepted {
            all_primary_ords.insert(ord);
        }
        for ord in &all_primary_ords {
            let entries = resolver.resolve(*ord);
            for e in &entries {
                if let Some(ref allowed) = allowed_docs {
                    if !allowed.contains(&e.doc_id) { continue; }
                }
                primary_by_doc.entry(e.doc_id).or_default().push(e.position);
            }
        }

        // For each doc, check: exists a chain where each literal's byte_from >= previous literal's byte_to.
        // This ensures literals don't overlap and appear in the correct order.
        let mut position_filtered = std::collections::HashSet::new();

        // Collect primary (byte_from, byte_to) per doc.
        let mut primary_bytes: HashMap<DocId, Vec<(u32, u32)>> = HashMap::new();
        for ord in &all_primary_ords {
            for e in &resolver.resolve(*ord) {
                if let Some(ref allowed) = allowed_docs {
                    if !allowed.contains(&e.doc_id) { continue; }
                }
                primary_bytes.entry(e.doc_id).or_default().push((e.byte_from, e.byte_to));
            }
        }

        for (&doc_id, byte_ranges) in &primary_bytes {
            'outer: for &(_, primary_byte_to) in byte_ranges {
                let mut min_byte = primary_byte_to;
                let mut all_ok = true;
                for other_by_doc in &other_postings_by_doc {
                    if let Some(positions) = other_by_doc.get(&doc_id) {
                        // Find earliest occurrence whose byte_from >= min_byte
                        if let Some(&(_, bf, bt)) = positions.iter()
                            .filter(|&&(_, bf, _)| bf >= min_byte)
                            .min_by_key(|&&(_, bf, _)| bf)
                        {
                            min_byte = bt; // next literal must start after this one ends
                        } else {
                            all_ok = false;
                            break;
                        }
                    } else {
                        all_ok = false;
                        break;
                    }
                }
                if all_ok {
                    position_filtered.insert(doc_id);
                    break 'outer;
                }
            }
        }
        eprintln!("[regex-timer] position-order filter: {} → {} docs",
            primary_by_doc.len(), position_filtered.len());
        allowed_docs = Some(position_filtered);
    }

    // 3c: Gap>0 cross-token — runs on position-filtered doc set.
    if !gap_candidates.is_empty() {
        if let Some(sib_table) = sfx_reader.sibling_table() {
            let gapmap = sfx_reader.gapmap();

            for &(ord, si, ref dfa_state) in &gap_candidates {
                // Precompute gap>0 siblings + their text ONCE per ordinal.
                let sibling_texts: Vec<(u32, String)> = sib_table
                    .siblings(ord as u32)
                    .into_iter()
                    .filter(|s| s.gap_len > 0)
                    .filter_map(|s| {
                        ord_to_term(s.next_ordinal as u64)
                            .map(|text| (s.next_ordinal, text))
                    })
                    .collect();

                if sibling_texts.is_empty() {
                    continue;
                }

                let first_entries = if let Some(ref allowed) = allowed_docs {
                    resolver.resolve_filtered(ord, allowed)
                } else {
                    resolver.resolve(ord)
                };

                // Seed: (doc_id, position, byte_from, dfa_state, current_ordinal)
                let mut active: Vec<(DocId, u32, u32, A::State, u64)> = Vec::new();
                for fe in &first_entries {
                    active.push((
                        fe.doc_id,
                        fe.position,
                        fe.byte_from + si as u32,
                        dfa_state.clone(),
                        ord,
                    ));
                }

                for _depth in 0..MAX_CONTINUATION_DEPTH {
                    if active.is_empty() {
                        break;
                    }

                    let mut next_active: Vec<(DocId, u32, u32, A::State, u64)> = Vec::new();

                    for &(doc_id, pos, byte_from, ref state, cur_ord) in &active {
                        let gap_bytes = gapmap.read_separator(doc_id, pos, pos + 1);
                        let gap_bytes = match gap_bytes {
                            Some(g) if !is_value_boundary(g) => g,
                            _ => continue,
                        };

                        // Empty gap = contiguous tokens → already handled by Phase 2/3b.
                        if gap_bytes.is_empty() {
                            continue;
                        }

                        // Feed gap bytes ONCE per doc — if DFA dies, skip ALL siblings.
                        let mut gap_state = state.clone();
                        let mut gap_alive = true;
                        for &byte in gap_bytes {
                            gap_state = automaton.accept(&gap_state, byte);
                            if !automaton.can_match(&gap_state) {
                                gap_alive = false;
                                break;
                            }
                        }
                        if !gap_alive {
                            continue;
                        }

                        // Reuse precomputed sibling texts when possible.
                        let owned_sibs;
                        let sibs: &[(u32, String)] = if cur_ord == ord {
                            &sibling_texts
                        } else {
                            // Deeper depth: different ordinal, recompute.
                            owned_sibs = sib_table
                                .siblings(cur_ord as u32)
                                .into_iter()
                                .filter(|s| s.gap_len > 0)
                                .filter_map(|s| {
                                    ord_to_term(s.next_ordinal as u64)
                                        .map(|text| (s.next_ordinal, text))
                                })
                                .collect::<Vec<_>>();
                            &owned_sibs
                        };

                        for (next_ord, next_text) in sibs {
                            let mut s = gap_state.clone();
                            let mut alive = true;
                            for &byte in next_text.as_bytes() {
                                s = automaton.accept(&s, byte);
                                if !automaton.can_match(&s) {
                                    alive = false;
                                    break;
                                }
                            }
                            if !alive {
                                continue;
                            }

                            // Verify position adjacency
                            let next_entries = resolver.resolve(*next_ord as u64);
                            for ne in &next_entries {
                                if ne.doc_id == doc_id && ne.position == pos + 1 {
                                    if automaton.is_match(&s) {
                                        doc_bitset.insert(doc_id);
                                        highlights.push((
                                            doc_id,
                                            byte_from as usize,
                                            ne.byte_to as usize,
                                        ));
                                    }
                                    if automaton.can_match(&s) && !automaton.is_match(&s) {
                                        next_active.push((
                                            doc_id,
                                            ne.position,
                                            byte_from,
                                            s.clone(),
                                            *next_ord as u64,
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    active = next_active;
                }
            }
        }
    }

    let phase3c_us = t3c.elapsed().as_micros();
    let total_us = t_total.elapsed().as_micros();

    eprintln!(
        "[regex-timer] '{}' literal='{}' | walk={}us ({}ent,{}par) | p1={}us ({}acc,{}ct) | p2={}us ({}g0,{}gap) | p3a={}us | p3b={}us | p3c={}us | total={}us | {}docs,{}hl",
        pattern, literal,
        walk_us, walk_entries, walk_parents,
        phase1_us, accepted.len(), cross_token.len(),
        phase2_us, valid_gap0_chains.len(), gap_candidates.len(),
        phase3a_us, phase3b_us, phase3c_us,
        total_us,
        doc_bitset.len(), highlights.len(),
    );

    Ok((doc_bitset, highlights))
}

/// Sibling-accelerated continuation: replaces Walk 2 (DFA × SFX FST) with
/// sibling link lookup + ord_to_term + DFA byte feed. Works for both gap=0
/// (contiguous tokens) and gap>0 (separated tokens via GapMap).
///
/// Falls back to `continuation_score` if no sibling table is available.
pub(crate) fn continuation_score_sibling<A: Automaton>(
    automaton: &A,
    sfx_dict: &SfxTermDictionary,
    resolver: &dyn PostingResolver,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    let sibling_table = match sfx_reader.sibling_table() {
        Some(t) => t,
        None => {
            // No sibling table — fall back to old Walk 2 approach.
            return continuation_score(automaton, sfx_dict, resolver, sfx_reader, mode, max_doc, None);
        }
    };

    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();
    let gapmap = sfx_reader.gapmap();

    // === Walk 1: initial DFA × SFX FST walk (identical to continuation_score) ===
    let si_zero_only = mode != ContinuationMode::Contains;
    let start_state = automaton.start();
    let matches = sfx_dict.search_continuation(automaton, start_state, si_zero_only);

    let mut candidates: HashMap<(DocId, u32), Vec<SiblingCandidateState<A::State>>> = HashMap::new();

    for m in &matches {
        let entries = resolver.resolve(m.raw_ordinal);
        for e in &entries {
            let byte_from = e.byte_from + m.si as u32;

            if m.is_accepting {
                doc_bitset.insert(e.doc_id);
                highlights.push((e.doc_id, byte_from as usize, e.byte_to as usize));
            } else if automaton.can_match(&m.end_state) {
                let states = candidates.entry((e.doc_id, e.position)).or_default();
                let cs = SiblingCandidateState {
                    dfa_state: m.end_state.clone(),
                    byte_from,
                    raw_ordinal: m.raw_ordinal,
                };
                states.push(cs);
            }
        }
    }

    // === Continuation via sibling links ===
    for _depth in 0..MAX_CONTINUATION_DEPTH {
        if candidates.is_empty() {
            break;
        }

        let mut new_candidates: HashMap<(DocId, u32), Vec<SiblingCandidateState<A::State>>> =
            HashMap::new();

        for (&(doc_id, pos), cand_states) in &candidates {
            // Read the gap bytes between pos and pos+1 for this document (once per position).
            let gap_bytes = gapmap.read_separator(doc_id, pos, pos + 1);
            let gap_bytes = match gap_bytes {
                Some(g) if !is_value_boundary(g) => g,
                _ => continue, // no next token or value boundary
            };

            for cs in cand_states {
                let siblings = sibling_table.siblings(cs.raw_ordinal as u32);

                for sib in &siblings {
                    let next_text = match ord_to_term(sib.next_ordinal as u64) {
                        Some(t) => t,
                        None => continue,
                    };

                    // Feed gap bytes to DFA
                    let mut state = cs.dfa_state.clone();
                    let mut alive = true;
                    for &byte in gap_bytes {
                        state = automaton.accept(&state, byte);
                        if !automaton.can_match(&state) {
                            alive = false;
                            break;
                        }
                    }
                    if !alive {
                        continue;
                    }

                    // Feed next token bytes to DFA
                    for &byte in next_text.as_bytes() {
                        state = automaton.accept(&state, byte);
                        if !automaton.can_match(&state) {
                            alive = false;
                            break;
                        }
                    }
                    if !alive {
                        continue;
                    }

                    // Verify position adjacency: next_ordinal must appear at pos+1 in this doc
                    let next_entries = resolver.resolve(sib.next_ordinal as u64);
                    for ne in &next_entries {
                        if ne.doc_id == doc_id && ne.position == pos + 1 {
                            if automaton.is_match(&state) {
                                doc_bitset.insert(doc_id);
                                highlights.push((
                                    doc_id,
                                    cs.byte_from as usize,
                                    ne.byte_to as usize,
                                ));
                            }
                            if automaton.can_match(&state) {
                                let new_cs = SiblingCandidateState {
                                    dfa_state: state.clone(),
                                    byte_from: cs.byte_from,
                                    raw_ordinal: sib.next_ordinal as u64,
                                };
                                new_candidates
                                    .entry((doc_id, ne.position))
                                    .or_default()
                                    .push(new_cs);
                            }
                        }
                    }
                }
            }
        }

        candidates = new_candidates;
    }

    Ok((doc_bitset, highlights))
}

/// Reusable by any query that needs cross-token matching: contains, startsWith, regex.
///
/// `store_dfa_verifier`: optional callback for depth 3+ stored text fallback.
/// Receives (doc_id, byte_from, automaton, dfa_state) and returns true if the
/// stored text confirms the DFA match from that position.
pub(crate) fn continuation_score<A: Automaton>(
    automaton: &A,
    sfx_dict: &SfxTermDictionary,
    resolver: &dyn PostingResolver,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
    store_dfa_verifier: Option<&dyn Fn(DocId, u32, &A, &A::State) -> Option<usize>>,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();
    let gapmap = sfx_reader.gapmap();

    // === Walk 1: initial walk ===
    let si_zero_only = mode != ContinuationMode::Contains;
    let start_state = automaton.start();
    let matches = sfx_dict.search_continuation(automaton, start_state, si_zero_only);

    let mut candidates: HashMap<(DocId, u32), Vec<CandidateState<A::State>>> = HashMap::new();

    for m in &matches {
        let entries = resolver.resolve(m.raw_ordinal);
        for e in &entries {
            let byte_from = e.byte_from + m.si as u32;

            if m.is_accepting {
                doc_bitset.insert(e.doc_id);
                highlights.push((e.doc_id, byte_from as usize, e.byte_to as usize));
            } else if automaton.can_match(&m.end_state) {
                let states = candidates.entry((e.doc_id, e.position)).or_default();
                let cs = CandidateState { dfa_state: m.end_state.clone(), byte_from };
                if !states.iter().any(|s| s.dfa_state == m.end_state) {
                    states.push(cs);
                }
            }
        }
    }

    // === Continuation loop ===
    for depth in 0..MAX_CONTINUATION_DEPTH {
        if candidates.is_empty() {
            break;
        }

        // Depth 3+: fallback to stored text verification if available.
        if depth >= 3 {
            if let Some(verify) = &store_dfa_verifier {
                for (&(doc, _pos), cand_states) in &candidates {
                    for cs in cand_states {
                        if let Some(byte_to) = verify(doc, cs.byte_from, automaton, &cs.dfa_state) {
                            doc_bitset.insert(doc);
                            highlights.push((doc, cs.byte_from as usize, byte_to));
                        }
                    }
                }
                break;
            }
        }

        let mut post_gap: HashMap<A::State, Vec<(DocId, u32, u32)>> = HashMap::new();

        for (&(doc, pos), cand_states) in &candidates {
            let gap = gapmap.read_separator(doc, pos, pos + 1);
            let Some(gap_bytes) = gap else { continue; };
            if is_value_boundary(gap_bytes) { continue; }

            for cs in cand_states {
                let mut state = cs.dfa_state.clone();
                let mut alive = true;
                for &byte in gap_bytes {
                    state = automaton.accept(&state, byte);
                    if !automaton.can_match(&state) { alive = false; break; }
                }
                if !alive { continue; }

                if automaton.is_match(&state) {
                    doc_bitset.insert(doc);
                }
                if automaton.can_match(&state) {
                    post_gap.entry(state).or_default().push((doc, pos + 1, cs.byte_from));
                }
            }
        }

        if post_gap.is_empty() { break; }

        let mut new_candidates: HashMap<(DocId, u32), Vec<CandidateState<A::State>>> = HashMap::new();

        for (gap_state, doc_positions) in &post_gap {
            let next_matches = sfx_dict.search_continuation(automaton, gap_state.clone(), true);

            let candidate_docs: HashMap<DocId, Vec<(u32, u32)>> = {
                let mut map: HashMap<DocId, Vec<(u32, u32)>> = HashMap::new();
                for &(doc, expected_pos, byte_from) in doc_positions {
                    map.entry(doc).or_default().push((expected_pos, byte_from));
                }
                map
            };

            for nm in &next_matches {
                let entries = resolver.resolve(nm.raw_ordinal);
                for e in &entries {
                    if let Some(expected) = candidate_docs.get(&e.doc_id) {
                        for &(exp_pos, byte_from) in expected {
                            if e.position == exp_pos {
                                if nm.is_accepting {
                                    doc_bitset.insert(e.doc_id);
                                    highlights.push((e.doc_id, byte_from as usize, e.byte_to as usize));
                                } else if automaton.can_match(&nm.end_state) {
                                    let states = new_candidates
                                        .entry((e.doc_id, e.position))
                                        .or_default();
                                    let cs = CandidateState {
                                        dfa_state: nm.end_state.clone(),
                                        byte_from,
                                    };
                                    if !states.iter().any(|s| s.dfa_state == nm.end_state) {
                                        states.push(cs);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        candidates = new_candidates;
    }

    Ok((doc_bitset, highlights))
}

impl Weight for RegexContinuationWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let max_doc = reader.max_doc();

        // Open .sfx — if not present, return empty scorer.
        let sfx_data = match reader.sfx_file(self.field) {
            Some(data) => data,
            None => return Ok(Box::new(crate::query::EmptyScorer)),
        };
        let sfx_bytes = sfx_data
            .read_bytes()
            .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
            .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;

        let inverted_index = reader.inverted_index(self.field)?;
        let term_dict = inverted_index.terms();
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, term_dict);

        let resolver = posting_resolver::build_resolver(reader, self.field)?;

        // Build ord_to_term for sibling-accelerated path.
        let ord_to_term_fn = |ord: u64| -> Option<String> {
            let mut bytes = Vec::new();
            if term_dict.ord_to_term(ord, &mut bytes).ok()? {
                String::from_utf8(bytes).ok()
            } else {
                None
            }
        };

        let use_sibling = sfx_reader.sibling_table().is_some();

        let (doc_bitset, highlights) = match &self.dfa_kind {
            DfaKind::Fuzzy { text, distance, prefix } => {
                let builder = get_builder(*distance);
                let dfa = if *prefix {
                    builder.build_prefix_dfa(text)
                } else {
                    builder.build_dfa(text)
                };
                let automaton = SfxDfaWrapper(dfa);
                if use_sibling {
                    continuation_score_sibling(
                        &automaton, &sfx_dict, &*resolver, &sfx_reader, self.mode, max_doc,
                        &ord_to_term_fn,
                    )?
                } else {
                    continuation_score(
                        &automaton, &sfx_dict, &*resolver, &sfx_reader, self.mode, max_doc,
                        None,
                    )?
                }
            }
            DfaKind::Regex { pattern } => {
                let t_dfa = std::time::Instant::now();
                let regex = Regex::new(pattern).map_err(|e| {
                    LucivyError::InvalidArgument(format!("RegexContinuation: {e}"))
                })?;
                let dfa_us = t_dfa.elapsed().as_micros();

                let t_setup = std::time::Instant::now();
                let automaton = SfxAutomatonAdapter(&regex);
                let setup_us = t_setup.elapsed().as_micros();

                eprintln!("[regex-timer] scorer: dfa_compile={}us setup={}us sibling={} pattern='{}'",
                    dfa_us, setup_us, use_sibling, pattern);

                if use_sibling {
                    regex_contains_via_literal(
                        &automaton, pattern, &sfx_dict, &*resolver, &sfx_reader,
                        self.mode, max_doc, &ord_to_term_fn,
                    )?
                } else {
                    continuation_score(
                        &automaton, &sfx_dict, &*resolver, &sfx_reader, self.mode, max_doc,
                        None,
                    )?
                }
            }
        };

        // Report highlights to sink
        if let Some(ref sink) = self.highlight_sink {
            let segment_id = reader.segment_id();
            for &(doc_id, byte_from, byte_to) in &highlights {
                sink.insert(
                    segment_id,
                    doc_id,
                    &self.highlight_field_name,
                    vec![[byte_from, byte_to]],
                );
            }
        }

        let doc_bitset = BitSetDocSet::from(doc_bitset);
        let scorer = ConstScorer::new(doc_bitset, boost);
        Ok(Box::new(scorer))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("RegexContinuationQuery", 1.0))
        } else {
            Err(LucivyError::InvalidArgument(
                "Document does not exist".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::TopDocs;
    use crate::schema::{IndexRecordOption, SchemaBuilder, TextFieldIndexing, TextOptions};
    use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
    use crate::{Index, LucivyDocument};

    fn build_continuation_index() -> (Index, Field) {
        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let field = schema_builder.add_text_field("body._raw", raw_opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        // Doc 0: "import rag3db from core"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "import rag3db from core");
        writer.add_document(doc).unwrap();

        // Doc 1: "rag3db is cool"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "rag3db is cool");
        writer.add_document(doc).unwrap();

        // Doc 2: "nothing here"
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "nothing here");
        writer.add_document(doc).unwrap();

        writer.commit().unwrap();

        (index, field)
    }

    #[test]
    fn test_continuation_single_token_match() {
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 2, "rag3db should match 2 docs");
    }

    #[test]
    fn test_continuation_cross_token_exact() {
        // "rag3db is cool" spans 3 tokens with spaces as gaps
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "should match doc 1 only");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_cross_token_fuzzy() {
        // Fuzzy "rag3db iz cool" d=1 → should match "rag3db is cool"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db iz cool".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "fuzzy d=1 should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_no_match() {
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is warm".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_continuation_contains_mid_token() {
        // "3db is" starts mid-token "rag3db" at SI=3, crosses gap to "is"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "3db is".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "contains '3db is' should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_cross_token() {
        // Regex "rag.db i. cool" — . matches any char, spans 3 tokens
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "rag.db i. cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "regex should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_contains_mid_token() {
        // Regex "3db i." — starts mid-token, crosses gap, . matches 's'
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "3db i.".into(),
            ContinuationMode::Contains,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "regex contains '3db i.' should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_continuation_regex_no_match() {
        // Regex "rag.db x. cool" — 'x.' won't match 'is'
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::from_regex(
            field,
            "rag.db x. cool".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0);
    }

    // ── Fuzzy single-token query → continuation tests ──
    // These verify that a single-token fuzzy query correctly spans token
    // boundaries when the edit distance budget absorbs the gap.

    #[test]
    fn test_fuzzy_single_token_absorbs_gap() {
        // "importrag3db" d=1 → should match "import rag3db" (space = 1 insertion)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "importrag3db".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 0 has "import rag3db from core" — "import rag3db" matches with d=1
        assert_eq!(results.len(), 1, "'importrag3db' d=1 should match 'import rag3db'");
        assert_eq!(results[0].1.doc_id, 0);
    }

    #[test]
    fn test_fuzzy_single_token_no_gap_budget() {
        // "importrag3db" d=0 → should NOT match "import rag3db" (no budget for gap)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "importrag3db".into(),
            ContinuationMode::StartsWith,
        );
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0, "d=0 cannot absorb the gap");
    }

    #[test]
    fn test_fuzzy_single_token_still_matches_single() {
        // "rag3dc" d=1 → should still match "rag3db" (single token, no continuation)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3dc".into(),
            ContinuationMode::Contains,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 2, "fuzzy 'rag3dc' d=1 should match 2 docs with 'rag3db'");
    }

    #[test]
    fn test_fuzzy_contains_absorbs_gap_mid_token() {
        // "3dbis" d=1 contains → "3db" (suffix SI=3) + gap " " (insertion d=1) + "is"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "3dbis".into(),
            ContinuationMode::Contains,
        )
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 1: "rag3db is cool" → suffix "3db" + " " gap + "is"
        assert_eq!(results.len(), 1, "'3dbis' d=1 contains should match doc 1");
        assert_eq!(results[0].1.doc_id, 1);
    }

    #[test]
    fn test_fuzzy_three_tokens_d2() {
        // "rag3dbiscool" d=2 → "rag3db" + " "(d=1) + "is" + " "(d=2) + "cool"
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag3dbiscool".into(),
            ContinuationMode::StartsWith,
        )
        .with_fuzzy_distance(2);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        // Doc 1: "rag3db is cool" — 2 spaces absorbed = d=2
        assert_eq!(results.len(), 1, "'rag3dbiscool' d=2 should match 'rag3db is cool'");
        assert_eq!(results[0].1.doc_id, 1);
    }

    // ── Highlight tests ──

    #[test]
    fn test_highlights_single_token() {
        // "rag3db" exact → highlight [0,6] in doc 1 ("rag3db is cool")
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "rag3db".into(),
            ContinuationMode::StartsWith,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let _ = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        let all = sink.all_entries();
        // Doc 1: "rag3db" at offset [0, 6]
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        assert!(
            doc1_entries.iter().any(|e| e.offsets.contains(&[0, 6])),
            "doc 1 should have highlight [0,6] for 'rag3db', got {:?}", doc1_entries
        );
    }

    #[test]
    fn test_highlights_cross_token() {
        // "rag3db is cool" exact → highlight [0,14] in doc 1
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "rag3db is cool".into(),
            ContinuationMode::StartsWith,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        assert_eq!(results.len(), 1);

        let all = sink.all_entries();
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        // "rag3db is cool" = bytes [0, 14]
        assert!(
            doc1_entries.iter().any(|e| e.offsets.contains(&[0, 14])),
            "doc 1 should have highlight [0,14], got {:?}", doc1_entries
        );
    }

    #[test]
    fn test_highlights_contains_mid_token() {
        // "3db is" contains → highlight starts at byte 3 (SI=3 of "rag3db")
        // "rag3db is cool" → "3db" starts at byte 3, "is" ends at byte 10
        let (index, field) = build_continuation_index();
        let sink = Arc::new(HighlightSink::default());
        let query = RegexContinuationQuery::new(
            field,
            "3db is".into(),
            ContinuationMode::Contains,
        )
        .with_highlight_sink(Arc::clone(&sink), "body._raw".into());

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        assert_eq!(results.len(), 1);

        let all = sink.all_entries();
        let doc1_entries: Vec<_> = all.iter().filter(|e| e.doc_id == 1).collect();
        assert!(!doc1_entries.is_empty(), "should have highlights for doc 1");
        // "3db is" in "rag3db is cool": byte 3 to byte 9
        // r(0)a(1)g(2)3(3)d(4)b(5) (6)i(7)s(8) → end offset 9
        assert!(
            doc1_entries.iter().any(|e| {
                e.offsets.iter().any(|o| o[0] == 3 && o[1] == 9)
            }),
            "doc 1 should have highlight [3,9], got {:?}", doc1_entries
        );
    }

    // ── startsWith (prefix DFA) tests ──

    #[test]
    fn test_starts_with_prefix_single_token() {
        // "rag" prefix → matches "rag3db" in docs 0 and 1
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "rag".into(),
            ContinuationMode::StartsWith,
        )
        .with_prefix();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 2, "prefix 'rag' should match 2 docs");
    }

    #[test]
    fn test_starts_with_prefix_cross_token() {
        // "import rag" prefix → matches "import rag3db from core" (doc 0)
        // The prefix DFA walks "import", gap " ", then "rag" (first 3 bytes of "rag3db") and accepts
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "import rag".into(),
            ContinuationMode::StartsWith,
        )
        .with_prefix();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "prefix 'import rag' should match doc 0");
        assert_eq!(results[0].1.doc_id, 0);
    }

    #[test]
    fn test_starts_with_prefix_fuzzy() {
        // "imporr rag" d=1 prefix → matches "import rag3db..." (doc 0)
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "imporr rag".into(),
            ContinuationMode::StartsWith,
        )
        .with_prefix()
        .with_fuzzy_distance(1);
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 1, "fuzzy prefix 'imporr rag' d=1 should match doc 0");
        assert_eq!(results[0].1.doc_id, 0);
    }

    #[test]
    fn test_starts_with_prefix_no_match() {
        // "zzz" prefix → no match
        let (index, field) = build_continuation_index();
        let query = RegexContinuationQuery::new(
            field,
            "zzz".into(),
            ContinuationMode::StartsWith,
        )
        .with_prefix();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let results = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .unwrap();

        assert_eq!(results.len(), 0);
    }
}
