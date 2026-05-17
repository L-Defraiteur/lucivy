//! ContainsQueryV3 — unified substring search (v2 + v3).
//!
//! Routes to SFX v3 briques when the segment has SFX3 format,
//! falls back to v2 SuffixContainsQuery for older segments.
//!
//! Handles: contains, term, startsWith, phrase (all d=0 substring queries).

use std::collections::HashMap;
use std::sync::Arc;

use crate::index::SegmentId;
use crate::query::phrase_query::scoring_utils::HighlightSink;
use crate::query::phrase_query::suffix_contains_query::{
    CachedSfxResult, SuffixContainsQuery,
};
use crate::query::{EnableScoring, Query, Weight};
use crate::schema::Field;
use crate::SegmentReader;

/// Substring search query (d=0).
///
/// Supports contains, term (anchor_start + exact_match),
/// startsWith (anchor_start), and phrase queries.
///
/// Automatically routes to v3 briques for SFX3 segments,
/// v2 code for older segments.
#[derive(Debug, Clone)]
pub struct ContainsQueryV3 {
    inner: SuffixContainsQuery,
}

impl ContainsQueryV3 {
    /// Create a new contains query.
    pub fn new(raw_field: Field, query_text: String) -> Self {
        Self {
            inner: SuffixContainsQuery::new(raw_field, query_text),
        }
    }

    /// Only match at token start (startsWith mode).
    pub fn with_anchor_start(mut self) -> Self {
        self.inner = self.inner.with_anchor_start();
        self
    }

    /// Match must cover entire word(s) (term mode).
    pub fn with_exact_match(mut self) -> Self {
        self.inner = self.inner.with_exact_match();
        self
    }

    /// Enable cross-token continuation.
    pub fn with_continuation(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_continuation(enabled);
        self
    }

    /// Enable strict separator validation.
    pub fn with_strict_separators(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_strict_separators(enabled);
        self
    }

    /// Attach highlight sink.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.inner = self.inner.with_highlight_sink(sink, field_name);
        self
    }

    /// Set global doc_freq (from cross-shard aggregation).
    pub fn with_global_doc_freq(mut self, doc_freq: u64) -> Self {
        self.inner = self.inner.with_global_doc_freq(doc_freq);
        self
    }

    /// Get query text.
    pub fn query_text(&self) -> &str {
        self.inner.query_text()
    }

    /// Get prescan doc_freq.
    pub fn prescan_doc_freq(&self) -> u64 {
        self.inner.prescan_doc_freq()
    }

    /// Access the raw field from inner.
    fn inner_field(&self) -> Field {
        // SuffixContainsQuery stores raw_field — access via sfx_prescan_params
        self.inner.sfx_prescan_params().first()
            .map(|p| p.field)
            .unwrap_or(Field::from_field_id(0))
    }

    /// Get anchor_start from inner.
    fn inner_anchor_start(&self) -> bool {
        self.inner.sfx_prescan_params().first()
            .map(|p| p.anchor_start)
            .unwrap_or(false)
    }

    /// Get exact_match from inner.
    fn inner_exact_match(&self) -> bool {
        self.inner.sfx_prescan_params().first()
            .map(|p| p.exact_match)
            .unwrap_or(false)
    }

    /// Get strict_separators from inner.
    fn inner_strict_separators(&self) -> bool {
        self.inner.sfx_prescan_params().first()
            .map(|p| p.strict_separators)
            .unwrap_or(false)
    }

    /// V3 prescan for a single segment.
    fn prescan_segment_v3(
        &self,
        _seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
        query_text: &str,
    ) -> crate::Result<(Vec<(crate::DocId, u32)>, Vec<(crate::DocId, usize, usize)>)> {
        use crate::suffix_fst::file_v3::SfxFileReaderV3;
        use crate::suffix_fst::briques::orchestrator;

        let reader = SfxFileReaderV3::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;

        // Build posting resolver for this segment
        let pr = crate::query::posting_resolver::build_resolver(_seg_reader, self.inner_field())?;

        let matches = orchestrator::contains_v3(
            &reader,
            query_text,
            &*pr,
            self.inner_anchor_start(),
            self.inner_exact_match(),
            self.inner_strict_separators(),
            None,
        );

        // Convert MatchV3 → (doc_tf, highlights)
        let highlights: Vec<(crate::DocId, usize, usize)> = matches.iter()
            .map(|m| (m.doc_id, m.byte_from as usize, m.byte_to as usize))
            .collect();

        let mut doc_ids: Vec<crate::DocId> = matches.iter().map(|m| m.doc_id).collect();
        doc_ids.sort_unstable();
        let doc_tf = count_tf_sorted(&doc_ids);

        Ok((doc_tf, highlights))
    }

    /// V2 prescan for a single segment (delegate to existing code).
    fn prescan_segment_v2(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &[u8],
        query_text: &str,
    ) -> crate::Result<(Vec<(crate::DocId, u32)>, Vec<(crate::DocId, usize, usize)>)> {
        use crate::suffix_fst::file::SfxFileReader;
        use crate::query::phrase_query::suffix_contains;
        use crate::query::phrase_query::suffix_contains_query::{run_sfx_walk, tokenize_query};

        let sfx_reader = SfxFileReader::open(sfx_bytes).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX v2: {e}")))?;

        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.inner_field())?;
        let resolver = |raw_ordinal: u64| -> Vec<suffix_contains::RawPostingEntry> {
            pr.resolve(raw_ordinal).into_iter().map(|e| suffix_contains::RawPostingEntry {
                doc_id: e.doc_id, token_index: e.position,
                byte_from: e.byte_from, byte_to: e.byte_to,
            }).collect()
        };

        let termtexts_bytes = seg_reader.sfx_index_file("termtexts", self.inner_field())
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref().to_vec());
        let termtexts_reader = termtexts_bytes.as_ref()
            .and_then(|b| crate::suffix_fst::termtexts::TermTextsReader::open(b));
        let ord_to_term_fn = |ord: u64| -> Option<String> {
            termtexts_reader.as_ref()?.text(ord as u32).map(|s| s.to_string())
        };

        let (query_tokens, query_separators) = tokenize_query(query_text);
        let (doc_tf, highlights) = run_sfx_walk(
            &sfx_reader, &resolver, query_text,
            &query_tokens, &query_separators,
            self.inner_anchor_start(), self.inner_exact_match(),
            false, // continuation
            self.inner_strict_separators(),
            None, None,
        );

        Ok((doc_tf, highlights))
    }
}

/// Count term frequency per document from sorted doc_ids.
fn count_tf_sorted(doc_ids: &[crate::DocId]) -> Vec<(crate::DocId, u32)> {
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

impl Query for ContainsQueryV3 {
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        use crate::suffix_fst::section_file::detect_sfx_version;

        // Check if ANY segment is v3. If so, use v3 prescan for v3 segments
        // and v2 for the rest. For simplicity, we accumulate into the inner's cache format.
        let mut has_v3 = false;
        for seg_reader in segments {
            if let Some(sfx_data) = seg_reader.sfx_file(self.inner_field()) {
                if let Ok(bytes) = sfx_data.read_bytes() {
                    if detect_sfx_version(bytes.as_ref()) == Some(3) {
                        has_v3 = true;
                        break;
                    }
                }
            }
        }

        if !has_v3 {
            // All segments are v2 — use the existing v2 prescan
            return self.inner.prescan_segments(segments);
        }

        // Mixed or all-v3: prescan each segment individually
        let mut cache = HashMap::new();
        let mut doc_freq = 0u64;
        let query_text = self.inner.query_text().to_string();
        let field = self.inner_field();

        for seg_reader in segments {
            let segment_id = seg_reader.segment_id();
            let sfx_data = match seg_reader.sfx_file(field) {
                Some(d) => d,
                None => continue,
            };
            let sfx_bytes = sfx_data.read_bytes().map_err(|e|
                crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;

            let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);

            let (doc_tf, highlights) = if version == 3 {
                self.prescan_segment_v3(seg_reader, &sfx_bytes, &query_text)?
            } else {
                self.prescan_segment_v2(seg_reader, &sfx_bytes, &query_text)?
            };

            doc_freq += doc_tf.len() as u64;
            // Always cache — even empty results — so the scorer never falls through
            // to the v2 code path (which would crash on SFX3 magic bytes).
            let key = format!("{}:{}", field.field_id(), query_text);
            cache.insert((key, segment_id), CachedSfxResult::new(doc_tf, highlights));
        }

        self.inner = self.inner.clone()
            .with_prescan_cache(cache)
            .with_global_doc_freq(doc_freq);
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
        // If prescan wasn't called yet (direct weight() without prescan_segments),
        // do it now so the cache is populated for v3 segments.
        if self.inner.prescan_cache_is_none() {
            if let Some(searcher) = enable_scoring.searcher() {
                let mut clone = self.clone();
                let seg_refs: Vec<&SegmentReader> = searcher.segment_readers().iter().collect();
                clone.prescan_segments(&seg_refs)?;
                return clone.inner.weight(enable_scoring);
            }
        }
        self.inner.weight(enable_scoring)
    }
}
