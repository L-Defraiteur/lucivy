//! Shared SFX scoring layer — reusable Weight/Scorer for all v3 query types.
//!
//! `SfxWeight` creates an `SfxScorer` from pre-scanned (doc_tf, highlights) cache.
//! `SfxScorer` iterates documents with BM25 scoring + optional coverage boost.
//!
//! Used by: ContainsQueryV3, FuzzyQueryV3, RegexQueryV3.

use std::collections::HashMap;
use std::sync::Arc;

use crate::docset::{DocSet, TERMINATED};
use crate::fieldnorm::FieldNormReader;
use crate::index::SegmentId;
use crate::query::bm25::Bm25Weight;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::{EmptyScorer, Explanation, Scorer, Weight};
use crate::schema::Field;
use crate::{DocId, Score, SegmentReader};

// ─── CachedPrescan ────────────────────────────────────────────────────────

/// Cached prescan result for one segment. Unified format for all query types.
#[derive(Clone, Debug)]
pub struct CachedPrescan {
    /// Per-document term frequency pairs: (doc_id, tf).
    pub doc_tf: Vec<(DocId, u32)>,
    /// Highlight byte offsets: (doc_id, byte_from, byte_to).
    pub highlights: Vec<(DocId, usize, usize)>,
}

impl CachedPrescan {
    pub fn new(doc_tf: Vec<(DocId, u32)>, highlights: Vec<(DocId, usize, usize)>) -> Self {
        Self { doc_tf, highlights }
    }

    pub fn empty() -> Self {
        Self { doc_tf: Vec::new(), highlights: Vec::new() }
    }
}

/// Count term frequency per document from sorted doc_ids.
pub fn count_tf_sorted(doc_ids: &[DocId]) -> Vec<(DocId, u32)> {
    if doc_ids.is_empty() { return Vec::new(); }
    let mut result = Vec::with_capacity(doc_ids.len() / 2 + 1);
    let mut prev = doc_ids[0];
    let mut count = 1u32;
    for &d in &doc_ids[1..] {
        if d == prev { count += 1; }
        else { result.push((prev, count)); prev = d; count = 1; }
    }
    result.push((prev, count));
    result
}

// ─── SfxWeight ────────────────────────────────────────────────────────────

/// Weight that creates scorers from pre-scanned (doc_tf, highlights) cache.
/// Shared by all v3 query types — no SFX file access in the scorer path.
pub struct SfxWeight {
    pub(crate) raw_field: Field,
    /// Cache key prefix (e.g. "1:mutex_lock" for field 1, query "mutex_lock").
    pub(crate) cache_key: String,
    /// Pre-scanned results keyed by (cache_key, segment_id).
    pub(crate) prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    /// Global doc_freq for BM25 IDF (aggregated across all segments/shards).
    pub(crate) global_doc_freq: u64,
    /// Whether BM25 scoring is enabled.
    pub(crate) scoring_enabled: bool,
    /// Global total docs (from EnableScoring, for cross-shard BM25).
    pub(crate) global_num_docs: u64,
    /// Global total tokens for the field (for average fieldnorm).
    pub(crate) global_num_tokens: u64,
    /// Highlight sink for emitting byte offsets.
    pub(crate) highlight_sink: Option<Arc<HighlightSink>>,
    /// Field name for highlight grouping.
    pub(crate) highlight_field_name: String,
}

impl SfxWeight {
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

    fn build_scorer(
        &self, reader: &SegmentReader, boost: Score,
        doc_tf: Vec<(DocId, u32)>,
    ) -> crate::Result<Box<dyn Scorer>> {
        let fieldnorm_reader = if let Some(fnr) = reader
            .fieldnorms_readers()
            .get_field(self.raw_field)?
        {
            fnr
        } else {
            FieldNormReader::constant(reader.max_doc(), 1)
        };

        let bm25_weight = if self.scoring_enabled {
            let (total_num_docs, total_num_tokens) = if self.global_num_docs > 0 {
                (self.global_num_docs, self.global_num_tokens)
            } else {
                let inverted_index = reader.inverted_index(self.raw_field)?;
                ((reader.max_doc() as u64).max(1), inverted_index.total_num_tokens())
            };
            let average_fieldnorm = total_num_tokens as Score / total_num_docs as Score;
            let doc_freq = if self.global_doc_freq > 0 { self.global_doc_freq } else { doc_tf.len() as u64 };
            Bm25Weight::for_one_term(doc_freq, total_num_docs, average_fieldnorm)
        } else {
            Bm25Weight::for_one_term(0, 1, 1.0)
        };

        Ok(Box::new(SfxScorer::new(
            doc_tf,
            bm25_weight.boost_by(boost),
            fieldnorm_reader,
        )))
    }
}

impl Weight for SfxWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
        let segment_id = reader.segment_id();

        if let Some(cached) = self.prescan_cache.get(&(self.cache_key.clone(), segment_id)) {
            if cached.doc_tf.is_empty() {
                return Ok(Box::new(EmptyScorer));
            }
            self.emit_highlights(segment_id, &cached.highlights);
            return self.build_scorer(reader, boost, cached.doc_tf.clone());
        }

        // No cache hit — prescan should have populated this. Return empty.
        Ok(Box::new(EmptyScorer))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> crate::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) != doc {
            return Err(crate::LucivyError::InvalidArgument(format!(
                "Document {doc} does not match"
            )));
        }
        Ok(Explanation::new("SfxWeight", scorer.score()))
    }
}

// ─── SfxScorer ────────────────────────────────────────────────────────────

/// Scorer that iterates pre-verified doc IDs with BM25 scoring.
/// Reusable by all query types — just needs (doc_id, tf) pairs.
pub struct SfxScorer {
    candidates: Vec<(DocId, u32)>,
    cursor: usize,
    bm25_weight: Bm25Weight,
    fieldnorm_reader: FieldNormReader,
    /// Per-doc coverage boost for fuzzy (matched_trigrams / total_trigrams).
    coverage_boost: HashMap<DocId, f32>,
}

impl SfxScorer {
    pub fn new(
        candidates: Vec<(DocId, u32)>,
        bm25_weight: Bm25Weight,
        fieldnorm_reader: FieldNormReader,
    ) -> Self {
        Self { candidates, cursor: 0, bm25_weight, fieldnorm_reader, coverage_boost: HashMap::new() }
    }

    pub fn with_coverage(mut self, coverage: Vec<(DocId, f32)>) -> Self {
        self.coverage_boost = coverage.into_iter().collect();
        self
    }
}

impl DocSet for SfxScorer {
    fn advance(&mut self) -> DocId {
        self.cursor += 1;
        self.doc()
    }

    fn doc(&self) -> DocId {
        if self.cursor < self.candidates.len() {
            self.candidates[self.cursor].0
        } else {
            TERMINATED
        }
    }

    fn size_hint(&self) -> u32 {
        self.candidates.len() as u32
    }

    fn seek(&mut self, target: DocId) -> DocId {
        while self.doc() < target {
            if self.advance() == TERMINATED {
                return TERMINATED;
            }
        }
        self.doc()
    }
}

impl Scorer for SfxScorer {
    fn score(&mut self) -> Score {
        if self.cursor >= self.candidates.len() { return 0.0; }
        let (doc, tf) = self.candidates[self.cursor];
        let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(doc);
        let base_score = self.bm25_weight.score(fieldnorm_id, tf);
        if let Some(&miss_penalty) = self.coverage_boost.get(&doc) {
            miss_penalty * 1000.0 + base_score
        } else {
            base_score
        }
    }
}
