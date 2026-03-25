//! SfxTermDictionary — term dictionary backed by the suffix FST (.sfx file).
//!
//! Drop-in replacement for TermDictionary when a .sfx file exists.
//! Filters SI=0 entries for exact/prefix/fuzzy/regex lookups (same results
//! as the standard ._raw FST). Contains lookups use any SI.
//!
//! Uses the same TermInfoStore as the underlying TermDictionary for
//! posting list resolution — ordinals are identical (both BTreeSet-sorted).

use std::io;

use lucivy_fst::{Automaton, IntoStreamer, Streamer};

use super::builder::{decode_output, decode_parent_entries, ParentEntry, ParentRef};
use super::file::SfxFileReader;
use crate::postings::TermInfo;
use crate::termdict::{TermDictionary, TermOrdinal};

/// Result of a continuation walk on the suffix FST.
/// Contains the DFA end state so the caller can feed gap bytes and continue.
#[derive(Debug, Clone)]
pub struct ContinuationMatch<S> {
    /// The raw ordinal — used to resolve postings via .sfxpost or TermDictionary.
    pub raw_ordinal: u64,
    /// Suffix index (0 = full token, >0 = starts mid-token).
    pub si: u16,
    /// DFA state at the end of this token/suffix.
    pub end_state: S,
    /// Whether the DFA accepts at this point (direct match).
    pub is_accepting: bool,
}

/// Wrapper automaton that prepends a required prefix byte before delegating
/// to the inner automaton. Used to restrict FST searches to SI=0 or SI>0 partitions.
struct PrefixByteAutomaton<'a, A: Automaton> {
    inner: &'a A,
    prefix_byte: u8,
}

/// State: None = expecting prefix byte, Some(inner_state) = delegating
#[derive(Clone)]
enum PrefixByteState<S: Clone> {
    ExpectingPrefix,
    Inner(S),
    Dead,
}

impl<'a, A: Automaton> Automaton for PrefixByteAutomaton<'a, A>
where A::State: Clone {
    type State = PrefixByteState<A::State>;

    fn start(&self) -> Self::State {
        PrefixByteState::ExpectingPrefix
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match state {
            PrefixByteState::Inner(s) => self.inner.is_match(s),
            _ => false,
        }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        match state {
            PrefixByteState::ExpectingPrefix => true,
            PrefixByteState::Inner(s) => self.inner.can_match(s),
            PrefixByteState::Dead => false,
        }
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        match state {
            PrefixByteState::ExpectingPrefix => {
                if byte == self.prefix_byte {
                    PrefixByteState::Inner(self.inner.start())
                } else {
                    PrefixByteState::Dead
                }
            }
            PrefixByteState::Inner(s) => PrefixByteState::Inner(self.inner.accept(s, byte)),
            PrefixByteState::Dead => PrefixByteState::Dead,
        }
    }
}

/// Wrapper automaton that combines prefix byte filtering with continuation
/// semantics (can_match as match). Used by search_continuation.
struct PrefixByteContinuationAutomaton<'a, A: Automaton> {
    inner: &'a A,
    start_state: A::State,
    prefix_byte: u8,
}

#[derive(Clone)]
enum PrefixContinuationState<S: Clone> {
    ExpectingPrefix,
    Inner(S),
    Dead,
}

impl<'a, A: Automaton> Automaton for PrefixByteContinuationAutomaton<'a, A>
where A::State: Clone {
    type State = PrefixContinuationState<A::State>;

    fn start(&self) -> Self::State {
        PrefixContinuationState::ExpectingPrefix
    }

    fn is_match(&self, state: &Self::State) -> bool {
        // Continuation semantics: "alive" = "matching"
        self.can_match(state)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        match state {
            PrefixContinuationState::ExpectingPrefix => true,
            PrefixContinuationState::Inner(s) => self.inner.can_match(s),
            PrefixContinuationState::Dead => false,
        }
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        match state {
            PrefixContinuationState::ExpectingPrefix => {
                if byte == self.prefix_byte {
                    PrefixContinuationState::Inner(self.start_state.clone())
                } else {
                    PrefixContinuationState::Dead
                }
            }
            PrefixContinuationState::Inner(s) => {
                PrefixContinuationState::Inner(self.inner.accept(s, byte))
            }
            PrefixContinuationState::Dead => PrefixContinuationState::Dead,
        }
    }
}

/// A term dictionary backed by the .sfx suffix FST.
///
/// For exact/prefix/fuzzy/regex (standard lookups): filters SI=0 entries,
/// returning the same results as the standard TermDictionary.
///
/// For contains lookups: uses any SI via the SfxFileReader directly.
pub struct SfxTermDictionary<'a> {
    sfx_reader: &'a SfxFileReader<'a>,
    /// The underlying TermDictionary for TermInfo resolution via ordinal.
    termdict: &'a TermDictionary,
}

impl<'a> SfxTermDictionary<'a> {
    /// Create from a parsed SfxFileReader and the existing TermDictionary.
    pub fn new(sfx_reader: &'a SfxFileReader<'a>, termdict: &'a TermDictionary) -> Self {
        Self { sfx_reader, termdict }
    }

    /// Number of unique terms (SI=0 entries = same as TermDictionary).
    pub fn num_terms(&self) -> u32 {
        self.sfx_reader.num_suffix_terms()
    }

    /// Lookup a term → TermInfo (SI=0 only, same as TermDictionary::get).
    ///
    /// The .sfx stores lowercased keys, so the lookup is done on the lowercased
    /// version of `key`. When multiple SI=0 parents exist (case variants like
    /// "HELLO" and "hello"), we verify against the TermDictionary to return the
    /// TermInfo for the exact original term.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<TermInfo>> {
        let key_str = std::str::from_utf8(key).map_err(|e| io::Error::other(e.to_string()))?;
        let lower = key_str.to_lowercase();
        let parents = self.sfx_reader.resolve_suffix(&lower);
        let mut term_buf = Vec::new();
        for p in &parents {
            if p.si == 0 {
                // Verify case-sensitive match: resolve ordinal → original term
                if self.termdict.ord_to_term(p.raw_ordinal, &mut term_buf)? && term_buf == key {
                    return Ok(Some(self.termdict.term_info_from_ord(p.raw_ordinal)));
                }
            }
        }
        Ok(None)
    }

    /// Lookup a term → ordinal (SI=0 only, same as TermDictionary::term_ord).
    ///
    /// Same lowercase + case-sensitive verification as `get()`.
    pub fn term_ord(&self, key: &[u8]) -> io::Result<Option<TermOrdinal>> {
        let key_str = std::str::from_utf8(key).map_err(|e| io::Error::other(e.to_string()))?;
        let lower = key_str.to_lowercase();
        let parents = self.sfx_reader.resolve_suffix(&lower);
        let mut term_buf = Vec::new();
        for p in &parents {
            if p.si == 0 {
                if self.termdict.ord_to_term(p.raw_ordinal, &mut term_buf)? && term_buf == key {
                    return Ok(Some(p.raw_ordinal));
                }
            }
        }
        Ok(None)
    }

    /// Resolve ordinal → TermInfo (delegates to underlying TermDictionary).
    pub fn term_info_from_ord(&self, term_ord: TermOrdinal) -> TermInfo {
        self.termdict.term_info_from_ord(term_ord)
    }

    /// Search with an automaton (SI=0 only). Returns all matching (key, TermInfo) pairs.
    /// Equivalent to TermDictionary::search(automaton).into_stream() iterated to completion.
    pub fn search_automaton<A: Automaton>(&self, automaton: &A) -> Vec<(String, TermInfo)>
    where A::State: Clone {
        // Walk only SI=0 partition (\x00 prefix) — wrap automaton to skip prefix byte
        let wrapper = PrefixByteAutomaton { inner: automaton, prefix_byte: super::builder::SI0_PREFIX };
        let mut stream = self.sfx_reader.fst().search(&wrapper).into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                let term_info = self.termdict.term_info_from_ord(p.raw_ordinal);
                // Strip prefix byte from key
                if key.len() > 1 {
                    if let Ok(s) = std::str::from_utf8(&key[1..]) {
                        results.push((s.to_string(), term_info));
                    }
                }
                break; // One parent per SI=0 entry
            }
        }
        results
    }

    /// Range scan (SI=0 only). Returns all terms in [ge, lt) with their TermInfo.
    pub fn range_scan(&self, ge: &[u8], lt: Option<&[u8]>) -> Vec<(String, TermInfo)> {
        // Prefix with \x00 for SI=0 partition
        let mut ge_key = vec![super::builder::SI0_PREFIX];
        ge_key.extend_from_slice(ge);
        let lt_key = lt.map(|l| {
            let mut k = vec![super::builder::SI0_PREFIX];
            k.extend_from_slice(l);
            k
        });

        let mut builder = self.sfx_reader.fst().range().ge(&ge_key);
        if let Some(ref lt_bound) = lt_key {
            builder = builder.lt(lt_bound);
        }
        let mut stream = builder.into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                let term_info = self.termdict.term_info_from_ord(p.raw_ordinal);
                if key.len() > 1 {
                    if let Ok(s) = std::str::from_utf8(&key[1..]) {
                        results.push((s.to_string(), term_info));
                    }
                }
                break;
            }
        }
        results
    }

    /// Search with automaton (SI=0 only). Returns (key, raw_ordinal) pairs.
    pub fn search_automaton_ordinals<A: Automaton>(&self, automaton: &A) -> Vec<(String, u64)>
    where A::State: Clone {
        let wrapper = PrefixByteAutomaton { inner: automaton, prefix_byte: super::builder::SI0_PREFIX };
        let mut stream = self.sfx_reader.fst().search(&wrapper).into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                if key.len() > 1 {
                    if let Ok(s) = std::str::from_utf8(&key[1..]) {
                        results.push((s.to_string(), p.raw_ordinal));
                    }
                }
                break;
            }
        }
        results
    }

    /// Lookup a term → raw_ordinal (SI=0 only).
    pub fn get_ordinal(&self, key: &[u8]) -> io::Result<Option<u64>> {
        let key_str = std::str::from_utf8(key).map_err(|e| io::Error::other(e.to_string()))?;
        let lower = key_str.to_lowercase();
        let parents = self.sfx_reader.resolve_suffix(&lower);
        for p in &parents {
            if p.si == 0 {
                return Ok(Some(p.raw_ordinal));
            }
        }
        Ok(None)
    }

    /// Range scan (SI=0 only). Returns (key, raw_ordinal) pairs.
    pub fn range_scan_ordinals(&self, ge: &[u8], lt: Option<&[u8]>) -> Vec<(String, u64)> {
        let mut ge_key = vec![super::builder::SI0_PREFIX];
        ge_key.extend_from_slice(ge);
        let lt_key = lt.map(|l| {
            let mut k = vec![super::builder::SI0_PREFIX];
            k.extend_from_slice(l);
            k
        });
        let mut builder = self.sfx_reader.fst().range().ge(&ge_key);
        if let Some(ref lt_bound) = lt_key {
            builder = builder.lt(lt_bound);
        }
        let mut stream = builder.into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                if key.len() > 1 {
                    if let Ok(s) = std::str::from_utf8(&key[1..]) {
                        results.push((s.to_string(), p.raw_ordinal));
                    }
                }
                break;
            }
        }
        results
    }

    /// Stream all terms (SI=0 only). Returns all terms with their TermInfo.
    pub fn stream_all(&self) -> Vec<(String, TermInfo)> {
        self.range_scan(&[], None)
    }

    /// Walk suffix FST with an automaton starting from an arbitrary state.
    ///
    /// Unlike `search_automaton` which only returns entries where the DFA
    /// accepts, this returns ALL entries where the DFA is still alive
    /// (`can_match`) at the end of the key. This is needed for regex
    /// continuation across token boundaries.
    ///
    /// Returns `ContinuationMatch` entries with the DFA end state, so the
    /// caller can feed gap bytes and continue to the next token.
    ///
    /// `si_zero_only`: if true, only SI=0 entries (full tokens). Use true
    /// for continuation walks (next token starts from beginning). Use false
    /// for the initial walk in contains mode (regex can start mid-token).
    pub fn search_continuation<A: Automaton>(
        &self,
        automaton: &A,
        start_state: A::State,
        si_zero_only: bool,
    ) -> Vec<ContinuationMatch<A::State>>
    where
        A::State: Clone,
    {
        let mut results = Vec::new();

        // Determine which partitions to search
        let prefixes: &[u8] = if si_zero_only {
            &[super::builder::SI0_PREFIX]
        } else {
            &[super::builder::SI0_PREFIX, super::builder::SI_REST_PREFIX]
        };

        for &prefix_byte in prefixes {
            let wrapper = PrefixByteContinuationAutomaton {
                inner: automaton,
                start_state: start_state.clone(),
                prefix_byte,
            };
            let mut stream = self.sfx_reader.fst().search(&wrapper).into_stream();

            while let Some((key, val)) = stream.next() {
                // Re-walk the DFA through the key bytes (skip prefix byte)
                let mut state = start_state.clone();
                for &byte in &key[1..] {
                    state = automaton.accept(&state, byte);
                }
                let is_accepting = automaton.is_match(&state);
                let is_alive = automaton.can_match(&state);

                if !is_alive {
                    continue;
                }

                let parents = self.decode_parents(val);
                for p in &parents {
                    results.push(ContinuationMatch {
                        raw_ordinal: p.raw_ordinal,
                        si: p.si,
                        end_state: state.clone(),
                        is_accepting,
                    });
                }
            }
        }
        results
    }

    /// Access the underlying SfxFileReader for contains queries (any SI).
    pub fn sfx_reader(&self) -> &SfxFileReader<'a> {
        &self.sfx_reader
    }

    /// Access the underlying TermDictionary.
    pub fn termdict(&self) -> &'a TermDictionary {
        self.termdict
    }

    fn decode_parents(&self, val: u64) -> Vec<ParentEntry> {
        match decode_output(val) {
            ParentRef::Single { raw_ordinal, si, token_len } => {
                vec![ParentEntry { raw_ordinal, si, token_len }]
            }
            ParentRef::Multi { offset } => {
                let table = lucivy_fst::OutputTable::new(self.sfx_reader.parent_list_data());
                let record = table.get(offset);
                decode_parent_entries(record)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaBuilder, TextFieldIndexing, TextOptions, IndexRecordOption};
    use crate::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer};
    use crate::{Index, LucivyDocument};

    #[test]
    fn test_sfx_term_dict_get_exact() {
        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();

        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();

        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // Exact lookup: "import" should return same TermInfo as standard TermDictionary
        let standard_ti = termdict.get(b"import").unwrap();
        let sfx_ti = sfx_dict.get(b"import").unwrap();
        assert_eq!(standard_ti, sfx_ti);

        // "rag3db" should also match
        let standard_ti = termdict.get(b"rag3db").unwrap();
        let sfx_ti = sfx_dict.get(b"rag3db").unwrap();
        assert_eq!(standard_ti, sfx_ti);

        // Suffix "g3db" should NOT match (SI>0)
        let sfx_ti = sfx_dict.get(b"g3db").unwrap();
        assert!(sfx_ti.is_none());

        // Nonexistent term
        let sfx_ti = sfx_dict.get(b"nonexistent").unwrap();
        assert!(sfx_ti.is_none());
    }

    #[test]
    fn test_sfx_term_dict_range_scan() {
        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();

        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();

        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // Range "im" to "in" should find "import" (SI=0) but not "mport" (SI>0)
        let results = sfx_dict.range_scan(b"im", Some(b"in"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "import");
    }

    #[test]
    fn test_sfx_term_dict_search_automaton() {
        use levenshtein_automata::LevenshteinAutomatonBuilder;
        use crate::suffix_fst::file::SfxDfaWrapper;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();

        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();

        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // Fuzzy search "importt" d=1 → should find "import"
        let builder = LevenshteinAutomatonBuilder::new(1, true);
        let dfa = builder.build_dfa("importt");
        let automaton = SfxDfaWrapper(dfa);
        let results = sfx_dict.search_automaton(&automaton);

        assert!(results.iter().any(|(k, _)| k == "import"), "fuzzy d=1 should find 'import'");
        // Should NOT contain suffixes like "mport"
        assert!(!results.iter().any(|(k, _)| k == "mport"), "should not contain suffixes");
    }

    /// Full parity: every term in the standard TermDictionary must return the
    /// same TermInfo via SfxTermDictionary::get().
    #[test]
    fn test_parity_all_terms_get() {
        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();

        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // Iterate ALL terms in standard TermDictionary and compare
        let mut stream = termdict.stream().unwrap();
        let mut checked = 0;
        while stream.advance() {
            let key = stream.key().to_vec();
            let standard_ti = stream.value().clone();
            let sfx_ti = sfx_dict.get(&key).unwrap()
                .unwrap_or_else(|| panic!("sfx_dict.get({:?}) returned None", String::from_utf8_lossy(&key)));
            assert_eq!(
                standard_ti, sfx_ti,
                "TermInfo mismatch for {:?}", String::from_utf8_lossy(&key)
            );
            checked += 1;
        }
        assert!(checked >= 6, "expected at least 6 terms, got {checked}");
    }

    /// Parity: FuzzyTermQuery returns same doc_ids via sfx as via standard.
    #[test]
    fn test_parity_fuzzy_term_query() {
        use crate::collector::TopDocs;
        use crate::query::FuzzyTermQuery;
        use crate::schema::Term;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Fuzzy "importt" d=1 → should find docs with "import"
        let term = Term::from_field_text(body_raw, "importt");
        let query = FuzzyTermQuery::new(term, 1, true);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 1, "fuzzy 'importt' d=1 should match 1 doc");
        assert_eq!(results[0].1.doc_id, 0);

        // Fuzzy "rag3dc" d=1 → should find docs with "rag3db"
        let term = Term::from_field_text(body_raw, "rag3dc");
        let query = FuzzyTermQuery::new(term, 1, true);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 2, "fuzzy 'rag3dc' d=1 should match 2 docs");

        // Fuzzy "zzzzz" d=1 → no match
        let term = Term::from_field_text(body_raw, "zzzzz");
        let query = FuzzyTermQuery::new(term, 1, true);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 0);
    }

    /// Parity: RegexQuery returns same doc_ids via sfx as via standard.
    #[test]
    fn test_parity_regex_query() {
        use crate::collector::TopDocs;
        use crate::query::RegexQuery;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Regex "rag.db" → matches "rag3db"
        let query = RegexQuery::from_pattern("rag.db", body_raw).unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 2, "regex 'rag.db' should match 2 docs");

        // Regex "imp.+" → matches "import"
        let query = RegexQuery::from_pattern("imp.+", body_raw).unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 1, "regex 'imp.+' should match 1 doc");
        assert_eq!(results[0].1.doc_id, 0);

        // Regex "zzz" → no match
        let query = RegexQuery::from_pattern("zzz", body_raw).unwrap();
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 0);
    }

    /// Parity: TermQuery returns same doc_ids via sfx as via standard.
    #[test]
    fn test_parity_term_query() {
        use crate::collector::TopDocs;
        use crate::query::TermQuery;
        use crate::schema::Term;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Exact "rag3db" → 2 docs
        let term = Term::from_field_text(body_raw, "rag3db");
        let query = TermQuery::new(term, IndexRecordOption::WithFreqs);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 2, "term 'rag3db' should match 2 docs");

        // Exact "import" → 1 doc
        let term = Term::from_field_text(body_raw, "import");
        let query = TermQuery::new(term, IndexRecordOption::WithFreqs);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.doc_id, 0);

        // Exact "nonexistent" → 0 docs
        let term = Term::from_field_text(body_raw, "nonexistent");
        let query = TermQuery::new(term, IndexRecordOption::WithFreqs);
        let results = searcher.search(&query, &TopDocs::with_limit(10).order_by_score()).unwrap();
        assert_eq!(results.len(), 0);
    }

    /// Parity: case-sensitive tokenizer — sfx must return correct TermInfo
    /// even when multiple case variants of the same token exist.
    #[test]
    fn test_parity_case_sensitive() {
        let mut schema_builder = SchemaBuilder::new();
        let opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("no_lower")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let field = schema_builder.add_text_field("text", opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        // Case-sensitive tokenizer: SimpleTokenizer without LowerCaser
        let tokenizer = TextAnalyzer::builder(SimpleTokenizer::default()).build();
        index.tokenizers().register("no_lower", tokenizer);

        let mut writer = index.writer_for_tests().unwrap();
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "HELLO world");
        writer.add_document(doc).unwrap();
        let mut doc = LucivyDocument::new();
        doc.add_text(field, "hello WORLD");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(field).unwrap();
        let termdict = inv_idx.terms();
        let sfx_data = seg_reader.sfx_file(field).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // "HELLO" and "hello" are different terms — sfx must distinguish them
        let ti_upper = sfx_dict.get(b"HELLO").unwrap().expect("HELLO should exist");
        let ti_lower = sfx_dict.get(b"hello").unwrap().expect("hello should exist");
        assert_ne!(ti_upper, ti_lower, "HELLO and hello should have different TermInfos");

        // Each should match via standard TermDictionary too
        let std_upper = termdict.get(b"HELLO").unwrap().expect("std HELLO");
        let std_lower = termdict.get(b"hello").unwrap().expect("std hello");
        assert_eq!(ti_upper, std_upper, "HELLO parity");
        assert_eq!(ti_lower, std_lower, "hello parity");
    }

    /// Test search_continuation: walk FST from a mid-DFA state.
    /// Simulates the second walk in a regex continuation chain.
    #[test]
    fn test_search_continuation_basic() {
        use crate::suffix_fst::file::SfxDfaWrapper;
        use levenshtein_automata::LevenshteinAutomatonBuilder;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();
        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // Build a DFA for "cool" d=0 (exact)
        let builder = LevenshteinAutomatonBuilder::new(0, true);
        let dfa = builder.build_dfa("cool");
        let automaton = SfxDfaWrapper(dfa);

        // Walk from start state, SI=0 only → should find "cool"
        let start = automaton.start();
        let results = sfx_dict.search_continuation(&automaton, start, true);
        let accepting: Vec<_> = results.iter().filter(|m| m.is_accepting).collect();
        assert_eq!(accepting.len(), 1, "should find 'cool' as accepting");
        assert_eq!(accepting[0].si, 0);

        // Now simulate continuation: advance DFA through "co" manually,
        // then walk from that state → should find entries starting with "ol"
        let mut state = automaton.start();
        state = automaton.accept(&state, b'c');
        state = automaton.accept(&state, b'o');
        assert!(automaton.can_match(&state), "DFA should be alive after 'co'");
        assert!(!automaton.is_match(&state), "DFA should not accept after 'co'");

        // Walk from mid-state, any SI → should find suffix "ol" (from "cool" SI=2)
        let results = sfx_dict.search_continuation(&automaton, state, false);
        let alive: Vec<_> = results.iter().filter(|m| m.si > 0).collect();
        assert!(!alive.is_empty(), "should find suffixes matching continuation from 'co'");

        // Among them, "ol" with SI=2 should lead to an accepting state
        // (after 'co' + 'ol' the DFA has seen "cool" → accept)
        let accepting: Vec<_> = results.iter().filter(|m| m.is_accepting).collect();
        assert!(!accepting.is_empty(), "should find accepting continuation after 'co'");
    }

    /// Test search_continuation returns alive-but-not-accepting entries.
    /// These entries need further continuation through gaps.
    #[test]
    fn test_search_continuation_alive_not_accepting() {
        use crate::suffix_fst::file::SfxDfaWrapper;
        use levenshtein_automata::LevenshteinAutomatonBuilder;

        let (index, body_raw) = build_test_index();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let seg_reader = &searcher.segment_readers()[0];

        let inv_idx = seg_reader.inverted_index(body_raw).unwrap();
        let termdict = inv_idx.terms();
        let sfx_data = seg_reader.sfx_file(body_raw).unwrap();
        let sfx_bytes = sfx_data.read_bytes().unwrap();
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref()).unwrap();
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, termdict);

        // DFA for "rag3dbiscool" d=0 — this spans 3 tokens ("rag3db" + "is" + "cool")
        // Walk 1 from start, SI=0: "rag3db" will be alive but NOT accepting
        // (the DFA has only seen 6 of 12 chars)
        let builder = LevenshteinAutomatonBuilder::new(0, true);
        let dfa = builder.build_dfa("rag3dbiscool");
        let automaton = SfxDfaWrapper(dfa);

        let start = automaton.start();
        let results = sfx_dict.search_continuation(&automaton, start, true);

        // "rag3db" should be alive but not accepting (6/12 chars consumed)
        let rag3db_matches: Vec<_> = results.iter()
            .filter(|m| {
                let mut buf = Vec::new();
                termdict.ord_to_term(m.raw_ordinal, &mut buf).unwrap();
                buf == b"rag3db"
            })
            .collect();
        assert!(!rag3db_matches.is_empty(), "rag3db should be found");
        assert!(
            rag3db_matches.iter().any(|m| !m.is_accepting && automaton.can_match(&m.end_state)),
            "rag3db should be alive but not accepting"
        );
    }

    fn build_test_index() -> (Index, crate::schema::Field) {
        let mut schema_builder = SchemaBuilder::new();
        let raw_opts = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets),
        );
        let body_raw = schema_builder.add_text_field("body._raw", raw_opts);
        let schema = schema_builder.build();

        let index = Index::create_in_ram(schema);
        let raw_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build();
        index.tokenizers().register("raw", raw_tokenizer);

        let mut writer = index.writer_for_tests().unwrap();

        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "import rag3db from core");
        writer.add_document(doc).unwrap();

        let mut doc = LucivyDocument::new();
        doc.add_text(body_raw, "rag3db is cool");
        writer.add_document(doc).unwrap();

        writer.commit().unwrap();

        (index, body_raw)
    }
}
