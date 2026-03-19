//! DAG-based search orchestration for sharded indexes.
//!
//! Every step of a sharded search is a DAG node with full observability:
//!
//! ```text
//! drain ── flush ── build_weight ──┬── search_shard_0 ──┐
//!                                  ├── search_shard_1 ──┼── merge_results
//!                                  └── search_shard_2 ──┘
//! ```

use std::collections::BinaryHeap;
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
// BuildWeightNode — parse query + compile Weight with global stats
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

        let query = crate::query::build_query(
            &self.query_config, &self.schema, &self.index,
            self.highlight_sink.clone(),
        )?;

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

// ---------------------------------------------------------------------------
// SearchShardNode — execute weight on one shard
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
