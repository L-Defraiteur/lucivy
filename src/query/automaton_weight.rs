use std::io;
use std::sync::Arc;

use common::BitSet;
use tantivy_fst::Automaton;

use super::phrase_prefix_query::prefix_end;
use crate::docset::DocSet;
use crate::fieldnorm::FieldNormReader;
use crate::index::SegmentReader;
use crate::postings::{Postings, TermInfo};
use crate::query::bm25::Bm25Weight;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::docset::COLLECT_BLOCK_BUFFER_LEN;
use crate::query::{BitSetDocSet, ConstScorer, Explanation, Scorer, Weight};
use crate::schema::{Field, IndexRecordOption};
use crate::termdict::{TermDictionary, TermStreamer};
use crate::suffix_fst::SfxTermDictionary;
use crate::suffix_fst::file::SfxFileReader;
use crate::index::InvertedIndexReader;
use crate::{DocId, Score, LucivyError, TERMINATED};

/// Bridge adapter: wraps a tantivy_fst::Automaton to implement lucivy_fst::Automaton
/// for searching the suffix FST.
pub(crate) struct SfxAutomatonAdapter<'a, A>(pub &'a A);

impl<A: Automaton> lucivy_fst::Automaton for SfxAutomatonAdapter<'_, A>
where
    A::State: Clone,
{
    type State = A::State;

    fn start(&self) -> Self::State {
        self.0.start()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        self.0.is_match(state)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        self.0.can_match(state)
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.accept(state, byte)
    }
}

/// A weight struct for Fuzzy Term and Regex Queries
pub struct AutomatonWeight<A> {
    field: Field,
    automaton: Arc<A>,
    // For JSON fields, the term dictionary include terms from all paths.
    // We apply additional filtering based on the given JSON path, when searching within the term
    // dictionary. This prevents terms from unrelated paths from matching the search criteria.
    json_path_bytes: Option<Box<[u8]>>,
    scoring_enabled: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

impl<A> AutomatonWeight<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    /// Create a new AutomationWeight
    pub fn new<IntoArcA: Into<Arc<A>>>(field: Field, automaton: IntoArcA) -> AutomatonWeight<A> {
        AutomatonWeight {
            field,
            automaton: automaton.into(),
            json_path_bytes: None,
            scoring_enabled: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Create a new AutomationWeight for a json path
    pub fn new_for_json_path<IntoArcA: Into<Arc<A>>>(
        field: Field,
        automaton: IntoArcA,
        json_path_bytes: &[u8],
    ) -> AutomatonWeight<A> {
        AutomatonWeight {
            field,
            automaton: automaton.into(),
            json_path_bytes: Some(json_path_bytes.to_vec().into_boxed_slice()),
            scoring_enabled: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    /// Set whether BM25 scoring is enabled. When disabled, uses fast path with ConstScorer.
    pub fn with_scoring(mut self, scoring_enabled: bool) -> Self {
        self.scoring_enabled = scoring_enabled;
        self
    }

    /// Attach a highlight sink to capture byte offsets during scoring.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }

    fn automaton_stream<'a>(
        &'a self,
        term_dict: &'a TermDictionary,
    ) -> io::Result<TermStreamer<'a, &'a A>> {
        let automaton: &A = &self.automaton;
        let mut term_stream_builder = term_dict.search(automaton);

        if let Some(json_path_bytes) = &self.json_path_bytes {
            term_stream_builder = term_stream_builder.ge(json_path_bytes);
            if let Some(end) = prefix_end(json_path_bytes) {
                term_stream_builder = term_stream_builder.lt(&end);
            }
        }

        term_stream_builder.into_stream()
    }

    /// Returns the term infos that match the automaton
    pub fn get_match_term_infos(&self, reader: &SegmentReader) -> crate::Result<Vec<TermInfo>> {
        let inverted_index = reader.inverted_index(self.field)?;
        self.collect_term_infos(reader, &inverted_index)
    }

    /// Collect matching TermInfos — via .sfx if available, otherwise via standard stream.
    /// JSON path queries always use the standard stream (JSON fields don't have .sfx).
    fn collect_term_infos(
        &self,
        reader: &SegmentReader,
        inverted_index: &InvertedIndexReader,
    ) -> crate::Result<Vec<TermInfo>> {
        // Try .sfx path (skip for JSON fields — they don't have suffix indexes)
        if self.json_path_bytes.is_none() {
            if let Some(sfx_data) = reader.sfx_file(self.field) {
                let sfx_bytes = sfx_data.read_bytes()
                    .map_err(|e| crate::LucivyError::SystemError(format!("read .sfx: {e}")))?;
                let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                    .map_err(|e| crate::LucivyError::SystemError(format!("open .sfx: {e}")))?;
                let sfx_dict = SfxTermDictionary::new(&sfx_reader, inverted_index.terms());
                let adapter = SfxAutomatonAdapter(&*self.automaton);
                return Ok(sfx_dict
                    .search_automaton(&adapter)
                    .into_iter()
                    .map(|(_, ti)| ti)
                    .collect());
            }
        }

        // Standard path: stream through TermDictionary
        let term_dict = inverted_index.terms();
        let mut term_stream = self.automaton_stream(term_dict)?;
        let mut term_infos = Vec::new();
        while term_stream.advance() {
            term_infos.push(term_stream.value().clone());
        }
        Ok(term_infos)
    }
}

/// Scorer that wraps a BitSetDocSet with pre-computed per-doc BM25 scores.
struct AutomatonScorer {
    doc_bitset: BitSetDocSet,
    scores: Vec<Score>,
    boost: Score,
}

impl AutomatonScorer {
    fn new(doc_bitset: BitSetDocSet, scores: Vec<Score>, boost: Score) -> Self {
        AutomatonScorer {
            doc_bitset,
            scores,
            boost,
        }
    }
}

impl DocSet for AutomatonScorer {
    fn advance(&mut self) -> DocId {
        self.doc_bitset.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.doc_bitset.seek(target)
    }

    fn fill_buffer(&mut self, buffer: &mut [DocId; COLLECT_BLOCK_BUFFER_LEN]) -> usize {
        self.doc_bitset.fill_buffer(buffer)
    }

    fn doc(&self) -> DocId {
        self.doc_bitset.doc()
    }

    fn size_hint(&self) -> u32 {
        self.doc_bitset.size_hint()
    }
}

impl Scorer for AutomatonScorer {
    #[inline]
    fn score(&mut self) -> Score {
        let doc = self.doc();
        self.scores[doc as usize] * self.boost
    }
}

impl<A> Weight for AutomatonWeight<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let max_doc = reader.max_doc();
        let mut doc_bitset = BitSet::with_max_value(max_doc);
        let inverted_index = reader.inverted_index(self.field)?;

        // Collect all matching term infos upfront (via .sfx or standard stream)
        let term_infos = self.collect_term_infos(reader, &inverted_index)?;

        // Get fieldnorm reader, falling back to constant(1) if not available (e.g. JSON fields).
        let fieldnorm_reader = reader
            .fieldnorms_readers()
            .get_field(self.field)?
            .unwrap_or_else(|| FieldNormReader::constant(max_doc, 1));

        if let Some(ref sink) = self.highlight_sink {
            // Highlight path: read full postings for offsets.
            let segment_id = reader.segment_id();

            if self.scoring_enabled {
                // Highlight + scoring: compute BM25 opportunistically while reading offsets.
                let total_num_tokens = inverted_index.total_num_tokens();
                let total_num_docs = (max_doc as u64).max(1);
                let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
                let mut scores = vec![0.0f32; max_doc as usize];

                for term_info in &term_infos {
                    let bm25 = Bm25Weight::for_one_term_without_explain(
                        term_info.doc_freq as u64,
                        total_num_docs,
                        average_fieldnorm,
                    );
                    let mut segment_postings = inverted_index.read_postings_from_terminfo(
                        term_info,
                        IndexRecordOption::WithFreqsAndPositionsAndOffsets,
                    )?;
                    loop {
                        let doc = segment_postings.doc();
                        if doc == TERMINATED {
                            break;
                        }
                        doc_bitset.insert(doc);
                        let term_freq = segment_postings.term_freq();
                        let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc);
                        scores[doc as usize] += bm25.score(fieldnorm_id, term_freq);
                        let mut offsets_buf = Vec::new();
                        segment_postings.append_offsets(&mut offsets_buf);
                        if !offsets_buf.is_empty() {
                            let offsets: Vec<[usize; 2]> = offsets_buf
                                .iter()
                                .map(|&(from, to)| [from as usize, to as usize])
                                .collect();
                            sink.insert(segment_id, doc, &self.highlight_field_name, offsets);
                        }
                        segment_postings.advance();
                    }
                }

                let doc_bitset = BitSetDocSet::from(doc_bitset);
                let scorer = AutomatonScorer::new(doc_bitset, scores, boost);
                Ok(Box::new(scorer))
            } else {
                // Highlight only (no scoring): collect offsets, use ConstScorer.
                for term_info in &term_infos {
                    let mut segment_postings = inverted_index.read_postings_from_terminfo(
                        term_info,
                        IndexRecordOption::WithFreqsAndPositionsAndOffsets,
                    )?;
                    loop {
                        let doc = segment_postings.doc();
                        if doc == TERMINATED {
                            break;
                        }
                        doc_bitset.insert(doc);
                        let mut offsets_buf = Vec::new();
                        segment_postings.append_offsets(&mut offsets_buf);
                        if !offsets_buf.is_empty() {
                            let offsets: Vec<[usize; 2]> = offsets_buf
                                .iter()
                                .map(|&(from, to)| [from as usize, to as usize])
                                .collect();
                            sink.insert(segment_id, doc, &self.highlight_field_name, offsets);
                        }
                        segment_postings.advance();
                    }
                }

                let doc_bitset = BitSetDocSet::from(doc_bitset);
                let const_scorer = ConstScorer::new(doc_bitset, boost);
                Ok(Box::new(const_scorer))
            }
        } else if self.scoring_enabled {
            // BM25 scoring path: read postings with freqs.
            let total_num_tokens = inverted_index.total_num_tokens();
            let total_num_docs = (max_doc as u64).max(1);
            let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
            let mut scores = vec![0.0f32; max_doc as usize];

            for term_info in &term_infos {
                let bm25 = Bm25Weight::for_one_term_without_explain(
                    term_info.doc_freq as u64,
                    total_num_docs,
                    average_fieldnorm,
                );
                let mut segment_postings = inverted_index.read_postings_from_terminfo(
                    term_info,
                    IndexRecordOption::WithFreqs,
                )?;
                loop {
                    let doc = segment_postings.doc();
                    if doc == TERMINATED {
                        break;
                    }
                    doc_bitset.insert(doc);
                    let term_freq = segment_postings.term_freq();
                    let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc);
                    scores[doc as usize] += bm25.score(fieldnorm_id, term_freq);
                    segment_postings.advance();
                }
            }

            let doc_bitset = BitSetDocSet::from(doc_bitset);
            let scorer = AutomatonScorer::new(doc_bitset, scores, boost);
            Ok(Box::new(scorer))
        } else {
            // Fast path: no scoring, no highlights. Block postings with Basic.
            for term_info in &term_infos {
                let mut block_segment_postings = inverted_index
                    .read_block_postings_from_terminfo(term_info, IndexRecordOption::Basic)?;
                loop {
                    let docs = block_segment_postings.docs();
                    if docs.is_empty() {
                        break;
                    }
                    for &doc in docs {
                        doc_bitset.insert(doc);
                    }
                    block_segment_postings.advance();
                }
            }

            let doc_bitset = BitSetDocSet::from(doc_bitset);
            let const_scorer = ConstScorer::new(doc_bitset, boost);
            Ok(Box::new(const_scorer))
        }
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("AutomatonScorer", 1.0))
        } else {
            Err(LucivyError::InvalidArgument(
                "Document does not exist".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use tantivy_fst::Automaton;

    use super::AutomatonWeight;
    use crate::docset::TERMINATED;
    use crate::query::Weight;
    use crate::schema::{Schema, STRING};
    use crate::{Index, IndexWriter};

    fn create_index() -> crate::Result<Index> {
        let mut schema = Schema::builder();
        let title = schema.add_text_field("title", STRING);
        let index = Index::create_in_ram(schema.build());
        let mut index_writer: IndexWriter = index.writer_for_tests()?;
        index_writer.add_document(doc!(title=>"abc"))?;
        index_writer.add_document(doc!(title=>"bcd"))?;
        index_writer.add_document(doc!(title=>"abcd"))?;
        index_writer.commit()?;
        Ok(index)
    }

    #[derive(Clone, Copy)]
    enum State {
        Start,
        NotMatching,
        AfterA,
    }

    struct PrefixedByA;

    impl Automaton for PrefixedByA {
        type State = State;

        fn start(&self) -> Self::State {
            State::Start
        }

        fn is_match(&self, state: &Self::State) -> bool {
            matches!(*state, State::AfterA)
        }

        fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
            match *state {
                State::Start => {
                    if byte == b'a' {
                        State::AfterA
                    } else {
                        State::NotMatching
                    }
                }
                State::AfterA => State::AfterA,
                State::NotMatching => State::NotMatching,
            }
        }
    }

    #[test]
    fn test_automaton_weight() -> crate::Result<()> {
        let index = create_index()?;
        let field = index.schema().get_field("title").unwrap();
        let automaton_weight = AutomatonWeight::new(field, PrefixedByA);
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let mut scorer = automaton_weight.scorer(searcher.segment_reader(0u32), 1.0)?;
        assert_eq!(scorer.doc(), 0u32);
        assert_eq!(scorer.score(), 1.0); // scoring disabled by default → ConstScorer
        assert_eq!(scorer.advance(), 2u32);
        assert_eq!(scorer.doc(), 2u32);
        assert_eq!(scorer.score(), 1.0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }

    #[test]
    fn test_automaton_weight_boost() -> crate::Result<()> {
        let index = create_index()?;
        let field = index.schema().get_field("title").unwrap();
        let automaton_weight = AutomatonWeight::new(field, PrefixedByA);
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let mut scorer = automaton_weight.scorer(searcher.segment_reader(0u32), 1.32)?;
        assert_eq!(scorer.doc(), 0u32);
        assert_eq!(scorer.score(), 1.32);
        Ok(())
    }

    #[test]
    fn test_automaton_weight_bm25() -> crate::Result<()> {
        let index = create_index()?;
        let field = index.schema().get_field("title").unwrap();
        let automaton_weight = AutomatonWeight::new(field, PrefixedByA).with_scoring(true);
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let mut scorer = automaton_weight.scorer(searcher.segment_reader(0u32), 1.0)?;
        assert_eq!(scorer.doc(), 0u32);
        assert!(scorer.score() > 0.0, "BM25 score should be positive");
        assert_ne!(scorer.score(), 1.0, "BM25 score should differ from ConstScorer");
        assert_eq!(scorer.advance(), 2u32);
        assert!(scorer.score() > 0.0);
        assert_eq!(scorer.advance(), TERMINATED);
        Ok(())
    }
}
