//! ContainsQueryV3 — standalone substring search (v2 + v3 segments).
//!
//! Owns its prescan cache and creates SfxWeight directly.
//! No wrapper around SuffixContainsQuery — this IS the primary query type.
//!
//! Handles: contains, term, startsWith, phrase (all d=0 substring queries).

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::sfx_scoring::{CachedPrescan, SfxWeight, count_tf_sorted};
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::{DocId, SegmentReader};

// Re-export for backward compat (sharded_handle, search_dag, etc.)
pub use crate::query::phrase_query::sfx_scoring::CachedPrescan as CachedSfxResult;

/// Substring search query (d=0).
///
/// Supports contains, term (anchor_start + exact_match),
/// startsWith (anchor_start), and phrase queries.
///
/// Automatically routes to v3 briques for SFX3 segments,
/// v2 code for older segments.
#[derive(Debug, Clone)]
pub struct ContainsQueryV3 {
    field: Field,
    query_text: String,
    anchor_start: bool,
    exact_match: bool,
    strict_separators: bool,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
}

impl ContainsQueryV3 {
    pub fn new(raw_field: Field, query_text: String) -> Self {
        Self {
            field: raw_field,
            query_text,
            anchor_start: false,
            exact_match: false,
            strict_separators: false,
            highlight_sink: None,
            highlight_field_name: String::new(),
            prescan_cache: HashMap::new(),
            global_doc_freq: 0,
        }
    }

    pub fn with_anchor_start(mut self) -> Self { self.anchor_start = true; self }
    pub fn with_exact_match(mut self) -> Self { self.exact_match = true; self }
    pub fn with_continuation(self, _enabled: bool) -> Self { self } // v3 always does cross-token
    pub fn with_strict_separators(mut self, enabled: bool) -> Self { self.strict_separators = enabled; self }
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }
    pub fn with_global_doc_freq(mut self, doc_freq: u64) -> Self { self.global_doc_freq = doc_freq; self }

    pub fn query_text(&self) -> &str { &self.query_text }
    pub fn prescan_doc_freq(&self) -> u64 { self.global_doc_freq }

    /// Cache key: "field_id:query_text" — consistent across prescan, weight, scorer.
    fn cache_key(&self) -> String {
        format!("{}:{}", self.field.field_id(), self.query_text)
    }

    // ─── Prescan per segment ──────────────────────────────────────────

    fn prescan_segment_v3(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::suffix_fst::file_v3::SfxFileReaderV3;
        use crate::suffix_fst::briques::orchestrator;

        let reader = SfxFileReaderV3::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

        let matches = orchestrator::contains_v3(
            &reader, &self.query_text, &*pr,
            self.anchor_start, self.exact_match, self.strict_separators, None,
        );

        let highlights: Vec<(DocId, usize, usize)> = matches.iter()
            .map(|m| (m.doc_id, m.byte_from as usize, m.byte_to as usize))
            .collect();
        let mut doc_ids: Vec<DocId> = matches.iter().map(|m| m.doc_id).collect();
        doc_ids.sort_unstable();
        Ok((count_tf_sorted(&doc_ids), highlights))
    }

    fn prescan_segment_v2(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::suffix_fst::file::SfxFileReader;
        use crate::query::phrase_query::suffix_contains;
        use crate::query::phrase_query::suffix_contains_query::{run_sfx_walk, tokenize_query};

        let sfx_reader = SfxFileReader::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX v2: {e}")))?;
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;
        let resolver = |raw_ordinal: u64| -> Vec<suffix_contains::RawPostingEntry> {
            pr.resolve(raw_ordinal).into_iter().map(|e| suffix_contains::RawPostingEntry {
                doc_id: e.doc_id, token_index: e.position,
                byte_from: e.byte_from, byte_to: e.byte_to,
            }).collect()
        };

        let termtexts_bytes = seg_reader.sfx_index_file("termtexts", self.field)
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());
        let termtexts_reader = termtexts_bytes.as_ref()
            .and_then(|b| crate::suffix_fst::termtexts::TermTextsReader::open(b));
        let ord_to_term_fn = |ord: u64| -> Option<String> {
            termtexts_reader.as_ref()?.text(ord as u32).map(|s| s.to_string())
        };

        let (query_tokens, query_separators) = tokenize_query(&self.query_text);
        let (doc_tf, highlights) = run_sfx_walk(
            &sfx_reader, &resolver, &self.query_text,
            &query_tokens, &query_separators,
            self.anchor_start, self.exact_match,
            false, self.strict_separators,
            None, Some(&ord_to_term_fn),
        );

        Ok((doc_tf, highlights))
    }

    // ─── Weight creation ──────────────────────────────────────────────

    fn make_weight(&self, enable_scoring: EnableScoring) -> crate::Result<Box<dyn Weight>> {
        let (scoring_enabled, global_num_docs, global_num_tokens) = match enable_scoring {
            EnableScoring::Enabled { searcher, .. } => {
                let schema = searcher.schema();
                let (nd, nt) = schema.fields()
                    .find(|(f, _)| *f == self.field)
                    .map(|_| {
                        let searcher_ref = &searcher;
                        let mut nd = 0u64;
                        let mut nt = 0u64;
                        for sr in searcher_ref.segment_readers() {
                            nd += sr.max_doc() as u64;
                            if let Ok(inv) = sr.inverted_index(self.field) {
                                nt += inv.total_num_tokens();
                            }
                        }
                        (nd.max(1), nt)
                    })
                    .unwrap_or((1, 0));
                (true, nd, nt)
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

// ─── Query trait ──────────────────────────────────────────────────────────

impl Query for ContainsQueryV3 {
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
        // Only keep entries matching our cache_key
        let key = self.cache_key();
        for ((k, sid), v) in cache {
            if k == key {
                self.prescan_cache.insert((k, sid), v);
            }
        }
    }

    fn sfx_prescan_params(&self) -> Vec<crate::query::SfxPrescanParam> {
        vec![crate::query::SfxPrescanParam {
            field: self.field,
            query_text: self.query_text.clone(),
            anchor_start: self.anchor_start,
            fuzzy_distance: 0,
            continuation: false,
            exact_match: self.exact_match,
            strict_separators: self.strict_separators,
        }]
    }
}
