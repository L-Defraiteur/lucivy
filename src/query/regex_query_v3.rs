//! RegexQueryV3 — standalone regex substring search.
//!
//! Owns its prescan cache and creates SfxWeight directly.
//! No wrapper around RegexContinuationQuery.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::sfx_scoring::{CachedPrescan, SfxWeight};
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::{DocId, SegmentReader};

/// Regex substring search query.
///
/// Pipeline: literal extraction → resolve via briques → gap validation (DFA).
/// strict_separators = true always (the regex defines what matches).
#[derive(Debug, Clone)]
pub struct RegexQueryV3 {
    field: Field,
    pattern: String,
    anchor_start: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}

impl RegexQueryV3 {
    pub fn new(raw_field: Field, pattern: String, anchor_start: bool) -> Self {
        Self {
            field: raw_field,
            pattern,
            anchor_start,
            highlight_sink: None,
            highlight_field_name: String::new(),
            prescan_cache: HashMap::new(),
            global_doc_freq: 0,
        }
    }

    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }

    fn cache_key(&self) -> String {
        format!("{}:regex:{}", self.field.field_id(), self.pattern)
    }

    // ─── Prescan per segment ──────────────────────────────────────────

    fn prescan_segment_v3(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::suffix_fst::file_v3::SfxFileReaderV3;
        use crate::suffix_fst::briques::regex_v3;

        let reader = SfxFileReaderV3::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

        let regex = tantivy_fst::Regex::new(&self.pattern).map_err(|e|
            crate::LucivyError::InvalidArgument(format!("regex: {e}")))?;
        let automaton = crate::query::automaton_weight::SfxAutomatonAdapter(&regex);

        let termtexts_bytes = seg_reader.sfx_index_file("termtexts", self.field)
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());
        let tt_v3 = termtexts_bytes.as_ref()
            .and_then(|b| crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b));
        let tt_v2 = if tt_v3.is_none() {
            termtexts_bytes.as_ref()
                .and_then(|b| crate::suffix_fst::termtexts::TermTextsReader::open(b))
        } else { None };

        let ord_to_term = |ord: u64| -> Option<String> {
            if let Some(ref tt) = tt_v3 {
                tt.text(ord as u32).map(|s| s.to_string())
            } else if let Some(ref tt) = tt_v2 {
                tt.text(ord as u32).map(|s| s.to_string())
            } else { None }
        };

        let posmap_bytes = seg_reader.posmap_file(self.field)
            .and_then(|d| d.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());
        let bytemap_bytes = seg_reader.bytemap_file(self.field)
            .and_then(|d| d.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());

        let (_bitset, highlights) = regex_v3::regex_v3(
            &automaton, &self.pattern, &reader, &*pr, &ord_to_term,
            self.anchor_start, seg_reader.max_doc(),
            posmap_bytes.as_deref(), bytemap_bytes.as_deref(),
        );

        let mut tf_map: HashMap<DocId, u32> = HashMap::new();
        for &(doc_id, _, _) in &highlights {
            *tf_map.entry(doc_id).or_insert(0) += 1;
        }
        Ok((tf_map.into_iter().collect(), highlights))
    }

    fn prescan_segment_v2(
        &self,
        seg_reader: &SegmentReader,
        _sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::query::phrase_query::regex_continuation_query::run_regex_prescan;
        let (doc_tf, highlights) = run_regex_prescan(
            seg_reader, self.field, &self.pattern, self.anchor_start,
        )?;
        Ok((doc_tf, highlights))
    }

    fn make_weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        let (scoring_enabled, global_num_docs, global_num_tokens) = match enable_scoring {
            EnableScoring::Enabled { searcher, .. } => {
                let mut nd = 0u64;
                let mut nt = 0u64;
                for sr in searcher.segment_readers() {
                    nd += sr.max_doc() as u64;
                    if let Ok(inv) = sr.inverted_index(self.field) {
                        nt += inv.total_num_tokens();
                    }
                }
                (true, nd.max(1), nt)
            }
            _ => (false, 0, 0),
        };

        Ok(Box::new(SfxWeight {
            raw_field: self.field,
            cache_key: self.cache_key(),
            prescan_cache: self.prescan_cache.clone(),
            global_doc_freq: self.global_doc_freq,
            scoring_enabled,
            global_num_docs,
            global_num_tokens,
            highlight_sink: self.highlight_sink.clone(),
            highlight_field_name: self.highlight_field_name.clone(),
        }))
    }
}

impl Query for RegexQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        use crate::suffix_fst::section_file::detect_sfx_version;

        self.prescan_cache.clear();
        self.global_doc_freq = 0;

        for seg_reader in segments {
            let segment_id = seg_reader.segment_id();
            let sfx_data = match seg_reader.sfx_file(self.field) {
                Some(d) => d,
                None => continue,
            };
            let sfx_bytes = sfx_data.read_bytes().map_err(|e|
                crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;

            let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
            let (doc_tf, highlights) = if version == 3 {
                self.prescan_segment_v3(seg_reader, &sfx_bytes)?
            } else {
                self.prescan_segment_v2(seg_reader, &sfx_bytes)?
            };

            self.global_doc_freq += doc_tf.len() as u64;
            self.prescan_cache.insert(
                (self.cache_key(), segment_id),
                CachedPrescan::new(doc_tf, highlights),
            );
        }
        Ok(())
    }

    fn weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        if self.prescan_cache.is_empty() {
            if let Some(searcher) = enable_scoring.searcher() {
                let mut clone = self.clone();
                let seg_refs: Vec<&SegmentReader> = searcher.segment_readers().iter().collect();
                clone.prescan_segments(&seg_refs)?;
                return clone.make_weight(enable_scoring);
            }
        }
        self.make_weight(enable_scoring)
    }

    fn collect_prescan_doc_freqs(&self, out: &mut HashMap<String, u64>) {
        out.insert(self.cache_key(), self.global_doc_freq);
    }

    fn set_global_contains_doc_freqs(&mut self, freqs: &HashMap<String, u64>) {
        if let Some(&freq) = freqs.get(&self.cache_key()) {
            self.global_doc_freq = freq;
        }
    }

    fn take_prescan_cache(
        &mut self,
        out: &mut HashMap<(String, SegmentId), CachedPrescan>,
    ) {
        out.extend(self.prescan_cache.drain());
    }

    fn inject_prescan_cache(
        &mut self,
        cache: HashMap<(String, SegmentId), CachedPrescan>,
    ) {
        let key = self.cache_key();
        for ((k, sid), v) in cache {
            if k == key {
                self.prescan_cache.insert((k, sid), v);
            }
        }
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        // Regex queries don't use sfx_prescan_params (they use regex_prescan_params).
        vec![]
    }
}
