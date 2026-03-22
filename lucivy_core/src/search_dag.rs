//! DAG-based search orchestration for sharded indexes.
//!
//! ```text
//! drain ── flush ── build_weight ──┬── search_shard_0 ──┐
//!                                  ├── search_shard_1 ──┼── merge_results
//!                                  └── search_shard_2 ──┘
//! ```
//!
//! BuildWeightNode does parallel prescan (one thread per shard) for globally
//! consistent BM25 contains/startsWith scoring.

use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use ld_lucivy::query::Weight;
use ld_lucivy::schema::Schema;
use ld_lucivy::{DocAddress, Index};

use crate::bm25_global::AggregatedBm25StatsOwned;
use crate::handle::LucivyHandle;
use crate::query::QueryConfig;
use crate::sharded_handle::{ShardMsg, ShardedSearchResult, ScoredEntry};

// ---------------------------------------------------------------------------
// DrainNode — flush ingestion pipeline
// ---------------------------------------------------------------------------

pub(crate) struct DrainNode {
    reader_pool: luciole::Pool<super::sharded_handle::ReaderMsg>,
    router_ref: luciole::ActorRef<super::sharded_handle::RouterMsg>,
}

impl DrainNode {
    pub fn new(
        reader_pool: luciole::Pool<super::sharded_handle::ReaderMsg>,
        router_ref: luciole::ActorRef<super::sharded_handle::RouterMsg>,
    ) -> Self {
        Self { reader_pool, router_ref }
    }
}

impl Node for DrainNode {
    fn node_type(&self) -> &'static str { "drain_pipeline" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        self.reader_pool.drain("search_drain_readers");
        self.router_ref.request(
            |r| super::sharded_handle::RouterMsg::Drain(luciole::DrainMsg(r)),
            "search_drain_router",
        ).ok();
        ctx.info("pipeline drained");
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FlushNode — commit uncommitted shards
// ---------------------------------------------------------------------------

pub(crate) struct FlushNode {
    shards: Vec<Arc<LucivyHandle>>,
    shard_pool: luciole::Pool<ShardMsg>,
}

impl FlushNode {
    pub fn new(shards: Vec<Arc<LucivyHandle>>, shard_pool: luciole::Pool<ShardMsg>) -> Self {
        Self { shards, shard_pool }
    }
}

impl Node for FlushNode {
    fn node_type(&self) -> &'static str { "flush_uncommitted" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut flushed = 0u32;
        for i in 0..self.shard_pool.len() {
            if self.shards[i].has_uncommitted() {
                let _ = self.shard_pool.worker(i).request(
                    |r| ShardMsg::Commit { fast: false, reply: r },
                    "flush_shard",
                );
                flushed += 1;
            }
        }
        ctx.metric("shards_flushed", flushed as f64);
        ctx.info(&format!("flushed {} shards", flushed));
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildWeightNode — parallel prescan + compile Weight with global stats
// ---------------------------------------------------------------------------

pub(crate) struct BuildWeightNode {
    shards: Vec<Arc<LucivyHandle>>,
    schema: Schema,
    index: Index,
    query_config: QueryConfig,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
}

impl BuildWeightNode {
    pub fn new(
        shards: Vec<Arc<LucivyHandle>>,
        schema: Schema,
        index: Index,
        query_config: QueryConfig,
        highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
    ) -> Self {
        Self { shards, schema, index, query_config, highlight_sink }
    }
}

impl Node for BuildWeightNode {
    fn node_type(&self) -> &'static str { "build_weight" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let searchers: Vec<_> = self.shards.iter().map(|s| s.reader.searcher()).collect();
        let global_stats = AggregatedBm25StatsOwned::new(searchers);

        // Extract contains/startsWith terms to prescan
        let contains_terms = extract_contains_terms(&self.query_config);

        // Resolve the field for SFX prescan
        let raw_field = self.query_config.field.as_ref()
            .and_then(|name| self.schema.get_field(name).ok());

        // Collect segment readers per shard
        let all_shard_segs: Vec<Vec<ld_lucivy::SegmentReader>> = self.shards.iter()
            .map(|s| s.reader.searcher().segment_readers().to_vec())
            .collect();

        // Parallel prescan via scatter DAG (uses luciole scheduler, no new threads)
        let (merged_cache, merged_freqs) = if !contains_terms.is_empty() && raw_field.is_some() {
            use ld_lucivy::query::{
                run_sfx_walk, tokenize_query, CachedSfxResult, RawPostingEntry,
                build_resolver,
            };
            use ld_lucivy::suffix_fst::file::SfxFileReader;

            let field = raw_field.unwrap();

            // Build scatter tasks: one per shard
            type PrescanResult = (HashMap<ld_lucivy::index::SegmentId, CachedSfxResult>, HashMap<String, u64>);
            let tasks: Vec<(&str, _)> = all_shard_segs.into_iter().enumerate()
                .map(|(i, shard_segs)| {
                    let name: &str = Box::leak(format!("prescan_{i}").into_boxed_str());
                    let terms = contains_terms.clone();
                    let f = move || -> Result<luciole::PortValue, String> {
                        let mut cache = HashMap::new();
                        let mut freqs: HashMap<String, u64> = HashMap::new();

                        for seg_reader in &shard_segs {
                            let sfx_data = match seg_reader.sfx_file(field) {
                                Some(d) => d,
                                None => continue,
                            };
                            let sfx_bytes = sfx_data.read_bytes()
                                .map_err(|e| format!("read sfx: {e}"))?;
                            let sfx_reader = SfxFileReader::open(sfx_bytes.as_ref())
                                .map_err(|e| format!("open sfx: {e}"))?;

                            let pr = build_resolver(seg_reader, field)
                                .map_err(|e| format!("resolver: {e}"))?;
                            let resolver = |ord: u64| -> Vec<RawPostingEntry> {
                                pr.resolve(ord).into_iter().map(|e| {
                                    RawPostingEntry {
                                        doc_id: e.doc_id, token_index: e.position,
                                        byte_from: e.byte_from, byte_to: e.byte_to,
                                    }
                                }).collect()
                            };

                            for (query_text, prefix_only, fuzzy_d, continuation) in &terms {
                                let (tokens, seps) = tokenize_query(query_text);
                                let (doc_tf, highlights) = run_sfx_walk(
                                    &sfx_reader, &resolver, query_text,
                                    &tokens, &seps,
                                    *fuzzy_d, *prefix_only, *continuation,
                                );

                                *freqs.entry(query_text.clone()).or_insert(0) += doc_tf.len() as u64;
                                if !doc_tf.is_empty() {
                                    cache.insert(seg_reader.segment_id(), CachedSfxResult::new(doc_tf, highlights));
                                }
                            }
                        }
                        Ok(luciole::PortValue::new((cache, freqs)))
                    };
                    (name, f)
                })
                .collect();

            // Execute scatter DAG on the luciole scheduler (parallel, no new threads)
            let mut scatter_dag = luciole::scatter::build_scatter_dag(tasks);
            let mut scatter_result = luciole::execute_dag(&mut scatter_dag, None)
                .map_err(|e| format!("prescan scatter: {e}"))?;

            let scatter_map = scatter_result
                .take_output::<HashMap<String, luciole::PortValue>>("collect", "results")
                .ok_or("prescan: no scatter results")?;

            let mut mc: HashMap<ld_lucivy::index::SegmentId, CachedSfxResult> = HashMap::new();
            let mut mf: HashMap<String, u64> = HashMap::new();
            for (_name, pv) in scatter_map {
                if let Some((cache, freqs)) = pv.take::<PrescanResult>() {
                    mc.extend(cache);
                    for (key, freq) in freqs {
                        *mf.entry(key).or_insert(0) += freq;
                    }
                }
            }
            (mc, mf)
        } else {
            (HashMap::new(), HashMap::new())
        };

        // Build query once (with highlights)
        let mut query = crate::query::build_query(
            &self.query_config, &self.schema, &self.index,
            self.highlight_sink.clone(),
        )?;

        // Inject prescan results into the query
        if !merged_freqs.is_empty() {
            query.set_global_contains_doc_freqs(&merged_freqs);
            query.inject_prescan_cache(merged_cache);
        }

        let searcher_0 = self.shards[0].reader.searcher();
        let enable_scoring = ld_lucivy::query::EnableScoring::enabled_from_statistics_provider(
            &global_stats, &searcher_0,
        );
        let weight: Arc<dyn Weight> = query
            .weight(enable_scoring)
            .map_err(|e| format!("weight: {e}"))?
            .into();

        ctx.metric("compiled", 1.0);
        ctx.set_output("weight", PortValue::new(weight));
        Ok(())
    }
}

/* OLD sequential fallback (correct but no sharding benefit):
   query.prescan_segments(&all_seg_readers); // sequential, all shards
   query.weight(enable_scoring); // reads cache
*/

/// Extract contains/startsWith terms from a query config for prescan.
/// Returns (query_text, prefix_only, fuzzy_distance, continuation).
fn extract_contains_terms(config: &QueryConfig) -> Vec<(String, bool, u8, bool)> {
    match config.query_type.as_str() {
        "contains" | "sfx_contains" => {
            if config.regex.unwrap_or(false) { return vec![]; } // regex uses different path
            config.value.as_ref().map(|v| {
                vec![(v.to_lowercase(), false, config.distance.unwrap_or(0), true)]
            }).unwrap_or_default()
        }
        "startsWith" => {
            config.value.as_ref().map(|v| {
                vec![(v.to_lowercase(), true, config.distance.unwrap_or(0), false)]
            }).unwrap_or_default()
        }
        "contains_split" | "sfx_contains_split" => {
            config.value.as_ref().map(|v| {
                v.split_whitespace()
                    .map(|w| (w.to_lowercase(), false, config.distance.unwrap_or(0), true))
                    .collect()
            }).unwrap_or_default()
        }
        "startsWith_split" => {
            config.value.as_ref().map(|v| {
                v.split_whitespace()
                    .map(|w| (w.to_lowercase(), true, config.distance.unwrap_or(0), false))
                    .collect()
            }).unwrap_or_default()
        }
        _ => vec![], // term, phrase, regex, fuzzy — no SFX prescan
    }
}

// ---------------------------------------------------------------------------
// SearchShardNode — execute weight on one shard (via shard pool)
// ---------------------------------------------------------------------------

pub(crate) struct SearchShardNode {
    shard_pool: luciole::Pool<ShardMsg>,
    shard_id: usize,
    top_k: usize,
}

impl SearchShardNode {
    pub fn new(shard_pool: luciole::Pool<ShardMsg>, shard_id: usize, top_k: usize) -> Self {
        Self { shard_pool, shard_id, top_k }
    }
}

impl Node for SearchShardNode {
    fn node_type(&self) -> &'static str { "search_shard" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("weight", PortType::of::<Arc<dyn Weight>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("hits", PortType::of::<Vec<(usize, f32, DocAddress)>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let weight = ctx.input("weight")
            .ok_or("missing weight")?
            .downcast::<Arc<dyn Weight>>()
            .ok_or("wrong weight type")?
            .clone();

        let result = self.shard_pool.worker(self.shard_id).request(
            |r| ShardMsg::Search { weight, top_k: self.top_k, reply: r },
            "search_shard",
        ).map_err(|e| format!("shard_{} request: {e}", self.shard_id))?;

        let hits = result.map_err(|e| format!("shard_{}: {e}", self.shard_id))?;
        let tagged: Vec<(usize, f32, DocAddress)> = hits.into_iter()
            .map(|(score, addr)| (self.shard_id, score, addr))
            .collect();

        ctx.metric("hits", tagged.len() as f64);
        ctx.set_output("hits", PortValue::new(tagged));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergeResultsNode — binary heap merge from all shards
// ---------------------------------------------------------------------------

pub(crate) struct MergeResultsNode {
    num_shards: usize,
    top_k: usize,
}

impl MergeResultsNode {
    pub fn new(num_shards: usize, top_k: usize) -> Self {
        Self { num_shards, top_k }
    }
}

impl Node for MergeResultsNode {
    fn node_type(&self) -> &'static str { "merge_results" }
    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_shards)
            .map(|i| PortDef::required(
                Box::leak(format!("hits_{}", i).into_boxed_str()),
                PortType::of::<Vec<(usize, f32, DocAddress)>>(),
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<Vec<ShardedSearchResult>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut heap = BinaryHeap::with_capacity(self.top_k + 1);

        for i in 0..self.num_shards {
            let port = format!("hits_{}", i);
            if let Some(value) = ctx.take_input(&port) {
                if let Some(hits) = value.take::<Vec<(usize, f32, DocAddress)>>() {
                    for (shard_id, score, doc_addr) in hits {
                        heap.push(ScoredEntry { score, shard_id, doc_address: doc_addr });
                        if heap.len() > self.top_k { heap.pop(); }
                    }
                }
            }
        }

        let mut results: Vec<ShardedSearchResult> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|e| ShardedSearchResult {
                score: e.score, shard_id: e.shard_id, doc_address: e.doc_address,
            })
            .collect();
        results.reverse();

        ctx.metric("total_results", results.len() as f64);
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_search_dag — factory
// ---------------------------------------------------------------------------

pub(crate) fn build_search_dag(
    shards: &[Arc<LucivyHandle>],
    shard_pool: &luciole::Pool<ShardMsg>,
    reader_pool: &luciole::Pool<super::sharded_handle::ReaderMsg>,
    router_ref: &luciole::ActorRef<super::sharded_handle::RouterMsg>,
    schema: &Schema,
    query_config: &QueryConfig,
    top_k: usize,
    highlight_sink: Option<Arc<ld_lucivy::query::HighlightSink>>,
) -> Result<Dag, String> {
    let mut dag = Dag::new();
    let num_shards = shards.len();

    // drain → flush → build_weight
    dag.add_node("drain", DrainNode::new(reader_pool.clone(), router_ref.clone()));
    dag.add_node("flush", FlushNode::new(shards.to_vec(), shard_pool.clone()));
    dag.add_node("build_weight", BuildWeightNode::new(
        shards.to_vec(),
        schema.clone(),
        shards[0].index.clone(),
        query_config.clone(),
        highlight_sink,
    ));
    dag.connect("drain", "done", "flush", "trigger")?;
    dag.connect("flush", "done", "build_weight", "trigger")?;

    // Parallel search shards
    for i in 0..num_shards {
        dag.add_node(
            &format!("search_{}", i),
            SearchShardNode::new(shard_pool.clone(), i, top_k),
        );
        dag.connect("build_weight", "weight", &format!("search_{}", i), "weight")?;
    }

    // Merge results
    dag.add_node("merge", MergeResultsNode::new(num_shards, top_k));
    for i in 0..num_shards {
        dag.connect(
            &format!("search_{}", i), "hits",
            "merge", &format!("hits_{}", i),
        )?;
    }

    Ok(dag)
}
