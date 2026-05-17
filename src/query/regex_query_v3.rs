//! RegexQueryV3 — regex substring search via literal extraction + DFA.
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

/// Regex substring search query.
///
/// Pipeline: literal extraction → resolve via briques → gap validation (DFA).
/// strict_separators = true always (the regex defines what matches).
///
/// Routes to v3 briques for SFX3 segments, v2 for older.
#[derive(Debug, Clone)]
pub struct RegexQueryV3 {
    inner: RegexContinuationQuery,
    field: Field,
    pattern: String,
    anchor_start: bool,
    prescan_cache: Option<HashMap<(String, SegmentId), CachedSfxResult>>,
    global_doc_freq: Option<u64>,
}

impl RegexQueryV3 {
    /// Create a new regex query.
    pub fn new(raw_field: Field, pattern: String, anchor_start: bool) -> Self {
        let inner = RegexContinuationQuery::from_regex(raw_field, pattern.clone(), anchor_start);
        Self {
            inner,
            field: raw_field,
            pattern,
            anchor_start,
            prescan_cache: None,
            global_doc_freq: None,
        }
    }

    /// Attach highlight sink.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.inner = self.inner.with_highlight_sink(sink, field_name);
        self
    }
}

impl Query for RegexQueryV3 {
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

        // V3 prescan with regex_v3 briques
        let mut cache = HashMap::new();
        let mut doc_freq = 0u64;

        // Build the regex automaton once (shared across segments)
        let regex = tantivy_fst::Regex::new(&self.pattern).map_err(|e|
            crate::LucivyError::InvalidArgument(format!("regex: {e}")))?;
        let automaton = crate::query::automaton_weight::SfxAutomatonAdapter(&regex);

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
                use crate::suffix_fst::briques::regex_v3;

                let reader = SfxFileReaderV3::open(sfx_bytes.as_ref()).map_err(|e|
                    crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
                let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

                // Build ord_to_term from termtexts v3
                let termtexts_bytes = seg_reader.sfx_index_file("termtexts", self.field)
                    .and_then(|fs| fs.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());

                // Try v3 termtexts first, fall back to v2
                let tt_v3 = termtexts_bytes.as_ref()
                    .and_then(|b| crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b));
                let tt_v2 = if tt_v3.is_none() {
                    termtexts_bytes.as_ref()
                        .and_then(|b| crate::suffix_fst::termtexts::TermTextsReader::open(b))
                } else {
                    None
                };

                let ord_to_term = |ord: u64| -> Option<String> {
                    if let Some(ref tt) = tt_v3 {
                        tt.text(ord as u32).map(|s| s.to_string())
                    } else if let Some(ref tt) = tt_v2 {
                        tt.text(ord as u32).map(|s| s.to_string())
                    } else {
                        None
                    }
                };

                let posmap_bytes = seg_reader.posmap_file(self.field)
                    .and_then(|d| d.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());
                let bytemap_bytes = seg_reader.bytemap_file(self.field)
                    .and_then(|d| d.read_bytes().ok())
                    .map(|b| b.as_ref().to_vec());

                let (_bitset, highlights) = regex_v3::regex_v3(
                    &automaton,
                    &self.pattern,
                    &reader,
                    &*pr,
                    &ord_to_term,
                    self.anchor_start,
                    seg_reader.max_doc(),
                    posmap_bytes.as_deref(),
                    bytemap_bytes.as_deref(),
                );

                // Convert highlights → doc_tf
                let mut tf_map: HashMap<crate::DocId, u32> = HashMap::new();
                for &(doc_id, _, _) in &highlights {
                    *tf_map.entry(doc_id).or_insert(0) += 1;
                }
                let doc_tf: Vec<(crate::DocId, u32)> = tf_map.into_iter().collect();

                (doc_tf, highlights)
            } else {
                // V2 fallback
                self.inner.prescan_segments(&[seg_reader])?;
                continue;
            };

            doc_freq += doc_tf.len() as u64;
            if !doc_tf.is_empty() {
                let key = format!("{}:regex:{}", self.field.field_id(), self.pattern);
                cache.insert((key, segment_id), CachedSfxResult::new(doc_tf, highlights));
            }
        }

        self.prescan_cache = Some(cache);
        self.global_doc_freq = Some(doc_freq);
        Ok(())
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        if let Some(freq) = self.global_doc_freq {
            let key = format!("{}:regex:{}", self.field.field_id(), self.pattern);
            out.insert(key, freq);
        } else {
            self.inner.collect_prescan_doc_freqs(out)
        }
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        let key = format!("{}:regex:{}", self.field.field_id(), self.pattern);
        if let Some(&freq) = freqs.get(&key) {
            self.global_doc_freq = Some(freq);
        }
        self.inner.set_global_contains_doc_freqs(freqs)
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        if let Some(cache) = self.prescan_cache.take() {
            out.extend(cache);
        } else {
            self.inner.take_prescan_cache(out)
        }
    }

    fn inject_prescan_cache(
        &mut self,
        cache: HashMap<(String, SegmentId), CachedSfxResult>,
    ) {
        if self.prescan_cache.is_some() {
            // V3 path — inject into our cache
            if let Some(ref mut existing) = self.prescan_cache {
                existing.extend(cache);
            }
        } else {
            self.inner.inject_prescan_cache(cache)
        }
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        self.inner.sfx_prescan_params()
    }

    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        // If we have v3 prescan cache, inject it into the inner before building weight
        if let Some(ref cache) = self.prescan_cache {
            let mut inner_clone = self.inner.clone();
            inner_clone.inject_prescan_cache(cache.clone());
            if let Some(freq) = self.global_doc_freq {
                // inner uses set_global_contains_doc_freqs
                let key = format!("{}:regex:{}", self.field.field_id(), self.pattern);
                let mut freqs = HashMap::new();
                freqs.insert(key, freq);
                inner_clone.set_global_contains_doc_freqs(&freqs);
            }
            return inner_clone.weight(enable_scoring);
        }
        self.inner.weight(enable_scoring)
    }
}
