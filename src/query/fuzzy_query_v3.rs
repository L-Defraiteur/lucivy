//! FuzzyQueryV3 — standalone fuzzy substring search (d>0).
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

/// Fuzzy substring search query (d>0).
///
/// Uses trigram pigeonhole principle for candidate generation.
/// Handles both v3 (briques) and v2 (RegexContinuationQuery fallback) segments.
#[derive(Debug, Clone)]
pub struct FuzzyQueryV3 {
    field: Field,
    query_text: String,
    distance: u8,
    strict_separators: bool,
    /// How a candidate window is validated: Levenshtein within `distance`
    /// (default) or Jaro-Winkler above a similarity. The pigeonhole at
    /// `distance` generates the candidates either way.
    metric: crate::suffix_fst::briques::jaro_winkler::FuzzyMetric,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    prescan_cache: HashMap<(String, SegmentId), CachedPrescan>,
    global_doc_freq: u64,
    /// Set when a segment's prescan hit the match cap (shared by the
    /// clones the prescan tasks run on).
    truncated: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FuzzyQueryV3 {
    /// Creates a fuzzy substring query on `raw_field` allowing up to `distance` edits, validated by Levenshtein.
    pub fn new(raw_field: Field, query_text: String, distance: u8) -> Self {
        Self {
            field: raw_field,
            query_text,
            distance,
            strict_separators: false,
            metric: Default::default(),
            highlight_sink: None,
            highlight_field_name: String::new(),
            prescan_cache: HashMap::new(),
            global_doc_freq: 0,
            truncated: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Selects how candidate windows are validated (Levenshtein by default, or Jaro-Winkler with a similarity floor).
    pub fn with_metric(mut self, metric: crate::suffix_fst::briques::jaro_winkler::FuzzyMetric) -> Self {
        self.metric = metric;
        self
    }

    /// Attaches a sink receiving match byte offsets, grouped under `field_name`.
    pub fn with_highlight_sink(mut self, sink: Arc<HighlightSink>, field_name: String) -> Self {
        self.highlight_sink = Some(sink);
        self.highlight_field_name = field_name;
        self
    }

    /// Sets whether separators between tokens must match those of the query text.
    pub fn with_strict_separators(mut self, enabled: bool) -> Self {
        self.strict_separators = enabled;
        self
    }

    fn cache_key(&self) -> String {
        use crate::suffix_fst::briques::jaro_winkler::FuzzyMetric;
        match self.metric {
            FuzzyMetric::Levenshtein =>
                format!("{}:fuzzy:{}:{}", self.field.field_id(), self.query_text, self.distance),
            FuzzyMetric::JaroWinkler { min_similarity } =>
                format!("{}:fuzzy:{}:{}:jw{:.3}", self.field.field_id(), self.query_text, self.distance, min_similarity),
        }
    }

    // ─── Prescan per segment ──────────────────────────────────────────

    fn prescan_segment_v3(
        &self,
        seg_reader: &SegmentReader,
        sfx_bytes: &common::OwnedBytes,
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)> {
        use crate::suffix_fst::file_v3::SfxFileReaderV3;
        use crate::suffix_fst::briques::orchestrator;

        // A dictionary segment uses the shard's shared, memoizing reader.
        let owned_reader;
        let view;
        let reader: &SfxFileReaderV3 = match seg_reader.sfx_dictionary_field(self.field) {
            Some(f) => {
                view = match seg_reader.sfx_index_file("gmap", self.field).and_then(|g| g.read_bytes().ok()) {
                    Some(gmap) => f.sfx_reader().for_segment(gmap),
                    None => f.sfx_reader().for_segment(common::OwnedBytes::empty()),
                };
                &view
            },
            None => {
                owned_reader = SfxFileReaderV3::open_owned(sfx_bytes.clone()).map_err(|e|
                    crate::LucivyError::SystemError(format!("open SFX3: {e}")))?;
                &owned_reader
            }
        };
        let pr = crate::query::posting_resolver::build_resolver(seg_reader, self.field)?;

        // No copy: see the note in contains_query_v3::run_sfx_v3_prescan.
        let load = |ext: &str| -> Option<common::OwnedBytes> {
            seg_reader.sfx_index_file(ext, self.field)
                .and_then(|fs| fs.read_bytes().ok())
        };
        let posmap_bytes = load("posmap");
        let wsp_bytes = load("word_sfxpost");
        let sib_bytes = load("sibling_v3");
        let tt_bytes = load("termtexts");
        let wpm_bytes = load("word_pos_map");
        // A dictionary segment (`sfx_version` 4): its readers translate
        // between the shard's global ids and its local ordinals.
        let gmap_bytes = load("gmap");
        let gmap = gmap_bytes.as_ref().and_then(|b| crate::suffix_fst::gmap::GmapReader::open(b));

        let ctx = crate::suffix_fst::briques::context::BriquesContext {
            reader,
            resolver: &*pr,
            filter_docs: seg_reader.doc_filter().map(|b| b as &dyn crate::query::posting_resolver::DocFilter),
            debug: false,
            trace_id: None,
            posmap: posmap_bytes.as_ref().and_then(|b| crate::suffix_fst::posmap::PosMapReader::open(b).map(|r| match gmap { Some(g) => r.with_gmap(g), None => r })),
            word_sfxpost: wsp_bytes.as_ref().and_then(|b| crate::suffix_fst::word_sfxpost::WordSfxPostReader::open(b).map(|r| match gmap { Some(g) => r.with_gmap(g), None => r })),
            sibling_v3: sib_bytes.as_ref().and_then(|b| crate::suffix_fst::sibling_table::SiblingTableReader::open(b).map(|r| match gmap { Some(g) => r.with_gmap(g), None => r })),
            termtexts: match seg_reader.sfx_dictionary_field(self.field) { Some(f) => f.termtexts_reader(), None => tt_bytes.as_ref().and_then(|b| crate::suffix_fst::termtexts_v3::TermTextsReaderV3::open(b)) },
            word_posmap: wpm_bytes.as_ref().and_then(|b| crate::suffix_fst::word_pos_map::WordPosMapReader::open(b).map(|r| match gmap { Some(g) => r.with_gmap(g), None => r })),
        };

        let (_bitset, highlights, coverage) = orchestrator::fuzzy_v3(
            &ctx, &self.query_text, self.distance,
            self.strict_separators, seg_reader.max_doc(), self.metric,
        );

        // Term frequency per doc: the map counts in O(n) over the
        // highlights; the RESULT must be sorted by doc — a scorer built on
        // it is a DocSet, and a union of DocSets reads `doc - window_start`
        // (buffered_union): an unsorted list panicked there, and made
        // `seek` skip documents.
        let mut tf_map: HashMap<DocId, u32> = HashMap::new();
        for &(doc_id, _, _) in &highlights {
            *tf_map.entry(doc_id).or_insert(0) += 1;
        }
        let mut doc_tf: Vec<(DocId, u32)> = tf_map.into_iter().collect();
        doc_tf.sort_unstable_by_key(|&(d, _)| d);

        Ok((doc_tf, highlights, coverage))
    }

    fn prescan_segment_v2(
        &self,
        seg_reader: &SegmentReader,
        _sfx_bytes: &[u8],
    ) -> crate::Result<(Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)> {
        use crate::query::phrase_query::regex_continuation_query::run_fuzzy_prescan;
        let (doc_tf, highlights, coverage) = run_fuzzy_prescan(
            seg_reader, self.field, &self.query_text, self.distance, false, false,
        )?;
        Ok((doc_tf, highlights, coverage))
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

static FZ_ONE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FZ_ONE_MAX_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FZ_INFLIGHT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FZ_INFLIGHT_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl FuzzyQueryV3 {
    fn prescan_one(
        &self,
        seg_reader: &SegmentReader,
    ) -> crate::Result<Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)>> {
        use crate::suffix_fst::section_file::detect_sfx_version;
        use std::sync::atomic::Ordering::Relaxed;
        let t0 = std::time::Instant::now();
        let now = FZ_INFLIGHT.fetch_add(1, Relaxed) + 1;
        FZ_INFLIGHT_MAX.fetch_max(now, Relaxed);
        let _ = crate::suffix_fst::briques::resolve::take_truncated_here();
        let r = (|| {
            let segment_id = seg_reader.segment_id();
            let Some(sfx_data) = seg_reader.sfx_file(self.field) else { return Ok(None) };
            let sfx_bytes = sfx_data.read_bytes().map_err(|e|
                crate::LucivyError::SystemError(format!("prescan read .sfx: {e}")))?;
            let version = detect_sfx_version(sfx_bytes.as_ref()).unwrap_or(1);
            let (doc_tf, highlights, coverage) = if version == 3 {
                self.prescan_segment_v3(seg_reader, &sfx_bytes)?
            } else {
                self.prescan_segment_v2(seg_reader, &sfx_bytes)?
            };
            Ok(Some((segment_id, doc_tf, highlights, coverage)))
        })();
        if crate::suffix_fst::briques::resolve::take_truncated_here() {
            self.truncated.store(true, Relaxed);
        }
        FZ_INFLIGHT.fetch_sub(1, Relaxed);
        let ns = t0.elapsed().as_nanos() as u64;
        FZ_ONE_NS.fetch_add(ns, Relaxed);
        FZ_ONE_MAX_NS.fetch_max(ns, Relaxed);
        r
    }

    fn record_prescan(
        &mut self,
        segment_id: crate::index::SegmentId,
        doc_tf: Vec<(DocId, u32)>,
        highlights: Vec<(DocId, usize, usize)>,
        coverage: Vec<(DocId, f32)>,
    ) {
        self.global_doc_freq += doc_tf.len() as u64;
        self.prescan_cache.insert(
            (self.cache_key(), segment_id),
            CachedPrescan::new(doc_tf, highlights).with_coverage(coverage),
        );
    }
}

impl Query for FuzzyQueryV3 {
    /// Prescan every segment on the luciole pool, as ContainsQueryV3 does.
    ///
    /// This loop was sequential: on 800 kernel segments the fuzzy wall time
    /// equalled its CPU sum while contains ran at a concurrency of 24 — a
    /// good part of the 50× between the two was this loop alone.
    fn prescan_segments(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {
        self.prescan_cache.clear();
        self.global_doc_freq = 0;
        self.prescan_segments_more(segments)
    }

    fn prescan_segments_more(&mut self, segments: &[&SegmentReader]) -> crate::Result<()> {

        type SegOutcome = Option<(crate::index::SegmentId, Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>, Vec<(DocId, f32)>)>;

        if segments.len() <= 1 {
            for seg_reader in segments {
                if let Some((segment_id, doc_tf, highlights, coverage)) = self.prescan_one(seg_reader)? {
                    self.record_prescan(segment_id, doc_tf, highlights, coverage);
                }
            }
            return Ok(());
        }

        // Shard dictionary: the FST phase once per shard, in parallel,
        // before the segments (see `briques::plan`). Nothing on a v3 index.
        crate::suffix_fst::briques::plan::plan_fuzzy(
            segments, self.field, &self.query_text, self.distance, self.strict_separators,
        );

        let names: Vec<String> = (0..segments.len()).map(|i| format!("seg_{i}")).collect();
        let mut tasks: Vec<(&str, Box<dyn FnOnce() -> Result<luciole::port::PortValue, String> + Send + 'static>)> =
            Vec::with_capacity(segments.len());
        for (i, seg_reader) in segments.iter().enumerate() {
            let seg = (*seg_reader).clone();
            let probe = self.clone();
            tasks.push((names[i].as_str(), Box::new(move || {
                let outcome: SegOutcome = probe.prescan_one(&seg)
                    .map_err(|e| format!("fuzzy prescan segment: {e}"))?;
                Ok(luciole::port::PortValue::new(outcome))
            })));
        }

        let t_dag = std::time::Instant::now();
        let mut dag = luciole::scatter::build_scatter_dag(tasks);
        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("fuzzy prescan DAG: {e}")))?;
        if crate::suffix_fst::briques::profile::enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            eprintln!("  [fz prescan] {} segments, scatter DAG wall {:.1}ms, per-segment CPU sum {:.1}ms, max {:.1}ms, peak concurrency {}",
                segments.len(), t_dag.elapsed().as_secs_f64() * 1e3,
                FZ_ONE_NS.swap(0, Relaxed) as f64 / 1e6,
                FZ_ONE_MAX_NS.swap(0, Relaxed) as f64 / 1e6,
                FZ_INFLIGHT_MAX.swap(0, Relaxed));
        }
        let map = result
            .take_output::<std::collections::HashMap<String, luciole::port::PortValue>>("collect", "results")
            .ok_or_else(|| crate::LucivyError::SystemError("fuzzy prescan DAG: no results".into()))?;
        let mut scatter = luciole::ScatterResults::from(map);
        for name in &names {
            if let Some(Some((segment_id, doc_tf, highlights, coverage))) = scatter.take::<SegOutcome>(name) {
                self.record_prescan(segment_id, doc_tf, highlights, coverage);
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
        vec![crate::query::SfxPrescanParam {
            field: self.field,
            query_text: self.query_text.clone(),
            anchor_start: false,
            fuzzy_distance: self.distance,
            continuation: false,
            exact_match: false,
            strict_separators: self.strict_separators,
        }]
    }
}
