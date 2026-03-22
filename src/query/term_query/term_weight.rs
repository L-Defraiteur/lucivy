use std::sync::Arc;

use super::term_scorer::TermScorer;
use crate::docset::{DocSet, COLLECT_BLOCK_BUFFER_LEN};
use crate::fieldnorm::FieldNormReader;
use crate::index::SegmentReader;
use crate::postings::{Postings, SegmentPostings};
use crate::query::bm25::Bm25Weight;
use crate::query::explanation::does_not_match;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::posting_resolver::build_resolver;
use crate::query::resolved_postings::ResolvedPostings;
use crate::query::weight::{for_each_docset_buffered, for_each_scorer};
use crate::query::{AllScorer, AllWeight, EmptyScorer, Explanation, Scorer, Weight};
use crate::schema::IndexRecordOption;
use crate::suffix_fst::SfxTermDictionary;
use crate::suffix_fst::file::SfxFileReader;
use crate::{DocId, Score, LucivyError, Term, TERMINATED};

pub struct TermWeight {
    term: Term,
    index_record_option: IndexRecordOption,
    similarity_weight: Bm25Weight,
    scoring_enabled: bool,
    prefer_sfxpost: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
}

enum TermOrEmptyOrAllScorer {
    TermScorer(Box<TermScorer>),
    ResolvedScorer(Box<ResolvedTermScorer>),
    Empty,
    AllMatch(AllScorer),
}

impl TermOrEmptyOrAllScorer {
    pub fn into_boxed_scorer(self) -> Box<dyn Scorer> {
        match self {
            TermOrEmptyOrAllScorer::TermScorer(scorer) => scorer,
            TermOrEmptyOrAllScorer::ResolvedScorer(scorer) => scorer,
            TermOrEmptyOrAllScorer::Empty => Box::new(EmptyScorer),
            TermOrEmptyOrAllScorer::AllMatch(scorer) => Box::new(scorer),
        }
    }
}

/// Scorer for term queries backed by ResolvedPostings from .sfxpost.
/// Avoids reading posting data from the inverted index. No BlockWAND.
struct ResolvedTermScorer {
    postings: ResolvedPostings,
    fieldnorm_reader: FieldNormReader,
    similarity_weight: Bm25Weight,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    segment_id: crate::index::SegmentId,
}

impl ResolvedTermScorer {
    fn new(
        postings: ResolvedPostings,
        fieldnorm_reader: FieldNormReader,
        similarity_weight: Bm25Weight,
    ) -> Self {
        Self {
            postings,
            fieldnorm_reader,
            similarity_weight,
            highlight_sink: None,
            highlight_field_name: String::new(),
            segment_id: crate::index::SegmentId::generate_random(),
        }
    }

    fn with_highlight_sink(
        mut self,
        sink: Arc<HighlightSink>,
        field_name: String,
        segment_id: crate::index::SegmentId,
    ) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self.segment_id = segment_id;
        let doc = self.postings.doc();
        self.capture_offsets(doc);
        self
    }

    #[inline]
    fn capture_offsets(&mut self, doc: DocId) {
        if doc == TERMINATED {
            return;
        }
        if let Some(ref sink) = self.highlight_sink {
            let mut offsets_buf = Vec::new();
            self.postings.append_offsets(&mut offsets_buf);
            if !offsets_buf.is_empty() {
                let offsets: Vec<[usize; 2]> = offsets_buf
                    .iter()
                    .map(|&(from, to)| [from as usize, to as usize])
                    .collect();
                sink.insert(self.segment_id, doc, &self.highlight_field_name, offsets);
            }
        }
    }

    fn explain(&self) -> Explanation {
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(self.postings.doc());
        let term_freq = self.postings.term_freq();
        self.similarity_weight.explain(fieldnorm_id, term_freq)
    }
}

impl DocSet for ResolvedTermScorer {
    #[inline]
    fn advance(&mut self) -> DocId {
        let doc = self.postings.advance();
        self.capture_offsets(doc);
        doc
    }

    #[inline]
    fn seek(&mut self, target: DocId) -> DocId {
        let doc = self.postings.seek(target);
        self.capture_offsets(doc);
        doc
    }

    #[inline]
    fn doc(&self) -> DocId {
        self.postings.doc()
    }

    fn size_hint(&self) -> u32 {
        self.postings.size_hint()
    }
}

impl Scorer for ResolvedTermScorer {
    #[inline]
    fn score(&mut self) -> Score {
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(self.doc());
        let term_freq = self.postings.term_freq();
        self.similarity_weight.score(fieldnorm_id, term_freq)
    }
}

impl Weight for TermWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        Ok(self.specialized_scorer(reader, boost)?.into_boxed_scorer())
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        match self.specialized_scorer(reader, 1.0)? {
            TermOrEmptyOrAllScorer::TermScorer(mut term_scorer) => {
                if term_scorer.doc() > doc || term_scorer.seek(doc) != doc {
                    return Err(does_not_match(doc));
                }
                let mut explanation = term_scorer.explain();
                explanation.add_context(format!("Term={:?}", self.term,));
                Ok(explanation)
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                if scorer.doc() > doc || scorer.seek(doc) != doc {
                    return Err(does_not_match(doc));
                }
                let mut explanation = scorer.explain();
                explanation.add_context(format!("Term={:?}", self.term,));
                Ok(explanation)
            }
            TermOrEmptyOrAllScorer::Empty => Err(does_not_match(doc)),
            TermOrEmptyOrAllScorer::AllMatch(_) => AllWeight.explain(reader, doc),
        }
    }

    fn count(&self, reader: &SegmentReader) -> crate::Result<u32> {
        if let Some(alive_bitset) = reader.alive_bitset() {
            Ok(self.scorer(reader, 1.0)?.count(alive_bitset))
        } else {
            let field = self.term.field();

            let inv_index = reader.inverted_index(field)?;
            let term_info = if let Some(sfx_data) = reader.sfx_file(field) {
                let sfx_bytes = sfx_data.read_bytes()
                    .map_err(|e| crate::LucivyError::SystemError(format!("read .sfx: {e}")))?;
                let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                    .map_err(|e| crate::LucivyError::SystemError(format!("open .sfx: {e}")))?;
                let sfx_dict = SfxTermDictionary::new(&sfx_reader, inv_index.terms());
                sfx_dict.get(self.term.serialized_value_bytes())?
            } else {
                inv_index.get_term_info(&self.term)?
            };
            Ok(term_info.map(|term_info| term_info.doc_freq).unwrap_or(0))
        }
    }

    /// Iterates through all of the document matched by the DocSet
    /// `DocSet` and push the scored documents to the collector.
    fn for_each(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score),
    ) -> crate::Result<()> {
        match self.specialized_scorer(reader, 1.0)? {
            TermOrEmptyOrAllScorer::TermScorer(mut term_scorer) => {
                for_each_scorer(&mut *term_scorer, callback);
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                for_each_scorer(&mut *scorer, callback);
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(mut all_scorer) => {
                for_each_scorer(&mut all_scorer, callback);
            }
        }
        Ok(())
    }

    /// Iterates through all of the document matched by the DocSet
    /// `DocSet` and push the scored documents to the collector.
    fn for_each_no_score(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(&[DocId]),
    ) -> crate::Result<()> {
        match self.specialized_scorer(reader, 1.0)? {
            TermOrEmptyOrAllScorer::TermScorer(mut term_scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut term_scorer, &mut buffer, callback);
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut *scorer, &mut buffer, callback);
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(mut all_scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut all_scorer, &mut buffer, callback);
            }
        };

        Ok(())
    }

    /// Calls `callback` with all of the `(doc, score)` for which score
    /// is exceeding a given threshold.
    ///
    /// This method is useful for the TopDocs collector.
    /// For all docsets, the blanket implementation has the benefit
    /// of prefiltering (doc, score) pairs, avoiding the
    /// virtual dispatch cost.
    ///
    /// More importantly, it makes it possible for scorers to implement
    /// important optimization (e.g. BlockWAND for union).
    fn for_each_pruning(
        &self,
        threshold: Score,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score) -> Score,
    ) -> crate::Result<()> {
        let specialized_scorer = self.specialized_scorer(reader, 1.0)?;
        match specialized_scorer {
            TermOrEmptyOrAllScorer::TermScorer(term_scorer) => {
                crate::query::boolean_query::block_wand_single_scorer(
                    *term_scorer,
                    threshold,
                    callback,
                );
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                // No BlockWAND with resolved postings — iterate all docs
                let mut threshold = threshold;
                while scorer.doc() != TERMINATED {
                    let score = scorer.score();
                    if score > threshold {
                        threshold = callback(scorer.doc(), score);
                    }
                    scorer.advance();
                }
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(_) => {
                return Err(LucivyError::InvalidArgument(
                    "for each pruning should only be called if scoring is enabled".to_string(),
                ));
            }
        }
        Ok(())
    }

    #[cfg(feature = "quickwit")]
    async fn scorer_async(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> crate::Result<Box<dyn Scorer>> {
        Ok(self.specialized_scorer_async(reader, boost).await?.into_boxed_scorer())
    }

    #[cfg(feature = "quickwit")]
    async fn count_async(&self, reader: &SegmentReader) -> crate::Result<u32> {
        if let Some(alive_bitset) = reader.alive_bitset() {
            Ok(self.scorer_async(reader, 1.0).await?.count(alive_bitset))
        } else {
            let field = self.term.field();
            let inv_index = reader.inverted_index_async(field).await?;
            let term_info = inv_index.get_term_info_async(&self.term).await?;
            Ok(term_info.map(|term_info| term_info.doc_freq).unwrap_or(0))
        }
    }

    #[cfg(feature = "quickwit")]
    async fn for_each_async(
        &self,
        reader: &SegmentReader,
        callback: &mut (dyn FnMut(DocId, Score) + Send),
    ) -> crate::Result<()> {
        match self.specialized_scorer_async(reader, 1.0).await? {
            TermOrEmptyOrAllScorer::TermScorer(mut term_scorer) => {
                for_each_scorer(&mut *term_scorer, callback);
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                for_each_scorer(&mut *scorer, callback);
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(mut all_scorer) => {
                for_each_scorer(&mut all_scorer, callback);
            }
        }
        Ok(())
    }

    #[cfg(feature = "quickwit")]
    async fn for_each_no_score_async(
        &self,
        reader: &SegmentReader,
        callback: &mut (dyn for<'a> FnMut(&'a [DocId]) + Send),
    ) -> crate::Result<()> {
        match self.specialized_scorer_async(reader, 1.0).await? {
            TermOrEmptyOrAllScorer::TermScorer(mut term_scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut term_scorer, &mut buffer, callback);
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut *scorer, &mut buffer, callback);
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(mut all_scorer) => {
                let mut buffer = [0u32; COLLECT_BLOCK_BUFFER_LEN];
                for_each_docset_buffered(&mut all_scorer, &mut buffer, callback);
            }
        };
        Ok(())
    }

    #[cfg(feature = "quickwit")]
    async fn for_each_pruning_async(
        &self,
        threshold: Score,
        reader: &SegmentReader,
        callback: &mut (dyn FnMut(DocId, Score) -> Score + Send),
    ) -> crate::Result<()> {
        let specialized_scorer = self.specialized_scorer_async(reader, 1.0).await?;
        match specialized_scorer {
            TermOrEmptyOrAllScorer::TermScorer(term_scorer) => {
                crate::query::boolean_query::block_wand_single_scorer(
                    *term_scorer,
                    threshold,
                    callback,
                );
            }
            TermOrEmptyOrAllScorer::ResolvedScorer(mut scorer) => {
                let mut threshold = threshold;
                while scorer.doc() != TERMINATED {
                    let score = scorer.score();
                    if score > threshold {
                        threshold = callback(scorer.doc(), score);
                    }
                    scorer.advance();
                }
            }
            TermOrEmptyOrAllScorer::Empty => {}
            TermOrEmptyOrAllScorer::AllMatch(_) => {
                return Err(LucivyError::InvalidArgument(
                    "for each pruning should only be called if scoring is enabled".to_string(),
                ));
            }
        }
        Ok(())
    }
}

impl TermWeight {
    pub fn new(
        term: Term,
        index_record_option: IndexRecordOption,
        similarity_weight: Bm25Weight,
        scoring_enabled: bool,
        prefer_sfxpost: bool,
    ) -> TermWeight {
        TermWeight {
            term,
            index_record_option,
            similarity_weight,
            scoring_enabled,
            prefer_sfxpost,
            highlight_sink: None,
            highlight_field_name: String::new(),
        }
    }

    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }

    pub fn term(&self) -> &Term {
        &self.term
    }

    /// We need a method to access the actual `TermScorer` implementation
    /// for `white box` test, checking in particular that the block max
    /// is correct.
    #[cfg(test)]
    pub(crate) fn term_scorer_for_test(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> crate::Result<Option<TermScorer>> {
        let scorer = self.specialized_scorer(reader, boost)?;
        Ok(match scorer {
            TermOrEmptyOrAllScorer::TermScorer(scorer) => Some(*scorer),
            _ => None,
        })
    }

    fn specialized_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> crate::Result<TermOrEmptyOrAllScorer> {
        let field = self.term.field();
        let inverted_index = reader.inverted_index(field)?;

        // Ordinal path: use .sfxpost for raw token matching (opt-in via prefer_sfxpost).
        if self.prefer_sfxpost && reader.sfxpost_file(field).is_some() {
            if let Some(sfx_data) = reader.sfx_file(field) {
                let sfx_bytes = sfx_data.read_bytes()
                    .map_err(|e| LucivyError::SystemError(format!("read .sfx: {e}")))?;
                let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                    .map_err(|e| LucivyError::SystemError(format!("open .sfx: {e}")))?;
                let sfx_dict = SfxTermDictionary::new(&sfx_reader, inverted_index.terms());

                if let Some(ordinal) = sfx_dict.get_ordinal(self.term.serialized_value_bytes())? {
                    let resolver = build_resolver(reader, field)?;
                    let doc_freq = resolver.doc_freq(ordinal);

                    if doc_freq == 0 {
                        return Ok(TermOrEmptyOrAllScorer::Empty);
                    }
                    if !self.scoring_enabled && doc_freq == reader.max_doc() {
                        return Ok(TermOrEmptyOrAllScorer::AllMatch(AllScorer::new(
                            reader.max_doc(),
                        )));
                    }

                    let entries = resolver.resolve(ordinal);
                    let postings = ResolvedPostings::from_entries(entries);
                    let fieldnorm_reader = self.fieldnorm_reader(reader)?;
                    let similarity_weight = self.similarity_weight.boost_by(boost);
                    let mut scorer = ResolvedTermScorer::new(
                        postings, fieldnorm_reader, similarity_weight,
                    );
                    if let Some(ref sink) = self.highlight_sink {
                        scorer = scorer.with_highlight_sink(
                            Arc::clone(sink),
                            self.highlight_field_name.clone(),
                            reader.segment_id(),
                        );
                    }
                    return Ok(TermOrEmptyOrAllScorer::ResolvedScorer(Box::new(scorer)));
                } else {
                    return Ok(TermOrEmptyOrAllScorer::Empty);
                }
            }
        }

        // Resolve term via standard term dict (fast, no SFX file open needed).
        let term_info = inverted_index.get_term_info(&self.term)?;
        let Some(term_info) = term_info else {
            return Ok(TermOrEmptyOrAllScorer::Empty);
        };

        if !self.scoring_enabled && term_info.doc_freq == reader.max_doc() {
            return Ok(TermOrEmptyOrAllScorer::AllMatch(AllScorer::new(
                reader.max_doc(),
            )));
        }

        let record_option = if self.highlight_sink.is_some() {
            IndexRecordOption::WithFreqsAndPositionsAndOffsets
        } else {
            self.index_record_option
        };

        let segment_postings: SegmentPostings =
            inverted_index.read_postings_from_terminfo(&term_info, record_option)?;

        let fieldnorm_reader = self.fieldnorm_reader(reader)?;
        let similarity_weight = self.similarity_weight.boost_by(boost);
        let mut scorer = TermScorer::new(segment_postings, fieldnorm_reader, similarity_weight);
        if let Some(ref sink) = self.highlight_sink {
            let segment_id = reader.segment_id();
            scorer = scorer.with_highlight_sink(Arc::clone(sink), self.highlight_field_name.clone(), segment_id);
        }
        Ok(TermOrEmptyOrAllScorer::TermScorer(Box::new(scorer)))
    }

    fn fieldnorm_reader(&self, segment_reader: &SegmentReader) -> crate::Result<FieldNormReader> {
        if self.scoring_enabled {
            if let Some(field_norm_reader) = segment_reader
                .fieldnorms_readers()
                .get_field(self.term.field())?
            {
                return Ok(field_norm_reader);
            }
        }
        Ok(FieldNormReader::constant(segment_reader.max_doc(), 1))
    }

    #[cfg(feature = "quickwit")]
    async fn specialized_scorer_async(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> crate::Result<TermOrEmptyOrAllScorer> {
        let field = self.term.field();
        let inverted_index = reader.inverted_index_async(field).await?;
        let Some(term_info) = inverted_index.get_term_info_async(&self.term).await? else {
            return Ok(TermOrEmptyOrAllScorer::Empty);
        };

        if !self.scoring_enabled && term_info.doc_freq == reader.max_doc() {
            return Ok(TermOrEmptyOrAllScorer::AllMatch(AllScorer::new(
                reader.max_doc(),
            )));
        }

        let segment_postings: SegmentPostings = inverted_index
            .read_postings_from_terminfo_async(&term_info, self.index_record_option)
            .await?;

        let fieldnorm_reader = self.fieldnorm_reader_async(reader).await?;
        let similarity_weight = self.similarity_weight.boost_by(boost);
        Ok(TermOrEmptyOrAllScorer::TermScorer(Box::new(
            TermScorer::new(segment_postings, fieldnorm_reader, similarity_weight),
        )))
    }

    #[cfg(feature = "quickwit")]
    async fn fieldnorm_reader_async(
        &self,
        segment_reader: &SegmentReader,
    ) -> crate::Result<FieldNormReader> {
        if self.scoring_enabled {
            if let Some(field_norm_reader) = segment_reader
                .fieldnorms_readers()
                .get_field_async(self.term.field())
                .await?
            {
                return Ok(field_norm_reader);
            }
        }
        Ok(FieldNormReader::constant(segment_reader.max_doc(), 1))
    }
}
