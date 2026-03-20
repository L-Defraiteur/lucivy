//! DAG-based commit orchestration for lucivy.
//!
//! Every step of the commit is a DAG node with full observability.
//! The structure guarantees correct ordering by construction:
//!
//! ```text
//! prepare ──┬── merge_0 ──┐
//!           ├── merge_1 ──┼── finalize ── save_metas ── gc ── reload
//!           └── merge_2 ──┘
//! ```
//!
//! - prepare: collect candidates, start_merge (lock segments), purge deletes
//! - merge_N: parallel merges (PollNode, cooperative yielding)
//! - finalize: end_merge for each result, commit segment manager
//! - save_metas: write meta.json atomically
//! - gc: garbage collect orphan files (safe — no merges in flight)
//! - reload: reader picks up new metas

use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::indexer::events::IndexEvent;
use crate::indexer::merge_operation::MergeOperation;
use crate::indexer::segment_entry::SegmentEntry;
use crate::indexer::segment_updater::SegmentUpdaterShared;
use crate::Opstamp;

// ---------------------------------------------------------------------------
// Types flowing between nodes
// ---------------------------------------------------------------------------

/// Output of PrepareNode: everything the merges need.
pub(crate) struct PrepareOutput {
    pub segment_entries_per_merge: Vec<(MergeOperation, Vec<SegmentEntry>)>,
}

/// Result of a single merge.
pub(crate) struct MergeResult {
    pub merge_op: MergeOperation,
    pub segment_entry: Option<SegmentEntry>,
    pub duration_ms: u64,
    pub docs_merged: u32,
}

// ---------------------------------------------------------------------------
// PrepareNode — collect candidates, start merges, purge deletes
// ---------------------------------------------------------------------------

pub(crate) struct PrepareNode {
    shared: Arc<SegmentUpdaterShared>,
    merge_ops: Vec<MergeOperation>,
    opstamp: Opstamp,
}

impl PrepareNode {
    pub fn new(
        shared: Arc<SegmentUpdaterShared>,
        merge_ops: Vec<MergeOperation>,
        opstamp: Opstamp,
    ) -> Self {
        Self { shared, merge_ops, opstamp }
    }
}

impl Node for PrepareNode {
    fn node_type(&self) -> &'static str { "prepare" }

    fn outputs(&self) -> Vec<PortDef> {
        let mut ports: Vec<PortDef> = Vec::new();
        for i in 0..self.merge_ops.len() {
            ports.push(PortDef::required(
                Box::leak(format!("merge_{}", i).into_boxed_str()),
                PortType::of::<(MergeOperation, Vec<SegmentEntry>)>(),
            ));
        }
        ports.push(PortDef::trigger("done"));
        ports
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // 1. Purge deletes (advance delete cursors to current opstamp)
        let segment_entries = self.shared.purge_deletes(self.opstamp)
            .map_err(|e| format!("purge_deletes: {e}"))?;
        ctx.metric("purged_segments", segment_entries.len() as f64);

        // 2. Commit segment manager (swap uncommitted → committed)
        self.shared.segment_manager.commit(segment_entries);

        // 3. Start each merge (lock segments in the manager)
        for (i, op) in self.merge_ops.iter().enumerate() {
            let entries = self.shared.segment_manager
                .start_merge(op.segment_ids())
                .map_err(|e| format!("start_merge[{}]: {e}", i))?;

            self.shared.event_bus.emit(IndexEvent::MergeStarted {
                segment_ids: op.segment_ids().to_vec(),
                target_opstamp: op.target_opstamp(),
            });

            ctx.info(&format!(
                "prepared merge_{}: {} segments, {} docs",
                i, entries.len(),
                entries.iter().map(|e| e.meta().num_docs()).sum::<u32>(),
            ));

            // Clone the op since we need to pass it to the merge node
            let op_clone = MergeOperation::new(
                op.target_opstamp(),
                op.segment_ids().to_vec(),
            );
            ctx.set_output(
                &format!("merge_{}", i),
                PortValue::new((op_clone, entries)),
            );
        }

        ctx.trigger("done");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergeNode — executes a merge DAG (postings ∥ store ∥ fast_fields → sfx)
// ---------------------------------------------------------------------------

pub(crate) struct MergeNode {
    shared: Arc<SegmentUpdaterShared>,
}

impl MergeNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>) -> Self {
        Self { shared }
    }
}

impl Node for MergeNode {
    fn node_type(&self) -> &'static str { "merge" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("input", PortType::of::<(MergeOperation, Vec<SegmentEntry>)>())]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("result", PortType::of::<MergeResult>())]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let (op, entries) = ctx.take_input("input")
            .ok_or("missing input")?
            .take::<(MergeOperation, Vec<SegmentEntry>)>()
            .ok_or("wrong input type")?;

        let start = std::time::Instant::now();
        let num_segments = op.segment_ids().len();

        // Build and execute the merge DAG
        let merge_dag = super::merge_dag::build_merge_dag(
            &self.shared.index, entries, op.target_opstamp(),
        ).map_err(|e| format!("build_merge_dag: {e}"))?;

        let (segment_entry, docs_merged) = match merge_dag {
            Some(mut dag) => {
                let mut dag_result = luciole::execute_dag(&mut dag, None)
                    .map_err(|e| format!("merge DAG: {e}"))?;

                if crate::diag::is_verbose() {
                    eprintln!("    merge ({} segments) — {}", num_segments, dag_result.display_summary());
                }

                // Extract the SegmentEntry from the close node's output
                let entry = dag_result.take_output::<SegmentEntry>("close", "entry");
                let docs = entry.as_ref().map(|e| e.meta().num_docs()).unwrap_or(0);
                (entry, docs)
            }
            None => {
                ctx.info("empty merge (0 docs)");
                (None, 0)
            }
        };

        let duration = start.elapsed();
        ctx.metric("duration_ms", duration.as_millis() as f64);
        ctx.metric("docs_merged", docs_merged as f64);

        self.shared.event_bus.emit(IndexEvent::MergeCompleted {
            segment_ids: op.segment_ids().to_vec(),
            duration,
            result_num_docs: docs_merged,
        });

        ctx.set_output("result", PortValue::new(MergeResult {
            merge_op: op,
            segment_entry,
            duration_ms: duration.as_millis() as u64,
            docs_merged,
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FinalizeNode — end_merge for each result, commit manager
// ---------------------------------------------------------------------------

pub(crate) struct FinalizeNode {
    shared: Arc<SegmentUpdaterShared>,
    num_merges: usize,
}

impl FinalizeNode {
    pub fn new(shared: Arc<SegmentUpdaterShared>, num_merges: usize) -> Self {
        Self { shared, num_merges }
    }
}

impl Node for FinalizeNode {
    fn node_type(&self) -> &'static str { "finalize" }

    fn inputs(&self) -> Vec<PortDef> {
        let mut ports: Vec<PortDef> = (0..self.num_merges)
            .map(|i| PortDef::required(
                Box::leak(format!("result_{}", i).into_boxed_str()),
                PortType::of::<MergeResult>(),
            ))
            .collect();
        ports.push(PortDef::optional("trigger", PortType::Trigger));
        ports
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("done")]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut total_docs = 0u32;

        for i in 0..self.num_merges {
            let port_name = format!("result_{}", i);
            if let Some(value) = ctx.take_input(&port_name) {
                if let Some(result) = value.take::<MergeResult>() {
                    total_docs += result.docs_merged;

                    // Advance deletes on merged segment if needed
                    let after_entry = if let Some(mut entry) = result.segment_entry {
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
        ctx.metric("merges", self.num_merges as f64);
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
        ctx.info(&format!("saved at opstamp {}", self.opstamp));
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

pub(crate) struct ReloadNode;

impl Node for ReloadNode {
    fn node_type(&self) -> &'static str { "reload" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        ctx.info("reload complete");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_commit_dag — factory function
// ---------------------------------------------------------------------------

/// Build the commit DAG. Every step is a node with full observability.
///
/// ```text
/// prepare ──┬── merge_0 ──┐
///           ├── merge_1 ──┼── finalize ── save_metas ── gc ── reload
///           └── merge_2 ──┘
/// ```
///
/// - prepare: purge_deletes + commit + start_merge (locks segments)
/// - merge_N: parallel merges via PollNode (cooperative)
/// - finalize: end_merge for each, advance deletes
/// - save_metas → gc → reload: sequential cleanup
pub(crate) fn build_commit_dag(
    shared: Arc<SegmentUpdaterShared>,
    merge_ops: Vec<MergeOperation>,
    opstamp: Opstamp,
    payload: Option<String>,
) -> Result<Dag, String> {
    let mut dag = Dag::new();
    let num_merges = merge_ops.len();

    // Prepare: purge + commit + start all merges
    dag.add_node("prepare", PrepareNode::new(shared.clone(), merge_ops, opstamp));

    if num_merges > 0 {
        // Parallel merge nodes
        for i in 0..num_merges {
            dag.add_node(&format!("merge_{}", i), MergeNode::new(shared.clone()));
            dag.connect(
                "prepare", &format!("merge_{}", i),
                &format!("merge_{}", i), "input",
            )?;
        }

        // Finalize: collect all results
        dag.add_node("finalize", FinalizeNode::new(shared.clone(), num_merges));
        for i in 0..num_merges {
            dag.connect(
                &format!("merge_{}", i), "result",
                "finalize", &format!("result_{}", i),
            )?;
        }
        // Also wait for prepare to complete
        dag.connect("prepare", "done", "finalize", "trigger")?;

        // Save depends on finalize
        dag.add_node("save", SaveMetasNode::new(shared.clone(), opstamp, payload));
        dag.connect("finalize", "done", "save", "trigger")?;
    } else {
        // No merges — save directly after prepare
        dag.add_node("save", SaveMetasNode::new(shared.clone(), opstamp, payload));
        dag.connect("prepare", "done", "save", "trigger")?;
    }

    // GC → Reload
    dag.add_node("gc", GCNode::new(shared.clone()));
    dag.add_node("reload", ReloadNode);
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

    #[test]
    fn test_commit_dag_merges_all_segments() -> crate::Result<()> {
        let directory = RamDirectory::create();
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_u64_field("num", INDEXED);
        let schema = schema_builder.build();
        let index = Index::create(directory.clone(), schema, crate::IndexSettings::default())?;

        let mut writer: IndexWriter = index.writer_with_num_threads(1, 32_000_000)?;
        writer.set_merge_policy(Box::new(MergeWheneverPossible));

        // Subscribe for observability
        let dag_events = luciole::subscribe_dag_events();

        // Create 4 segments
        for seg in 0..4 {
            for i in 0u64..100u64 {
                writer.add_document(doc!(field => i))?;
            }
            writer.commit()?;
        }

        // Wait for all merges (triggers full DAG commit)
        writer.wait_merging_threads()?;

        // Collect DAG events
        let mut events = Vec::new();
        while let Some(evt) = dag_events.try_recv() {
            events.push(evt);
        }

        let dag_completions: Vec<_> = events.iter()
            .filter_map(|e| match e {
                luciole::DagEvent::DagCompleted { total_ms, node_count } =>
                    Some((*total_ms, *node_count)),
                _ => None,
            })
            .collect();
        eprintln!("DAG executions: {:?}", dag_completions);

        // Verify: 1 segment, 400 docs
        let reader = index.reader()?;
        let num_segments = reader.searcher().segment_readers().len();
        let num_docs = reader.searcher().num_docs();
        eprintln!("segments={}, docs={}", num_segments, num_docs);

        assert_eq!(num_segments, 1, "all segments should be merged into 1");
        assert_eq!(num_docs, 400);
        Ok(())
    }

    #[test]
    fn test_commit_dag_with_deletes() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let text_field = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());

        let mut writer = index.writer_for_tests()?;
        writer.set_merge_policy(Box::new(MergeWheneverPossible));

        // 200 docs: 100 "a" + 100 "b"
        for _ in 0..100 {
            writer.add_document(doc!(text_field=>"a"))?;
            writer.add_document(doc!(text_field=>"b"))?;
        }
        writer.commit()?;

        // 200 more: 100 "c" + 100 "d"
        for _ in 0..100 {
            writer.add_document(doc!(text_field=>"c"))?;
            writer.add_document(doc!(text_field=>"d"))?;
        }
        writer.commit()?;

        // 2 more
        writer.add_document(doc!(text_field=>"e"))?;
        writer.add_document(doc!(text_field=>"f"))?;
        writer.commit()?;

        // Delete all "a" (100 docs)
        let term = crate::Term::from_field_text(text_field, "a");
        writer.delete_term(term);
        writer.commit()?;

        // Wait for merges
        writer.wait_merging_threads()?;

        let reader = index.reader()?;
        let num_segments = reader.searcher().segment_readers().len();
        let num_docs = reader.searcher().num_docs();
        eprintln!("segments={}, docs={}", num_segments, num_docs);

        assert_eq!(num_segments, 1);
        assert_eq!(num_docs, 302); // 402 - 100 deleted "a"
        Ok(())
    }

    #[test]
    fn test_commit_dag_no_merges() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_u64_field("num", INDEXED);
        let index = Index::create_in_ram(schema_builder.build());

        let mut writer = index.writer_for_tests()?;
        // Default merge policy — single commit, no merge needed

        for i in 0u64..10 {
            writer.add_document(doc!(field => i))?;
        }
        writer.commit()?;

        let reader = index.reader()?;
        assert_eq!(reader.searcher().num_docs(), 10);
        Ok(())
    }
}
