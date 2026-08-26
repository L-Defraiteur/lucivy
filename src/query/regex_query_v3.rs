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
    /// Set when a segment's prescan hit the match cap (shared by the
    /// clones the prescan tasks run on).
    truncated: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RegexQueryV3 {
    /// Creates a regex query on `raw_field`; `anchor_start` restricts matches to the start of a token.
    pub fn new(raw_field: Field, pattern: String, anchor_start: bool) -> Self {
        Self {
            field: raw_field,
            pattern,
            anchor_start,
            highlight_sink: None,
            highlight_field_name: String::new(),
            prescan_cache: HashMap::new(),
            global_doc_freq: 0,
            truncated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Attaches a sink receiving match byte offsets, grouped under `field_name`.
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
        sfx_bytes: &common::OwnedBytes,
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)> {
        use crate::suffix_fst::file_v3::SfxFileReaderV3;
        use crate::suffix_fst::briques::regex_verified;

        let reader = SfxFileReaderV3::open_owned(sfx_bytes.clone()).map_err(|e|
            crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

        // Required literals + the real regex on rebuilt windows.
        let load = |ext: &str| -> Option<common::OwnedBytes> {
            seg_reader.sfx_index_file(ext, self.field)
                .and_then(|fs| fs.read_bytes().ok())
        };
        let posmap_bytes = load("posmap");
        let bytemap_bytes = load("bytemap");
        let wsp_bytes = load("word_sfxpost");
        let sib_bytes = load("sibling_v3");
        let tt_bytes = load("termtexts");
        let wpm_bytes = load("word_pos_map");
        let ctx = crate::suffix_fst::briques::context::BriquesContext {
            reader: &reader,
            resolver: &*pr,
            filter_docs: seg_reader.doc_filter().map(|b| b as &dyn crate::query::posting_resolver::DocFilter),
            debug: false,
            trace_id: None,
            posmap: posmap_bytes.as_ref().and_then(|b| crate::suffix_fst::posmap::PosMapReader::open(b)),
            bytemap: bytemap_bytes.as_ref().and_then(|b| crate::suffix_fst::bytemap::ByteBitmapReader::open(b)),
            word_sfxpost: wsp_bytes.as_ref().and_then(|b| crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(b)),
            sibling_v3: sib_bytes.as_ref().and_then(|b| crate::suffix_fst::sibling_table::SiblingTableReader::open(b)),
            termtexts: tt_bytes.as_ref().and_then(|b| crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b)),
            word_posmap: wpm_bytes.as_ref().and_then(|b| crate::suffix_fst::word_pos_map::WordPosMapReader::open(b)),
        };
        let Some(plan) = regex_verified::plan(&self.pattern) else {
            return Err(crate::LucivyError::InvalidArgument(format!(
                "regex {:?}: cannot be parsed", self.pattern)));
        };
        let re = regex::RegexBuilder::new(&self.pattern).case_insensitive(true).build()
            .map_err(|e| crate::LucivyError::InvalidArgument(format!("regex: {e}")))?;
        let highlights = regex_verified::regex_verified(&ctx, &self.pattern, &plan, &re, seg_reader.max_doc());
        // O(n) count over the highlights; the result is a DocSet source
        // and must come out sorted by doc (see fuzzy_query_v3).
        let mut tf_map: HashMap<DocId, u32> = HashMap::new();
        for &(doc_id, _, _) in &highlights {
            *tf_map.entry(doc_id).or_insert(0) += 1;
        }
        let mut doc_tf: Vec<(DocId, u32)> = tf_map.into_iter().collect();
        doc_tf.sort_unstable_by_key(|&(d, _)| d);
        Ok((doc_tf, highlights))
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
        // The global statistics provider, never the local searcher: summing
        // max_doc() over this searcher gives ONE shard's doc count while
        // global_doc_freq is aggregated across all of them, and doc_freq >
        // doc_count trips bm25::idf ("754 >= 763" on a 4-shard v3 index,
        // distributed coherence panel). Same fix as ContainsQueryV3.
        let (scoring_enabled, global_num_docs, global_num_tokens) = match enable_scoring {
            EnableScoring::Enabled { stats, .. } => {
                let nd = stats.total_num_docs().unwrap_or(0).max(1);
                let nt = stats.total_num_tokens(self.field).unwrap_or(0);
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

impl RegexQueryV3 {
    fn prescan_one(
        &self,
        seg_reader: &SegmentReader,
    ) -> crate::Result<Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>> {
        use crate::suffix_fst::section_file::detect_sfx_version;
        let segment_id = seg_reader.segment_id();
        let Some(sfx_data) = seg_reader.sfx_file(self.field) else { return Ok(None) };
        let sfx_bytes = sfx_data.read_bytes().map_err(|e|
            crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;
        let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
        let _ = crate::suffix_fst::briques::resolve::take_truncated_here();
        let (doc_tf, highlights) = if version == 3 {
            self.prescan_segment_v3(seg_reader, &sfx_bytes)?
        } else {
            self.prescan_segment_v2(seg_reader, &sfx_bytes)?
        };
        if crate::suffix_fst::briques::resolve::take_truncated_here() {
            self.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Some((segment_id, doc_tf, highlights)))
    }

    fn record_prescan(
        &mut self,
        segment_id: crate::index::SegmentId,
        doc_tf: Vec<(DocId, u32)>,
        highlights: Vec<(DocId, usize, usize)>,
    ) {
        self.global_doc_freq += doc_tf.len() as u64;
        self.prescan_cache.insert(
            (self.cache_key(), segment_id),
            CachedPrescan::new(doc_tf, highlights),
        );
    }
}

impl Query for RegexQueryV3 {
    /// Prescan every segment on the luciole pool, as contains and fuzzy do.
    /// The loop was sequential: wall time equalled the CPU sum over segments.
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        self.prescan_cache.clear();
        self.global_doc_freq = 0;
        self.prescan_segments_more(segments)
    }

    fn prescan_segments_more(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {

        type SegOutcome = Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)>;

        if segments.len() <= 1 {
            for seg_reader in segments {
                if let Some((segment_id, doc_tf, highlights)) = self.prescan_one(seg_reader)? {
                    self.record_prescan(segment_id, doc_tf, highlights);
                }
            }
            return Ok(());
        }

        let names: Vec<String> = (0..segments.len()).map(|i| format!("seg_{i}")).collect();
        let mut tasks: Vec<(&str, Box<dyn FnOnce() -> Result<luciole::port::PortValue, String> + Send + 'static>)> =
            Vec::with_capacity(segments.len());
        for (i, seg_reader) in segments.iter().enumerate() {
            let seg = (*seg_reader).clone();
            let probe = self.clone();
            tasks.push((names[i].as_str(), Box::new(move || {
                let outcome: SegOutcome = probe.prescan_one(&seg)
                    .map_err(|e| format!("regex prescan segment: {e}"))?;
                Ok(luciole::port::PortValue::new(outcome))
            })));
        }
        let t_dag = std::time::Instant::now();
        let mut dag = luciole::scatter::build_scatter_dag(tasks);
        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("regex prescan DAG: {e}")))?;
        if crate::suffix_fst::briques::profile::enabled() {
            eprintln!("  [rx prescan] {} segments, scatter DAG wall {:.1}ms",
                segments.len(), t_dag.elapsed().as_secs_f64() * 1e3);
        }
        let map = result
            .take_output::<std::collections::HashMap<String, luciole::port::PortValue>>("collect", "results")
            .ok_or_else(|| crate::LucivyError::SystemError("regex prescan DAG: no results".into()))?;
        let mut scatter = luciole::ScatterResults::from(map);
        for name in &names {
            if let Some(Some((segment_id, doc_tf, highlights))) = scatter.take::<SegOutcome>(name) {
                self.record_prescan(segment_id, doc_tf, highlights);
            }
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

    fn prescan_truncated(&self) -> bool {
        self.truncated.load(std::sync::atomic::Ordering::Relaxed)
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
