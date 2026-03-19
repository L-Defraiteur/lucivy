//! DAG-based commit orchestration for lucivy.
//!
//! Replaces `drain_all_merges()` + manual sequencing with a structured DAG
//! that guarantees: merges parallel → end_merge → save_metas → GC → reload.
//!
//! The DAG resolves by construction:
//! - GC cannot run during merges (dependency edge)
//! - Segments without sfx (merges complete fully before commit)
//! - Mmap cache stale (single reload after all writes)

use std::sync::Arc;
use std::time::Instant;

use luciole::node::{Node, NodeContext, NodePoll, PollNode, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::merge_state::{MergeState, StepResult};
use crate::indexer::segment_entry::SegmentEntry;
use crate::indexer::segment_manager::SegmentManager;
use crate::indexer::segment_updater::SegmentUpdaterShared;
use crate::index::Index;
use crate::Opstamp;

// ---------------------------------------------------------------------------
// MergeResult — data flowing between merge and end_merge nodes
// ---------------------------------------------------------------------------

/// Result of a single merge, passed from MergeNode to EndMergeNode.
pub(crate) struct MergeResult {
    pub merge_op: MergeOperation,
    pub segment_entry: Option<SegmentEntry>,
    pub duration_ms: u64,
    pub docs_merged: u32,
}

// ---------------------------------------------------------------------------
// MergeNode — PollNode wrapping MergeState::step()
// ---------------------------------------------------------------------------

/// Executes one merge operation incrementally via `MergeState::step()`.
///
/// As a PollNode, it yields after each step — in multi-thread mode this is
/// near-free, in WASM single-thread it lets other work items run.
pub(crate) struct MergeNode {
    shared: Arc<SegmentUpdaterShared>,
    merge_op: MergeOperation,
    state: Option<MergeState>,
    initialized: bool,
}

impl MergeNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>, merge_op: MergeOperation) -> Self {
        Self {
            shared,
            merge_op,
            state: None,
            initialized: false,
        }
    }
}

impl PollNode for MergeNode {
    fn node_type(&self) -> &'static str { "merge" }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("result", PortType::of::<MergeResult>())]
    }

    fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String> {
        // Lazy init: create MergeState on first poll
        if !self.initialized {
            self.initialized = true;

            let segment_entries = self.shared.segment_manager
                .start_merge(self.merge_op.segment_ids())
                .map_err(|e| format!("start_merge failed: {e}"))?;

            match MergeState::new(
                &self.shared.index,
                segment_entries,
                self.merge_op.target_opstamp(),
            ) {
                Ok(Some(state)) => {
                    let total_docs = state.total_docs();
                    ctx.metric("total_docs", total_docs as f64);
                    ctx.info(&format!(
                        "starting merge of {} segments ({} docs)",
                        self.merge_op.segment_ids().len(),
                        total_docs,
                    ));
                    self.state = Some(state);
                }
                Ok(None) => {
                    // Empty merge — 0 docs
                    ctx.metric("total_docs", 0.0);
                    ctx.info("empty merge (0 docs), skipping");
                    ctx.set_output("result", PortValue::new(MergeResult {
                        merge_op: MergeOperation::new(
                            self.merge_op.target_opstamp(),
                            self.merge_op.segment_ids().to_vec(),
                        ),
                        segment_entry: None,
                        duration_ms: 0,
                        docs_merged: 0,
                    }));
                    return Ok(NodePoll::Ready);
                }
                Err(e) => {
                    return Err(format!("MergeState::new failed: {e}"));
                }
            }
        }

        // Step the merge
        let state = self.state.as_mut().unwrap();
        match state.step() {
            StepResult::Continue => Ok(NodePoll::Pending),
            StepResult::Done(segment_entry) => {
                let duration_ms = state.merge_start().elapsed().as_millis() as u64;
                let docs = state.total_docs();
                ctx.metric("duration_ms", duration_ms as f64);
                ctx.metric("docs_merged", docs as f64);
                ctx.info(&format!("merge completed: {} docs in {}ms", docs, duration_ms));

                ctx.set_output("result", PortValue::new(MergeResult {
                    merge_op: MergeOperation::new(
                        self.merge_op.target_opstamp(),
                        self.merge_op.segment_ids().to_vec(),
                    ),
                    segment_entry,
                    duration_ms,
                    docs_merged: docs,
                }));
                Ok(NodePoll::Ready)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EndMergeNode — collect merge results, call segment_manager.end_merge()
// ---------------------------------------------------------------------------

pub(crate) struct EndMergeNode {
    shared: Arc<SegmentUpdaterShared>,
    num_merges: usize,
}

impl EndMergeNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>, num_merges: usize) -> Self {
        Self { shared, num_merges }
    }
}

impl Node for EndMergeNode {
    fn node_type(&self) -> &'static str { "end_merge" }

    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_merges)
            .map(|i| PortDef::required(
                // Leak a string to get &'static str — OK for DAG lifetime
                Box::leak(format!("result_{}", i).into_boxed_str()),
                PortType::of::<MergeResult>(),
            ))
            .collect()
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut total_docs = 0u32;
        let mut total_merges = 0u32;

        for i in 0..self.num_merges {
            let port_name = format!("result_{}", i);
            if let Some(value) = ctx.take_input(&port_name) {
                if let Some(result) = value.take::<MergeResult>() {
                    total_docs += result.docs_merged;
                    total_merges += 1;

                    // Apply merge result to segment manager
                    let after_entry = result.segment_entry;

                    // Advance deletes if needed (same logic as do_end_merge)
                    let after_entry = if let Some(mut entry) = after_entry {
                        let mut delete_cursor = entry.delete_cursor().clone();
                        if let Some(delete_op) = delete_cursor.get() {
                            let committed_opstamp = self.shared.load_meta().opstamp;
                            if delete_op.opstamp < committed_opstamp {
                                let segment = self.shared.index.segment(entry.meta().clone());
                                crate::indexer::index_writer::advance_deletes(
                                    segment, &mut entry, committed_opstamp,
                                ).map_err(|e| format!("advance_deletes: {e}"))?;
                            }
                        }
                        Some(entry)
                    } else {
                        None
                    };

                    self.shared.segment_manager.end_merge(
                        result.merge_op.segment_ids(),
                        after_entry,
                    ).map_err(|e| format!("end_merge: {e}"))?;
                }
            }
        }

        ctx.metric("total_docs", total_docs as f64);
        ctx.metric("merges_completed", total_merges as f64);
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PurgeDeletesNode
// ---------------------------------------------------------------------------

pub(crate) struct PurgeDeletesNode {
    shared: Arc<SegmentUpdaterShared>,
    opstamp: Opstamp,
}

impl PurgeDeletesNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>, opstamp: Opstamp) -> Self {
        Self { shared, opstamp }
    }
}

impl Node for PurgeDeletesNode {
    fn node_type(&self) -> &'static str { "purge_deletes" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let entries = self.shared.purge_deletes(self.opstamp)
            .map_err(|e| format!("purge_deletes: {e}"))?;
        self.shared.segment_manager.commit(entries);
        ctx.metric("opstamp", self.opstamp as f64);
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SaveMetasNode
// ---------------------------------------------------------------------------

pub(crate) struct SaveMetasNode {
    shared: Arc<SegmentUpdaterShared>,
    opstamp: Opstamp,
    payload: Option<String>,
}

impl SaveMetasNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>, opstamp: Opstamp, payload: Option<String>) -> Self {
        Self { shared, opstamp, payload }
    }
}

impl Node for SaveMetasNode {
    fn node_type(&self) -> &'static str { "save_metas" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        self.shared.save_metas(self.opstamp, self.payload.clone())
            .map_err(|e| format!("save_metas: {e}"))?;
        ctx.info(&format!("saved metas at opstamp {}", self.opstamp));
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GCNode
// ---------------------------------------------------------------------------

pub(crate) struct GCNode {
    shared: Arc<SegmentUpdaterShared>,
}

impl GCNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>) -> Self {
        Self { shared }
    }
}

impl Node for GCNode {
    fn node_type(&self) -> &'static str { "gc" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let result = super::segment_updater::garbage_collect_files(&self.shared)
            .map_err(|e| format!("gc: {e}"))?;
        ctx.metric("deleted_files", result.deleted_files.len() as f64);
        ctx.info(&format!("GC removed {} files", result.deleted_files.len()));
        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ReloadNode
// ---------------------------------------------------------------------------

pub(crate) struct ReloadNode {
    shared: Arc<SegmentUpdaterShared>,
}

impl ReloadNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>) -> Self {
        Self { shared }
    }
}

impl Node for ReloadNode {
    fn node_type(&self) -> &'static str { "reload" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // The reader picks up the new metas on next search
        // (searcher is reopened lazily via load_meta).
        // Nothing explicit needed here — save_metas already wrote the new meta.
        ctx.info("reload complete");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_commit_dag — factory function
// ---------------------------------------------------------------------------

/// Build the commit DAG for a full commit (rebuild_sfx=true).
///
/// ```text
/// ┌─ purge_deletes ──────────────────────────────────────┐
/// │                                                       │
/// │  merge_0 ─┐                                          │
/// │  merge_1 ─┼─▸ end_merge ─▸ save_metas ─▸ gc ─▸ reload│
/// │  merge_2 ─┘                                          │
/// └──────────────────────────────────────────────────────┘
/// ```
pub(crate) fn build_commit_dag(
    shared: Arc<SegmentUpdaterShared>,
    merge_ops: Vec<MergeOperation>,
    opstamp: Opstamp,
    payload: Option<String>,
) -> Result<Dag, String> {
    let mut dag = Dag::new();
    let num_merges = merge_ops.len();

    // Purge deletes + commit segment manager (independent of merges)
    dag.add_node("purge", PurgeDeletesNode::new(shared.clone(), opstamp));

    if num_merges > 0 {
        // Add merge nodes (parallel)
        for (i, op) in merge_ops.into_iter().enumerate() {
            dag.add_poll_node(&format!("merge_{}", i), MergeNode::new(shared.clone(), op));
        }

        // End merge collects all results
        dag.add_node("end_merge", EndMergeNode::new(shared.clone(), num_merges));
        for i in 0..num_merges {
            dag.connect(
                &format!("merge_{}", i), "result",
                "end_merge", &format!("result_{}", i),
            )?;
        }

        // Save metas depends on both purge and end_merge
        dag.add_node("save", SaveMetasNode::new(shared.clone(), opstamp, payload));
        dag.connect("purge", "done", "save", "trigger")?;
        dag.connect("end_merge", "done", "save", "trigger")?;
    } else {
        // No merges — save metas directly after purge
        dag.add_node("save", SaveMetasNode::new(shared.clone(), opstamp, payload));
        dag.connect("purge", "done", "save", "trigger")?;
    }

    // GC → Reload (sequential after save)
    dag.add_node("gc", GCNode::new(shared.clone()));
    dag.add_node("reload", ReloadNode::new(shared.clone()));
    dag.chain(&["save", "gc", "reload"])?;

    Ok(dag)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::RamDirectory;
    use crate::indexer::merge_policy::tests::MergeWheneverPossible;
    use crate::schema::*;
    use crate::{Index, IndexWriter};
    use crate::indexer::events::IndexEvent;

    /// Reproduce the `garbage_collect_works_as_intended` scenario
    /// but with full DAG observability to debug why merges fail.
    #[test]
    fn test_commit_dag_observability() -> crate::Result<()> {
        let directory = RamDirectory::create();
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_u64_field("num", INDEXED);
        let schema = schema_builder.build();
        let index = Index::create(directory.clone(), schema, crate::IndexSettings::default())?;

        let mut writer: IndexWriter = index.writer_with_num_threads(1, 32_000_000)?;
        writer.set_merge_policy(Box::new(MergeWheneverPossible));

        // Subscribe to index events for observability
        let events_rx = writer.subscribe_index_events();

        // Create 4 segments
        for seg in 0..4 {
            for i in 0u64..100u64 {
                writer.add_document(doc!(field => i))?;
            }
            writer.commit()?;
            eprintln!("[test] commit #{} done", seg);
        }

        // Collect and print all events so far
        let mut events = Vec::new();
        while let Some(evt) = events_rx.try_recv() {
            events.push(evt);
        }

        eprintln!("\n=== Index Events ({}) ===", events.len());
        for evt in &events {
            match evt {
                IndexEvent::MergeStarted { segment_ids, target_opstamp } => {
                    let ids: Vec<String> = segment_ids.iter()
                        .map(|id| id.uuid_string()[..8].to_string())
                        .collect();
                    eprintln!("  MERGE STARTED: {} segments {:?} opstamp={}",
                        segment_ids.len(), ids, target_opstamp);
                }
                IndexEvent::MergeCompleted { segment_ids, duration, result_num_docs } => {
                    let ids: Vec<String> = segment_ids.iter()
                        .map(|id| id.uuid_string()[..8].to_string())
                        .collect();
                    eprintln!("  MERGE COMPLETED: {:?} -> {} docs in {:?}",
                        ids, result_num_docs, duration);
                }
                IndexEvent::MergeFailed { segment_ids, error } => {
                    let ids: Vec<String> = segment_ids.iter()
                        .map(|id| id.uuid_string()[..8].to_string())
                        .collect();
                    eprintln!("  MERGE FAILED: {:?} error={}", ids, error);
                }
                IndexEvent::CommitStarted { opstamp } => {
                    eprintln!("  COMMIT STARTED: opstamp={}", opstamp);
                }
                IndexEvent::CommitCompleted { opstamp, duration } => {
                    eprintln!("  COMMIT COMPLETED: opstamp={} {:?}", opstamp, duration);
                }
                _ => {}
            }
        }

        // Now check segment state
        let reader = index.reader()?;
        let num_segments = reader.searcher().segment_readers().len();
        let num_docs = reader.searcher().num_docs();
        eprintln!("\n=== State after 4 commits ===");
        eprintln!("  segments: {}", num_segments);
        eprintln!("  docs: {}", num_docs);

        // Wait for merges
        eprintln!("\n=== Waiting for merging threads ===");
        writer.wait_merging_threads()?;

        // Collect more events
        while let Some(evt) = events_rx.try_recv() {
            match &evt {
                IndexEvent::MergeCompleted { segment_ids, result_num_docs, duration } => {
                    let ids: Vec<String> = segment_ids.iter()
                        .map(|id| id.uuid_string()[..8].to_string())
                        .collect();
                    eprintln!("  MERGE COMPLETED (post-wait): {:?} -> {} docs {:?}",
                        ids, result_num_docs, duration);
                }
                IndexEvent::MergeFailed { segment_ids, error } => {
                    let ids: Vec<String> = segment_ids.iter()
                        .map(|id| id.uuid_string()[..8].to_string())
                        .collect();
                    eprintln!("  MERGE FAILED (post-wait): {:?} error={}", ids, error);
                }
                _ => {}
            }
        }

        reader.reload()?;
        let num_segments_after = reader.searcher().segment_readers().len();
        let num_docs_after = reader.searcher().num_docs();
        eprintln!("\n=== State after wait ===");
        eprintln!("  segments: {} (expected 1)", num_segments_after);
        eprintln!("  docs: {} (expected 400)", num_docs_after);

        // Don't assert yet — we want to see the output
        if num_segments_after != 1 {
            eprintln!("\n!!! BUG: expected 1 segment, got {}", num_segments_after);
            eprintln!("Segment details:");
            for (i, reader) in reader.searcher().segment_readers().iter().enumerate() {
                eprintln!("  seg[{}]: {} docs, id={}",
                    i, reader.num_docs(), reader.segment_id().uuid_string()[..8].to_string());
            }
        }

        assert_eq!(num_segments_after, 1, "merge should produce 1 segment");
        assert_eq!(num_docs_after, 400);
        Ok(())
    }
}
