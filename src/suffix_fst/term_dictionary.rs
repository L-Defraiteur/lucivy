//! SfxTermDictionary — term dictionary backed by the suffix FST (.sfx file).
//!
//! Drop-in replacement for TermDictionary when a .sfx file exists.
//! Filters SI=0 entries for exact/prefix/fuzzy/regex lookups (same results
//! as the standard ._raw FST). Contains lookups use any SI.
//!
//! Uses the same TermInfoStore as the underlying TermDictionary for
//! posting list resolution — ordinals are identical (both BTreeSet-sorted).

use std::io;

use lucivy_fst::{Automaton, IntoStreamer, Map, Streamer};

use super::builder::{decode_output, decode_parent_entries, ParentEntry, ParentRef};
use super::file::SfxFileReader;
use crate::postings::TermInfo;
use crate::termdict::{TermDictionary, TermOrdinal};

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
        let mut stream = self.sfx_reader.fst().search(automaton).into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                if p.si == 0 {
                    let term_info = self.termdict.term_info_from_ord(p.raw_ordinal);
                    if let Ok(s) = std::str::from_utf8(key) {
                        results.push((s.to_string(), term_info));
                    }
                    break; // SI=0 is first (sorted), take only the first
                }
            }
        }
        results
    }

    /// Range scan (SI=0 only). Returns all terms in [ge, lt) with their TermInfo.
    /// Equivalent to TermDictionary::range().ge().lt().into_stream() iterated.
    pub fn range_scan(&self, ge: &[u8], lt: Option<&[u8]>) -> Vec<(String, TermInfo)> {
        let mut builder = self.sfx_reader.fst().range().ge(ge);
        if let Some(lt_bound) = lt {
            builder = builder.lt(lt_bound);
        }
        let mut stream = builder.into_stream();
        let mut results = Vec::new();
        while let Some((key, val)) = stream.next() {
            let parents = self.decode_parents(val);
            for p in &parents {
                if p.si == 0 {
                    let term_info = self.termdict.term_info_from_ord(p.raw_ordinal);
                    if let Ok(s) = std::str::from_utf8(key) {
                        results.push((s.to_string(), term_info));
                    }
                    break;
                }
            }
        }
        results
    }

    /// Stream all terms (SI=0 only). Returns all terms with their TermInfo.
    pub fn stream_all(&self) -> Vec<(String, TermInfo)> {
        self.range_scan(&[], None)
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
            ParentRef::Single { raw_ordinal, si } => {
                vec![ParentEntry { raw_ordinal, si }]
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
    use crate::suffix_fst::SfxCollector;
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
        use crate::termdict::TermStreamer;
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
