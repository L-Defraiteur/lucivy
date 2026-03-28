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

use crate::index::{SegmentId, SegmentReader};
use crate::query::automaton_weight::SfxAutomatonAdapter;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::suffix_contains_query::SuffixContainsScorer;
use crate::fieldnorm::FieldNormReader;
use crate::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use crate::query::bm25::Bm25Weight;
use crate::query::posting_resolver;
use crate::schema::Field;
use crate::suffix_fst::file::{SfxDfaWrapper, SfxFileReader};
use crate::suffix_fst::gapmap::is_value_boundary;
use crate::store::StoreReader;
use crate::suffix_fst::SfxTermDictionary;
use crate::{DocId, LucivyError, Score};

/// Cached regex prescan result per segment.
#[derive(Clone, Debug)]
pub struct CachedRegexResult {
    pub doc_tf: Vec<(DocId, u32)>,
    pub highlights: Vec<(DocId, usize, usize)>,
}

/// Run regex prescan on a single segment. Compiles DFA, runs the walk,
/// returns (doc_tf, highlights). Called by the search DAG prescan node.
pub fn run_regex_prescan(
    reader: &SegmentReader,
    field: Field,
    pattern: &str,
    mode: ContinuationMode,
) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
    let sfx_data = match reader.sfx_file(field) {
        Some(d) => d,
        None => return Ok((vec![], vec![])),
    };
    let sfx_bytes = sfx_data
        .read_bytes()
        .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
    let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
        .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;

    let pr = posting_resolver::build_resolver(reader, field)?;

    let inverted_index = reader.inverted_index(field)?;
    let term_dict = inverted_index.terms();
    let sfx_dict = SfxTermDictionary::new(&sfx_reader, term_dict);

    let ord_to_term_fn = |ord: u64| -> Option<String> {
        let mut bytes = Vec::new();
        if term_dict.ord_to_term(ord, &mut bytes).ok()? {
            String::from_utf8(bytes).ok()
        } else {
            None
        }
    };

    let posmap_bytes = reader.posmap_file(field)
        .and_then(|d| d.read_bytes().ok())
        .map(|b| b.as_ref().to_vec());

    let regex = Regex::new(pattern).map_err(|e| {
        LucivyError::InvalidArgument(format!("RegexContinuation: {e}"))
    })?;
    let automaton = SfxAutomatonAdapter(&regex);

    let (_, highlights) = regex_contains_via_literal(
        &automaton, pattern, &sfx_dict, &*pr, &sfx_reader,
        mode, reader.max_doc(), &ord_to_term_fn,
        posmap_bytes.as_deref(),
    )?;

    let doc_tf = highlights_to_doc_tf(&highlights);
    Ok((doc_tf, highlights))
}

/// Run fuzzy prescan on a single segment via trigram pigeonhole.
pub fn run_fuzzy_prescan(
    reader: &SegmentReader,
    field: Field,
    query_text: &str,
    distance: u8,
    prefix: bool,
    mode: ContinuationMode,
) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
    let sfx_data = match reader.sfx_file(field) {
        Some(d) => d,
        None => return Ok((vec![], vec![])),
    };
    let sfx_bytes = sfx_data
        .read_bytes()
        .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
    let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
        .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;

    let pr = posting_resolver::build_resolver(reader, field)?;

    let inverted_index = reader.inverted_index(field)?;
    let term_dict = inverted_index.terms();

    let ord_to_term_fn = |ord: u64| -> Option<String> {
        let mut bytes = Vec::new();
        if term_dict.ord_to_term(ord, &mut bytes).ok()? {
            String::from_utf8(bytes).ok()
        } else {
            None
        }
    };

    let posmap_bytes = reader.posmap_file(field)
        .and_then(|d| d.read_bytes().ok())
        .map(|b| b.as_ref().to_vec());

    let (_, highlights) = fuzzy_contains_via_trigram(
        query_text, distance, prefix, &sfx_reader, &*pr,
        &ord_to_term_fn, mode, reader.max_doc(),
        posmap_bytes.as_deref(),
    )?;

    let doc_tf = highlights_to_doc_tf(&highlights);
    Ok((doc_tf, highlights))
}

/// Convert highlights to (doc_id, term_frequency) pairs for BM25 scoring.
pub fn highlights_to_doc_tf(highlights: &[(DocId, usize, usize)]) -> Vec<(DocId, u32)> {
    let mut counts: HashMap<DocId, u32> = HashMap::new();
    for &(doc_id, _, _) in highlights {
        *counts.entry(doc_id).or_default() += 1;
    }
    let mut doc_tf: Vec<(DocId, u32)> = counts.into_iter().collect();
    doc_tf.sort_by_key(|&(d, _)| d);
    doc_tf
}

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
    /// Prescan cache: segment_id → cached regex results (populated by DAG injection).
    regex_prescan_cache: Option<HashMap<SegmentId, CachedRegexResult>>,
    /// Global doc_freq from prescan (correct IDF across all segments/shards).
    global_regex_doc_freq: Option<u64>,
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
            regex_prescan_cache: None,
            global_regex_doc_freq: None,
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
            regex_prescan_cache: None,
            global_regex_doc_freq: None,
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
    fn prescan_segments(&mut self, segments: &[&crate::SegmentReader]) -> crate::Result<()> {
        let mut cache = HashMap::new();
        let mut doc_freq = 0u64;
        match &self.dfa_kind {
            DfaKind::Regex { pattern } => {
                let pattern = pattern.clone();
                for seg_reader in segments {
                    let (doc_tf, highlights) = run_regex_prescan(
                        seg_reader, self.field, &pattern, self.mode,
                    )?;
                    doc_freq += doc_tf.len() as u64;
                    if !doc_tf.is_empty() {
                        cache.insert(seg_reader.segment_id(), CachedRegexResult { doc_tf, highlights });
                    }
                }
            }
            DfaKind::Fuzzy { text, distance, prefix } => {
                let text = text.clone();
                let distance = *distance;
                let prefix = *prefix;
                for seg_reader in segments {
                    let (doc_tf, highlights) = run_fuzzy_prescan(
                        seg_reader, self.field, &text, distance, prefix, self.mode,
                    )?;
                    doc_freq += doc_tf.len() as u64;
                    if !doc_tf.is_empty() {
                        cache.insert(seg_reader.segment_id(), CachedRegexResult { doc_tf, highlights });
                    }
                }
            }
        }
        self.regex_prescan_cache = Some(cache);
        self.global_regex_doc_freq = Some(doc_freq);
        Ok(())
    }

    fn collect_regex_prescan_doc_freqs(&self, out: &mut std::collections::HashMap<String, u64>) {
        if let Some(freq) = self.global_regex_doc_freq {
            let key = match &self.dfa_kind {
                DfaKind::Regex { pattern } => pattern.clone(),
                DfaKind::Fuzzy { text, distance, .. } => format!("fuzzy:{}:{}", text, distance),
            };
            out.insert(key, freq);
        }
    }

    fn weight(&self, enable_scoring: EnableScoring<'_>) -> crate::Result<Box<dyn Weight>> {
        let (scoring_enabled, global_num_docs, global_num_tokens) = match &enable_scoring {
            EnableScoring::Enabled { stats, .. } => {
                let num_docs = stats.total_num_docs().unwrap_or(0);
                let num_tokens = stats.total_num_tokens(self.field).unwrap_or(0);
                (true, num_docs, num_tokens)
            }
            _ => (false, 0, 0),
        };

        Ok(Box::new(RegexContinuationWeight {
            field: self.field,
            dfa_kind: self.dfa_kind.clone(),
            mode: self.mode,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
            scoring_enabled,
            global_num_docs,
            global_num_tokens,
            regex_prescan_cache: self.regex_prescan_cache.clone().unwrap_or_default(),
            global_regex_doc_freq: self.global_regex_doc_freq.unwrap_or(0),
        }))
    }

    fn regex_prescan_params(&self) -> Vec<crate::query::RegexPrescanParam> {
        match &self.dfa_kind {
            DfaKind::Regex { pattern } => vec![crate::query::RegexPrescanParam {
                field: self.field,
                pattern: pattern.clone(),
                mode: self.mode,
            }],
            // Fuzzy uses prescan_segments() directly, not DAG regex prescan params.
            DfaKind::Fuzzy { .. } => vec![],
        }
    }

    fn inject_regex_prescan_cache(
        &mut self,
        cache: HashMap<SegmentId, CachedRegexResult>,
    ) {
        if let Some(ref mut existing) = self.regex_prescan_cache {
            existing.extend(cache);
        } else {
            self.regex_prescan_cache = Some(cache);
        }
    }

    fn set_global_regex_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        let key = match &self.dfa_kind {
            DfaKind::Regex { pattern } => pattern.clone(),
            DfaKind::Fuzzy { text, distance, .. } => format!("fuzzy:{}:{}", text, distance),
        };
        if let Some(&df) = freqs.get(&key) {
            self.global_regex_doc_freq = Some(df);
        }
    }
}

struct RegexContinuationWeight {
    field: Field,
    dfa_kind: DfaKind,
    mode: ContinuationMode,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    scoring_enabled: bool,
    global_num_docs: u64,
    global_num_tokens: u64,
    regex_prescan_cache: HashMap<SegmentId, CachedRegexResult>,
    global_regex_doc_freq: u64,
}

/// Candidate state: DFA end state + byte_from of match start for highlights.
#[derive(Clone)]
struct CandidateState<S> {
    dfa_state: S,
    byte_from: u32,
}

use crate::query::posting_resolver::PostingResolver;

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
const MIN_LITERAL_LEN: usize = 1;

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

/// Generate n-grams from a query string, adapting n-gram size to query length.
/// Short queries (< 7 chars) use bigrams, longer queries use trigrams.
/// Spaces are treated as separators (tokens never contain spaces).
/// Returns (ngram_strings, query_byte_positions, ngram_size).
fn generate_ngrams(query: &str, distance: u8) -> (Vec<String>, Vec<usize>, usize) {
    let lower = query.to_lowercase();
    // Strip spaces to measure effective length (spaces are token boundaries)
    let effective_len: usize = lower.chars().filter(|c| *c != ' ').count();

    // Choose n-gram size: bigrams for short queries, trigrams for longer ones.
    // A single edit can break at most n adjacent n-grams. For the pigeonhole
    // to guarantee at least 1 survivor: effective_len - n + 1 - n*d >= 1
    // → effective_len >= n*(d+1) + n. For trigrams d=1: need >= 7 chars.
    let n = if effective_len >= 3 * (distance as usize + 1) + 1 { 3 } else { 2 };

    let bytes = lower.as_bytes();
    let mut ngrams = Vec::new();
    let mut positions = Vec::new();

    if bytes.len() < n {
        let trimmed = lower.replace(' ', "");
        if !trimmed.is_empty() {
            ngrams.push(trimmed);
            positions.push(0);
        }
        return (ngrams, positions, n);
    }

    for i in 0..=bytes.len() - n {
        if !lower.is_char_boundary(i) || !lower.is_char_boundary(i + n) {
            continue;
        }
        let gram = &lower[i..i + n];
        if gram.contains(' ') {
            continue;
        }
        ngrams.push(gram.to_string());
        positions.push(i);
    }

    // Fallback: if no n-gram survived, use individual words
    if ngrams.is_empty() {
        for word in lower.split_whitespace() {
            if !word.is_empty() {
                let pos = word.as_ptr() as usize - lower.as_ptr() as usize;
                ngrams.push(word.to_string());
                positions.push(pos);
            }
        }
    }
    (ngrams, positions, n)
}

/// Fuzzy search via trigram pigeonhole + Levenshtein DFA validation.
/// 1. Generates trigrams from the query, searches each via find_literal (cross-token)
/// 2. Filters candidates by order + threshold + byte span (fast, eliminates 99%+)
/// 3. Validates survivors with Levenshtein DFA via PosMap walk (guaranteed correct)
pub fn fuzzy_contains_via_trigram(
    query_text: &str,
    distance: u8,
    prefix: bool,
    sfx_reader: &SfxFileReader,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    mode: ContinuationMode,
    max_doc: DocId,
    posmap_data: Option<&[u8]>,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)> {
    use super::literal_resolve::{self, LiteralMatch};
    use crate::suffix_fst::posmap::PosMapReader;

    let (ngrams, query_positions, n) = generate_ngrams(query_text, distance);

    eprintln!("[fuzzy-debug] query='{}' d={} ngrams={:?} n={} positions={:?}",
        query_text, distance, ngrams, n, query_positions);

    if ngrams.is_empty() {
        return Ok((BitSet::with_max_value(max_doc), Vec::new()));
    }

    // Threshold: each edit can break at most n adjacent n-grams.
    // Add extra slack (+1) for cross-token trigrams that may not be findable.
    let threshold = (ngrams.len() as i32 - n as i32 * distance as i32 - 1).max(1) as usize;
    eprintln!("[fuzzy-debug] threshold={} (ngrams={}, n={}, d={})", threshold, ngrams.len(), n, distance);

    // Step 1: Find each n-gram via exact contains (cross-token aware)
    let mut all_matches: Vec<Vec<LiteralMatch>> = Vec::new();
    for gram in &ngrams {
        let matches = literal_resolve::find_literal(sfx_reader, gram, resolver, ord_to_term);
        eprintln!("[fuzzy-debug] find_literal('{}') → {} matches", gram, matches.len());
        all_matches.push(matches);
    }

    // Step 2: Filter by trigram order + threshold + byte span
    let grouped: Vec<literal_resolve::MatchesByDoc> = all_matches.iter()
        .map(|matches| literal_resolve::group_by_doc(matches))
        .collect();

    let candidates = literal_resolve::intersect_trigrams_with_threshold(
        &grouped, &query_positions, threshold, distance,
    );
    eprintln!("[fuzzy-debug] candidates after intersection: {}", candidates.len());

    // Step 3: Validate each candidate with Levenshtein DFA via PosMap
    let builder = get_builder(distance);
    let dfa = if prefix {
        builder.build_prefix_dfa(&query_text.to_lowercase())
    } else {
        builder.build_dfa(&query_text.to_lowercase())
    };
    let automaton = SfxDfaWrapper(dfa);
    let start_state = automaton.start();
    let posmap = posmap_data.and_then(PosMapReader::open);

    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();

    for (doc_id, first_bf, last_bt, first_tri_idx) in &candidates {
        let doc_id = *doc_id;
        let first_tri_idx = *first_tri_idx;

        // Find the token position for first_bf from the trigram matches
        let first_pos = all_matches.iter()
            .flat_map(|m| m.iter())
            .filter(|m| m.doc_id == doc_id && m.byte_from == *first_bf)
            .map(|m| m.position)
            .min();

        let Some(fp) = first_pos else { continue; };

        if let Some(pm) = &posmap {
            // Fix cross-token DFA validation:
            // Build the text around the match region, then validate with a
            // sliding-window DFA scan. This handles both same-token and
            // cross-token matches uniformly.
            //
            // Gap handling: for single-word queries (no spaces), skip gap
            // bytes — the SFX cross-token model concatenates tokens without
            // separators. For multi-word queries (has spaces), include gaps
            // — the spaces in the query correspond to token separators.
            let back_bytes = query_positions[first_tri_idx];
            let lookback_positions = (back_bytes as u32 + 2).min(fp);
            let start_pos = fp - lookback_positions;
            // Only look forward enough to cover the query — NOT 64 tokens,
            // which would create a huge concat text and cause false DFA matches.
            let forward_bytes = query_text.len() + distance as usize;
            let forward_positions = forward_bytes as u32 / 2 + 3;
            let max_pos = pm.num_tokens(doc_id);
            let end_pos = (fp + forward_positions).min(max_pos);

            let include_gaps = query_text.contains(' ');
            let mut concat_bytes: Vec<u8> = Vec::new();
            let mut fp_concat_offset: usize = 0;
            for pos in start_pos..end_pos {
                if pos == fp { fp_concat_offset = concat_bytes.len(); }
                if include_gaps && pos > start_pos {
                    let gap = sfx_reader.gapmap().read_separator(doc_id, pos - 1, pos);
                    if let Some(gap_bytes) = gap {
                        if is_value_boundary(gap_bytes) { break; }
                        concat_bytes.extend_from_slice(gap_bytes);
                    }
                }
                if let Some(tok_ord) = pm.ordinal_at(doc_id, pos) {
                    if let Some(text) = ord_to_term(tok_ord as u64) {
                        concat_bytes.extend_from_slice(text.as_bytes());
                    }
                }
            }

            // Find the first matched trigram near fp in concat to anchor the DFA.
            // Search from fp_concat_offset to avoid matching a wrong occurrence
            // of the trigram in an earlier token.
            let trigram_bytes = ngrams[first_tri_idx].as_bytes();
            let trigram_near_fp = concat_bytes[fp_concat_offset..].windows(trigram_bytes.len())
                .position(|w| w == trigram_bytes)
                .map(|p| p + fp_concat_offset);

            let expected_start = trigram_near_fp
                .map(|tp| tp.saturating_sub(query_positions[first_tri_idx]))
                .unwrap_or(fp_concat_offset);

            // Validate from the anchored position. Try expected_start first,
            // then ±1 for edge cases (deletions/insertions at the start).
            // Feed up to query_len + distance bytes and take the LONGEST match
            // (the DFA can accept early via trailing deletions).
            let mut matched = false;
            let mut match_len: usize = 0;
            let max_feed = query_text.len() + distance as usize + 1;
            let try_offsets = [0i32, -1, 1];
            for &offset in &try_offsets {
                let start_byte = if offset < 0 {
                    expected_start.checked_sub((-offset) as usize)
                } else {
                    Some(expected_start + offset as usize)
                };
                let Some(sb) = start_byte else { continue };
                if sb >= concat_bytes.len() { continue; }
                let mut s = start_state.clone();
                let mut fed: usize = 0;
                let mut local_matched = false;
                let mut best_len: usize = 0;
                let mut best_diff: usize = usize::MAX;
                let qlen = query_text.len();
                for &byte in &concat_bytes[sb..] {
                    if fed >= max_feed { break; }
                    s = automaton.accept(&s, byte);
                    fed += 1;
                    if !automaton.can_match(&s) { break; }
                    if automaton.is_match(&s) {
                        local_matched = true;
                        let diff = (fed as isize - qlen as isize).unsigned_abs();
                        if diff < best_diff {
                            best_diff = diff;
                            best_len = fed;
                        }
                    }
                }
                if local_matched {
                    matched = true;
                    match_len = best_len;
                    break;
                }
            }

            if matched {
                doc_bitset.insert(doc_id);
                let adj_bf = first_bf.saturating_sub(query_positions[first_tri_idx] as u32);
                let adj_bt = adj_bf + match_len as u32;
                highlights.push((doc_id, adj_bf as usize, adj_bt as usize));
            }
        } else {
            // No PosMap — accept trigram-filtered candidates conservatively
            doc_bitset.insert(doc_id);
            highlights.push((doc_id, *first_bf as usize, *last_bt as usize));
        }
    }

    Ok((doc_bitset, highlights))
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
/// Falls back to empty results when no usable literal found (never scans full FST).
pub fn regex_contains_via_literal<A: Automaton>(
    automaton: &A,
    pattern: &str,
    sfx_dict: &SfxTermDictionary,
    resolver: &dyn PostingResolver,
    sfx_reader: &SfxFileReader,
    mode: ContinuationMode,
    max_doc: DocId,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
    posmap_data: Option<&[u8]>,
) -> crate::Result<(BitSet, Vec<(DocId, usize, usize)>)>
where
    A::State: Clone + Eq + std::hash::Hash,
{
    use super::literal_resolve::{self, LiteralMatch};
    use std::time::Instant;
    use crate::suffix_fst::posmap::PosMapReader;

    let t_total = Instant::now();
    let all_literals = extract_all_literals(pattern);

    let viable: Vec<&String> = all_literals.iter()
        .filter(|l| l.len() >= MIN_LITERAL_LEN)
        .collect();

    if viable.is_empty() {
        eprintln!("[regex-timer] no viable literal for '{}' → 0 results", pattern);
        return Ok((BitSet::with_max_value(max_doc), Vec::new()));
    }

    // ═══════════════════════════════════════════════════════════════════
    // Step 1: Resolve each literal using exact contains logic (cross-token aware).
    // ═══════════════════════════════════════════════════════════════════
    let t0 = Instant::now();

    let mut all_matches: Vec<Vec<LiteralMatch>> = Vec::new();
    for lit in &viable {
        let matches = literal_resolve::find_literal(sfx_reader, lit, resolver, ord_to_term);
        eprintln!("[regex-timer] find_literal('{}') → {} matches", lit, matches.len());
        all_matches.push(matches);
    }

    let find_us = t0.elapsed().as_micros();

    // ═══════════════════════════════════════════════════════════════════
    // Step 2: Single-literal → DFA validate each match. Multi-literal → intersect + PosMap.
    // ═══════════════════════════════════════════════════════════════════
    let t1 = Instant::now();

    let mut doc_bitset = BitSet::with_max_value(max_doc);
    let mut highlights: Vec<(DocId, usize, usize)> = Vec::new();
    let posmap = posmap_data.and_then(PosMapReader::open);
    let start_state = automaton.start();

    let first_literal = &viable[0];

    if all_matches.len() == 1 {
        // ── Single-literal: DFA validate ──
        // The find_literal matched the literal as a substring (may be mid-token).
        // Feed the literal text itself to the DFA (we know it's in the text).
        // Then walk forward via PosMap for the rest of the regex.
        let literal_bytes = first_literal.as_bytes();

        for m in &all_matches[0] {
            // Feed the literal to get DFA state after the matched substring.
            let mut state = start_state.clone();
            let mut alive = true;
            for &byte in literal_bytes {
                state = automaton.accept(&state, byte);
                if !automaton.can_match(&state) { alive = false; break; }
            }
            if !alive { continue; }

            if automaton.is_match(&state) {
                doc_bitset.insert(m.doc_id);
                highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                continue;
            }

            // DFA alive but not accepting → cross-token via PosMap.
            if automaton.can_match(&state) {
                if let Some(pm) = &posmap {
                    let max_pos = pm.num_tokens(m.doc_id);
                    let end_pos = (m.position + MAX_CONTINUATION_DEPTH as u32).min(max_pos);
                    if end_pos > m.position {
                        if let Some(final_state) = literal_resolve::validate_path(
                            automaton, &state, pm, sfx_reader, ord_to_term,
                            m.doc_id, m.position, end_pos - 1,
                        ) {
                            if automaton.is_match(&final_state) {
                                doc_bitset.insert(m.doc_id);
                                highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                            }
                        }
                    }
                } else {
                    // No PosMap — accept conservatively.
                    doc_bitset.insert(m.doc_id);
                    highlights.push((m.doc_id, m.byte_from as usize, m.byte_to as usize));
                }
            }
        }
    } else {
        // ── Multi-literal: intersect + position ordering + PosMap DFA validate ──
        let grouped: Vec<literal_resolve::MatchesByDoc> = all_matches.iter()
            .map(|matches| literal_resolve::group_by_doc(matches))
            .collect();

        let ordered = literal_resolve::intersect_literals_ordered(&grouped);

        eprintln!("[regex-timer] multi-literal intersect: {} docs survive ordering", ordered.len());

        if let Some(pm) = &posmap {
            // PosMap walk: validate DFA between the literal positions.
            for &(doc_id, first_bf, last_bt) in &ordered {
                // Find actual positions of first and last literals in this doc.
                let first_pos = grouped[0].get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, bf, _)| bf == first_bf))
                    .map(|&(pos, _, _)| pos);
                let last_pos = grouped.last().unwrap().get(&doc_id)
                    .and_then(|v| v.iter().find(|&&(_, _, bt)| bt == last_bt))
                    .map(|&(pos, _, _)| pos);

                let (Some(fp), Some(lp)) = (first_pos, last_pos) else { continue; };

                // Feed first token to DFA, starting from the literal's offset
                // within the token. For `ag3.*ver` with token "rag3db", feed
                // from byte 1 ("ag3db") not byte 0 ("rag3db"), because the DFA
                // expects the first literal at its start state.
                let first_state = if let Some(tok_ord) = pm.ordinal_at(doc_id, fp) {
                    if let Some(text) = ord_to_term(tok_ord as u64) {
                        let offset = text.find(first_literal.as_str()).unwrap_or(0);
                        let mut s = start_state.clone();
                        let mut alive = true;
                        for &byte in &text.as_bytes()[offset..] {
                            s = automaton.accept(&s, byte);
                            if !automaton.can_match(&s) { alive = false; break; }
                        }
                        if !alive { continue; }
                        s
                    } else { continue; }
                } else { continue; };

                // Validate path from first token to last token.
                if fp == lp {
                    // Same token — already validated above.
                    if automaton.is_match(&first_state) {
                        doc_bitset.insert(doc_id);
                        highlights.push((doc_id, first_bf as usize, last_bt as usize));
                    }
                } else {
                    // Walk intermediate tokens via PosMap.
                    let result = literal_resolve::validate_path(
                        automaton, &first_state, pm, sfx_reader, ord_to_term,
                        doc_id, fp, lp,
                    );
                    if let Some(final_state) = result {
                        if automaton.is_match(&final_state) {
                            doc_bitset.insert(doc_id);
                            highlights.push((doc_id, first_bf as usize, last_bt as usize));
                        }
                    }
                }
            }
        } else {
            // No PosMap — accept all ordered matches (conservative).
            for &(doc_id, first_bf, last_bt) in &ordered {
                doc_bitset.insert(doc_id);
                highlights.push((doc_id, first_bf as usize, last_bt as usize));
            }
        }
    }

    let total_us = t_total.elapsed().as_micros();
    eprintln!(
        "[regex-timer] '{}' | find={}us | validate={}us | total={}us | {}docs,{}hl",
        pattern, find_us, t1.elapsed().as_micros(), total_us,
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

impl RegexContinuationWeight {
    /// Emit highlights to the sink (shared by cached and fallback paths).
    fn emit_highlights(&self, segment_id: SegmentId, highlights: &[(DocId, usize, usize)]) {
        if let Some(ref sink) = self.highlight_sink {
            for &(doc_id, byte_from, byte_to) in highlights {
                sink.insert(
                    segment_id,
                    doc_id,
                    &self.highlight_field_name,
                    vec![[byte_from, byte_to]],
                );
            }
        }
    }

    /// Build BM25 scorer from doc_tf (shared by cached and fallback paths).
    fn build_scorer(
        &self, reader: &SegmentReader, boost: Score,
        doc_tf: Vec<(DocId, u32)>,
    ) -> crate::Result<Box<dyn Scorer>> {
        let fieldnorm_reader = reader.fieldnorms_readers()
            .get_field(self.field)?
            .unwrap_or_else(|| FieldNormReader::constant(reader.max_doc(), 1));

        let bm25_weight = if self.scoring_enabled {
            let (total_num_docs, total_num_tokens) = if self.global_num_docs > 0 {
                (self.global_num_docs, self.global_num_tokens)
            } else {
                let inv_idx = reader.inverted_index(self.field)?;
                ((reader.max_doc() as u64).max(1), inv_idx.total_num_tokens())
            };
            let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
            let doc_freq = if self.global_regex_doc_freq > 0 {
                self.global_regex_doc_freq
            } else {
                doc_tf.len() as u64
            };
            Bm25Weight::for_one_term(doc_freq, total_num_docs, average_fieldnorm)
        } else {
            Bm25Weight::for_one_term(0, 1, 1.0)
        };

        Ok(Box::new(SuffixContainsScorer::new(
            doc_tf,
            bm25_weight.boost_by(boost),
            fieldnorm_reader,
        )))
    }

    /// Fallback: run regex walk without prescan cache.
    /// Compiles DFA, opens .sfx/.posmap, runs the walk, converts to doc_tf.
    fn run_regex_fallback(
        &self, reader: &SegmentReader,
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        let max_doc = reader.max_doc();

        let sfx_data = match reader.sfx_file(self.field) {
            Some(data) => data,
            None => return Ok((vec![], vec![])),
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
            DfaKind::Fuzzy { text, distance, prefix } if *distance > 0 => {
                // Fuzzy d>=1 via trigram pigeonhole + DFA validation.
                let posmap_bytes = reader.posmap_file(self.field)
                    .and_then(|data| data.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());
                fuzzy_contains_via_trigram(
                    text, *distance, *prefix, &sfx_reader, &*resolver,
                    &ord_to_term_fn, self.mode, max_doc,
                    posmap_bytes.as_deref(),
                )?
            }
            DfaKind::Fuzzy { text, distance, prefix } => {
                // Exact (d=0) — use existing DFA continuation path.
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
                let posmap_bytes = reader.posmap_file(self.field)
                    .and_then(|data| data.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());

                let regex = Regex::new(pattern).map_err(|e| {
                    LucivyError::InvalidArgument(format!("RegexContinuation: {e}"))
                })?;
                let automaton = SfxAutomatonAdapter(&regex);

                if use_sibling {
                    regex_contains_via_literal(
                        &automaton, pattern, &sfx_dict, &*resolver, &sfx_reader,
                        self.mode, max_doc, &ord_to_term_fn,
                        posmap_bytes.as_deref(),
                    )?
                } else {
                    continuation_score(
                        &automaton, &sfx_dict, &*resolver, &sfx_reader, self.mode, max_doc,
                        None,
                    )?
                }
            }
        };

        // Convert (BitSet, highlights) → (doc_tf, highlights)
        let _ = doc_bitset; // BitSet no longer needed — doc_tf derived from highlights
        let doc_tf = highlights_to_doc_tf(&highlights);
        Ok((doc_tf, highlights))
    }
}

impl Weight for RegexContinuationWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();

        // === FAST PATH: prescan cache available (from DAG) ===
        if let Some(cached) = self.regex_prescan_cache.get(&segment_id) {
            if cached.doc_tf.is_empty() {
                return Ok(Box::new(crate::query::EmptyScorer));
            }
            self.emit_highlights(segment_id, &cached.highlights);
            return self.build_scorer(reader, boost, cached.doc_tf.clone());
        }

        // === SLOW PATH: fallback (non-DAG or prescan skipped) ===
        let (doc_tf, highlights) = self.run_regex_fallback(reader)?;
        if doc_tf.is_empty() {
            return Ok(Box::new(crate::query::EmptyScorer));
        }
        self.emit_highlights(segment_id, &highlights);
        self.build_scorer(reader, boost, doc_tf)
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("RegexContinuationQuery", scorer.score()))
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

    #[test]
    fn test_fuzzy_ngram_variants() {
        let (index, field) = build_continuation_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Target text in index: "rag3db" (in docs 0 and 1)
        let cases: Vec<(&str, u8, bool)> = vec![
            ("rag3db", 0, true),       // exact
            ("rak3db", 1, true),       // sub pos 2 (g→k), 4 chars prefix
            ("rag3xb", 1, true),       // sub pos 4 (d→x)
            ("rag3d", 1, true),        // deletion at end
            ("ag3db", 1, true),        // deletion at start
            ("rag3dba", 1, true),      // insertion at end
            ("rXg3db", 1, true),       // sub pos 1 (a→X)
            ("rak3db", 0, false),      // not exact
            ("rak3xb", 1, false),      // 2 edits
        ];

        for (query, dist, should_match) in &cases {
            let q = RegexContinuationQuery::new(
                field, query.to_string(), ContinuationMode::Contains,
            ).with_fuzzy_distance(*dist);
            let results = searcher.search(&q, &TopDocs::with_limit(10).order_by_score()).unwrap();
            let matched = !results.is_empty();
            eprintln!("  '{}' d={} → {} results {}", query, dist, results.len(),
                if matched == *should_match { "✓" } else { "FAIL" });
            assert_eq!(matched, *should_match,
                "'{}' d={}: got {} results", query, dist, results.len());
        }
    }
}
