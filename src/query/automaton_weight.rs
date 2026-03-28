use std::io;
use std::sync::Arc;

use common::BitSet;
use tantivy_fst::Automaton;

use super::phrase_prefix_query::prefix_end;
use crate::docset::DocSet;
use crate::fieldnorm::FieldNormReader;
use crate::index::SegmentReader;
use crate::postings::{Postings, TermInfo};
use crate::query::bm25::{Bm25StatisticsProvider, Bm25Weight};
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::posting_resolver::{PostingEntry, PostingResolver, build_resolver};
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
pub struct SfxAutomatonAdapter<'a, A>(pub &'a A);

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
    /// When true, prefer .sfxpost ordinal path over inverted index posting reads.
    /// Set by queries that target raw (non-stemmed) tokens (term, fuzzy, regex via lucivy_core).
    prefer_sfxpost: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    /// Global BM25 stats provider (Arc for cross-shard/segment consistency).
    stats: Option<Arc<dyn Bm25StatisticsProvider + Send + Sync>>,
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
            prefer_sfxpost: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
            stats: None,
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
            prefer_sfxpost: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
            stats: None,
        }
    }

    /// Set global BM25 stats provider (Arc for cross-shard/segment consistency).
    pub fn with_stats(mut self, stats: Arc<dyn Bm25StatisticsProvider + Send + Sync>) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Set whether BM25 scoring is enabled. When disabled, uses fast path with ConstScorer.
    pub fn with_scoring(mut self, scoring_enabled: bool) -> Self {
        self.scoring_enabled = scoring_enabled;
        self
    }

    /// Prefer .sfxpost ordinal path over inverted index posting reads.
    /// Use for queries targeting raw (non-stemmed) tokens.
    pub fn with_prefer_sfxpost(mut self, prefer: bool) -> Self {
        self.prefer_sfxpost = prefer;
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
        Ok(self.collect_term_infos(reader, &inverted_index)?
            .into_iter().map(|(_, ti)| ti).collect())
    }

    /// Collect matching TermInfos.
    /// - SFX path (prefer_sfxpost=true): walks the suffix FST, needed for
    ///   substring/contains queries where the automaton must match suffixes.
    /// - Standard path (prefer_sfxpost=false): walks the term dict FST,
    ///   for whole-token matching (fuzzy, regex on tokens). Same as tantivy.
    /// Returns (term_bytes, TermInfo) pairs for global doc_freq lookup.
    fn collect_term_infos(
        &self,
        reader: &SegmentReader,
        inverted_index: &InvertedIndexReader,
    ) -> crate::Result<Vec<(Vec<u8>, TermInfo)>> {
        // SFX path: needed when matching suffixes (contains+regex)
        if self.prefer_sfxpost && self.json_path_bytes.is_none() {
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
                    .map(|(term_str, ti)| (term_str.into_bytes(), ti))
                    .collect());
            }
        }

        // Standard path: stream through TermDictionary
        let term_dict = inverted_index.terms();
        let mut term_stream = self.automaton_stream(term_dict)?;
        let mut term_infos = Vec::new();
        while term_stream.advance() {
            term_infos.push((term_stream.key().to_vec(), term_stream.value().clone()));
        }
        Ok(term_infos)
    }

    /// Collect matching ordinals via .sfx. Returns None if unavailable (JSON fields, old segments).
    fn collect_ordinals(&self, reader: &SegmentReader) -> crate::Result<Option<Vec<u64>>> {
        if self.json_path_bytes.is_some() {
            return Ok(None);
        }
        let sfx_data = match reader.sfx_file(self.field) {
            Some(d) => d,
            None => return Ok(None),
        };
        let sfx_bytes = sfx_data.read_bytes()
            .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
        let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
            .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;
        let inverted_index = reader.inverted_index(self.field)?;
        let sfx_dict = SfxTermDictionary::new(&sfx_reader, inverted_index.terms());
        let adapter = SfxAutomatonAdapter(&*self.automaton);
        Ok(Some(
            sfx_dict.search_automaton_ordinals(&adapter)
                .into_iter()
                .map(|(_, ord)| ord)
                .collect()
        ))
    }

    /// Get BM25 stats: (total_num_docs, average_fieldnorm).
    /// Uses global stats provider if available, otherwise falls back to per-segment.
    fn bm25_stats(&self, inverted_index: &InvertedIndexReader, max_doc: u32) -> (u64, Score) {
        let total_num_docs = self.stats.as_ref()
            .and_then(|s| s.total_num_docs().ok())
            .unwrap_or_else(|| (max_doc as u64).max(1));
        let total_num_tokens = self.stats.as_ref()
            .and_then(|s| s.total_num_tokens(self.field).ok())
            .unwrap_or_else(|| inverted_index.total_num_tokens());
        let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
        (total_num_docs, average_fieldnorm)
    }

    /// Build scorer from ordinals resolved via PostingResolver.
    /// Avoids reading posting data from the inverted index — only metadata (total_num_tokens).
    fn scorer_from_ordinals(
        &self,
        reader: &SegmentReader,
        ordinals: &[u64],
        resolver: &dyn PostingResolver,
        inverted_index: &InvertedIndexReader,
        fieldnorm_reader: &FieldNormReader,
        boost: Score,
        max_doc: u32,
    ) -> crate::Result<Box<dyn Scorer>> {
        let mut doc_bitset = BitSet::with_max_value(max_doc);

        if let Some(ref sink) = self.highlight_sink {
            let segment_id = reader.segment_id();

            if self.scoring_enabled {
                // Highlight + BM25 scoring
                let (total_num_docs, average_fieldnorm) = self.bm25_stats(inverted_index, max_doc);
                let mut scores = vec![0.0f32; max_doc as usize];

                for &ordinal in ordinals {
                    let entries = resolver.resolve(ordinal);
                    let doc_freq = resolver.doc_freq(ordinal);
                    let bm25 = Bm25Weight::for_one_term_without_explain(
                        doc_freq as u64, total_num_docs, average_fieldnorm,
                    );
                    for_each_doc_group(&entries, |doc_id, tf, doc_entries| {
                        doc_bitset.insert(doc_id);
                        let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc_id);
                        scores[doc_id as usize] += bm25.score(fieldnorm_id, tf);
                        let offsets: Vec<[usize; 2]> = doc_entries.iter()
                            .map(|e| [e.byte_from as usize, e.byte_to as usize])
                            .collect();
                        if !offsets.is_empty() {
                            sink.insert(segment_id, doc_id, &self.highlight_field_name, offsets);
                        }
                    });
                }

                let doc_bitset = BitSetDocSet::from(doc_bitset);
                Ok(Box::new(AutomatonScorer::new(doc_bitset, scores, boost)))
            } else {
                // Highlight only (no scoring)
                for &ordinal in ordinals {
                    let entries = resolver.resolve(ordinal);
                    for_each_doc_group(&entries, |doc_id, _, doc_entries| {
                        doc_bitset.insert(doc_id);
                        let offsets: Vec<[usize; 2]> = doc_entries.iter()
                            .map(|e| [e.byte_from as usize, e.byte_to as usize])
                            .collect();
                        if !offsets.is_empty() {
                            sink.insert(segment_id, doc_id, &self.highlight_field_name, offsets);
                        }
                    });
                }

                let doc_bitset = BitSetDocSet::from(doc_bitset);
                Ok(Box::new(ConstScorer::new(doc_bitset, boost)))
            }
        } else if self.scoring_enabled {
            // BM25 scoring only (no highlights)
            let (total_num_docs, average_fieldnorm) = self.bm25_stats(inverted_index, max_doc);
            let mut scores = vec![0.0f32; max_doc as usize];

            for &ordinal in ordinals {
                let entries = resolver.resolve(ordinal);
                let doc_freq = resolver.doc_freq(ordinal);
                let bm25 = Bm25Weight::for_one_term_without_explain(
                    doc_freq as u64, total_num_docs, average_fieldnorm,
                );
                for_each_doc_group(&entries, |doc_id, tf, _| {
                    doc_bitset.insert(doc_id);
                    let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc_id);
                    scores[doc_id as usize] += bm25.score(fieldnorm_id, tf);
                });
            }

            let doc_bitset = BitSetDocSet::from(doc_bitset);
            Ok(Box::new(AutomatonScorer::new(doc_bitset, scores, boost)))
        } else {
            // Fast path: no scoring, no highlights
            for &ordinal in ordinals {
                let entries = resolver.resolve(ordinal);
                for entry in &entries {
                    doc_bitset.insert(entry.doc_id);
                }
            }

            let doc_bitset = BitSetDocSet::from(doc_bitset);
            Ok(Box::new(ConstScorer::new(doc_bitset, boost)))
        }
    }

    /// Look up global doc_freq for a term via the stats provider.
    /// Falls back to the per-segment doc_freq if no provider or lookup fails.
    fn global_doc_freq(&self, term_bytes: &[u8], local_doc_freq: u32) -> u64 {
        if let Some(ref stats) = self.stats {
            let term = crate::schema::Term::from_field_bytes(self.field, term_bytes);
            stats.doc_freq(&term).unwrap_or(local_doc_freq as u64)
        } else {
            local_doc_freq as u64
        }
    }

    /// Build scorer from TermInfos via the inverted index (fallback for segments without .sfx).
    fn scorer_from_term_infos(
        &self,
        reader: &SegmentReader,
        inverted_index: &InvertedIndexReader,
        term_infos: &[(Vec<u8>, TermInfo)],
        fieldnorm_reader: &FieldNormReader,
        boost: Score,
        max_doc: u32,
    ) -> crate::Result<Box<dyn Scorer>> {
        let mut doc_bitset = BitSet::with_max_value(max_doc);

        if let Some(ref sink) = self.highlight_sink {
            let segment_id = reader.segment_id();

            if self.scoring_enabled {
                let (total_num_docs, average_fieldnorm) = self.bm25_stats(inverted_index, max_doc);
                let mut scores = vec![0.0f32; max_doc as usize];

                for (term_bytes, term_info) in term_infos {
                    let doc_freq = self.global_doc_freq(term_bytes, term_info.doc_freq);
                    let bm25 = Bm25Weight::for_one_term_without_explain(
                        doc_freq,
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
                Ok(Box::new(AutomatonScorer::new(doc_bitset, scores, boost)))
            } else {
                for (_term_bytes, term_info) in term_infos {
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
            let (total_num_docs, average_fieldnorm) = self.bm25_stats(inverted_index, max_doc);
            let mut scores = vec![0.0f32; max_doc as usize];

            for (term_bytes, term_info) in term_infos {
                let doc_freq = self.global_doc_freq(term_bytes, term_info.doc_freq);
                let bm25 = Bm25Weight::for_one_term_without_explain(
                    doc_freq,
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
            for (_term_bytes, term_info) in term_infos {
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
}

/// Iterate posting entries grouped by doc_id (entries must be sorted by doc_id).
/// Calls `on_doc(doc_id, term_freq, entries_for_doc)` for each unique document.
fn for_each_doc_group(
    entries: &[PostingEntry],
    mut on_doc: impl FnMut(DocId, u32, &[PostingEntry]),
) {
    let mut start = 0;
    while start < entries.len() {
        let doc_id = entries[start].doc_id;
        let mut end = start + 1;
        while end < entries.len() && entries[end].doc_id == doc_id {
            end += 1;
        }
        on_doc(doc_id, (end - start) as u32, &entries[start..end]);
        start = end;
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
        let inverted_index = reader.inverted_index(self.field)?;
        let fieldnorm_reader = reader
            .fieldnorms_readers()
            .get_field(self.field)?
            .unwrap_or_else(|| FieldNormReader::constant(max_doc, 1));

        // Ordinal path: use .sfxpost instead of reading posting data from inverted index.
        // Only activated when prefer_sfxpost is set (queries targeting raw tokens)
        // AND .sfxpost exists for this field.
        if self.prefer_sfxpost && reader.sfxpost_file(self.field).is_some() {
            if let Some(ordinals) = self.collect_ordinals(reader)? {
                let resolver = build_resolver(reader, self.field)?;
                return self.scorer_from_ordinals(
                    reader, &ordinals, &*resolver, &inverted_index,
                    &fieldnorm_reader, boost, max_doc,
                );
            }
        }

        // Fallback: standard TermInfo path (no .sfx, e.g. JSON fields)
        let t0 = std::time::Instant::now();
        let term_infos = self.collect_term_infos(reader, &inverted_index)?;
        let t_collect = t0.elapsed();

        let t1 = std::time::Instant::now();
        let result = self.scorer_from_term_infos(
            reader, &inverted_index, &term_infos,
            &fieldnorm_reader, boost, max_doc,
        );
        let t_score = t1.elapsed();

        if crate::diag::diag_bus().is_active() {
            eprintln!("[automaton_weight] collect={:.3}ms ({} terms) score={:.3}ms max_doc={} has_stats={}",
                t_collect.as_secs_f64() * 1000.0,
                term_infos.len(),
                t_score.as_secs_f64() * 1000.0,
                max_doc,
                self.stats.is_some(),
            );
        }
        result
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
