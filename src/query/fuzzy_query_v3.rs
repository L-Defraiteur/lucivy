//! FuzzyQueryV3 — fuzzy substring search (d>0) via trigram pigeonhole.
//!
//! Routes to SFX v3 briques when the segment has SFX3 format,
//! falls back to v2 RegexContinuationQuery for older segments.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::suffix_contains_query::CachedSfxResult;
use crate::query::phrase_query::regex_continuation_query::RegexContinuationQuery;
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::SegmentReader;

/// Fuzzy substring search query (d>0).
///
/// Uses trigram pigeonhole principle for candidate generation,
/// then validates with Levenshtein distance.
///
/// Routes to v3 briques for SFX3 segments, v2 for older.
#[derive(Debug, Clone)]
pub struct FuzzyQueryV3 {
    inner: RegexContinuationQuery,
    field: Field,
    query_text: String,
    distance: u8,
    strict_separators: bool,
    prescan_cache: Option<HashMap<(String, SegmentId), CachedSfxResult>>,
    global_doc_freq: Option<u64>,
}

impl FuzzyQueryV3 {
    /// Create a new fuzzy query.
    pub fn new(raw_field: Field, query_text: String, distance: u8) -> Self {
        let inner = RegexContinuationQuery::new(raw_field, query_text.clone(), false)
            .with_fuzzy_distance(distance);
        Self {
            inner,
            field: raw_field,
            query_text,
            distance,
            strict_separators: false,
            prescan_cache: None,
            global_doc_freq: None,
        }
    }

    /// Attach highlight sink.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.inner = self.inner.with_highlight_sink(sink, field_name);
        self
    }

    /// Enable strict separator validation.
    pub fn with_strict_separators(mut self, enabled: bool) -> Self {
        self.strict_separators = enabled;
        self
    }
}

impl Query for FuzzyQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        use crate::suffix_fst::section_file::detect_sfx_version;

        let mut has_v3 = false;
        for seg_reader in segments {
            if let Some(sfx_data) = seg_reader.sfx_file(self.field) {
                if let Ok(bytes) = sfx_data.read_bytes() {
                    if detect_sfx_version(bytes.as_ref()) == Some(3) {
                        has_v3 = true;
                        break;
                    }
                }
            }
        }

        if !has_v3 {
            return self.inner.prescan_segments(segments);
        }

        // V3 prescan
        let mut cache = HashMap::new();
        let mut doc_freq = 0u64;

        for seg_reader in segments {
            let sfx_data = match seg_reader.sfx_file(self.field) {
                Some(d) => d,
                None => continue,
            };
            let sfx_bytes = sfx_data.read_bytes().map_err(|e|
                crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;

            let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
            let segment_id = seg_reader.segment_id();

            let (doc_tf, highlights) = if version == 3 {
                use crate::suffix_fst::file_v3::SfxFileReaderV3;
                use crate::suffix_fst::briques::orchestrator;

                let reader = SfxFileReaderV3::open(sfx_bytes.as_ref()).map_err(|e|
                    crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
                let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

                let (bitset, highlights, _coverage) = orchestrator::fuzzy_v3(
                    &reader,
                    &self.query_text,
                    self.distance,
                    &*pr,
                    self.strict_separators,
                    seg_reader.max_doc(),
                );

                let doc_tf: Vec<(crate::DocId, u32)> = highlights.iter()
                    .map(|&(doc_id, _, _)| doc_id)
                    .collect::<Vec<_>>()
                    .chunks(1) // each highlight = 1 occurrence
                    .map(|c| (c[0], 1))
                    .collect();
                // Deduplicate doc_tf
                let mut tf_map: HashMap<crate::DocId, u32> = HashMap::new();
                for &(doc_id, _, _) in &highlights {
                    *tf_map.entry(doc_id).or_insert(0) += 1;
                }
                let doc_tf: Vec<(crate::DocId, u32)> = tf_map.into_iter().collect();

                (doc_tf, highlights)
            } else {
                // V2 fallback — delegate to inner's prescan for this segment
                // (simplified: just run the whole inner prescan)
                self.inner.prescan_segments(&[seg_reader])?;
                continue;
            };

            doc_freq += doc_tf.len() as u64;
            let key = format!("{}:{}", self.field.field_id(), self.query_text);
            cache.insert((key, segment_id), CachedSfxResult::new(doc_tf, highlights));
        }

        self.prescan_cache = Some(cache);
        self.global_doc_freq = Some(doc_freq);
        Ok(())
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        self.inner.collect_prescan_doc_freqs(out)
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        self.inner.set_global_contains_doc_freqs(freqs)
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        self.inner.take_prescan_cache(out)
    }

    fn inject_prescan_cache(
        &mut self,
        cache: HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        self.inner.inject_prescan_cache(cache)
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        self.inner.sfx_prescan_params()
    }

    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        // If prescan wasn't called yet, do it now for v3 segments.
        if self.prescan_cache.is_none() {
            if let Some(searcher) = enable_scoring.searcher() {
                let mut clone = self.clone();
                let seg_refs: Vec<&crate::SegmentReader> = searcher.segment_readers().iter().collect();
                clone.prescan_segments(&seg_refs)?;
                // Inject v3 cache into inner (RegexContinuationQuery)
                if let Some(ref cache) = clone.prescan_cache {
                    use crate::query::Query as _;
                    use crate::query::phrase_query::regex_continuation_query::CachedPrescanResult;
                    let mut regex_cache = HashMap::new();
                    for ((_, seg_id), sfx_result) in cache {
                        regex_cache.insert(*seg_id, CachedPrescanResult {
                            doc_tf: sfx_result.doc_tf.clone(),
                            highlights: sfx_result.highlights.clone(),
                            doc_coverage: Vec::new(),
                        });
                    }
                    clone.inner.inject_regex_prescan_cache(regex_cache);
                }
                return clone.inner.weight(enable_scoring);
            }
        }
        self.inner.weight(enable_scoring)
    }
}
