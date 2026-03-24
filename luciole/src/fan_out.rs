//! FanOutMerge — parallel fan-out with merge, as a DAG helper.
//!
//! Spawns N parallel worker nodes and a merge node that collects results.
//!
//! ```ignore
//! dag.fan_out_merge::<ResultType>(
//!     "prescan",                                    // name prefix
//!     4,                                            // parallelism
//!     |i| Box::new(PrescanNode::new(i)),           // node factory
//!     "output",                                     // worker output port
//!     |results| merge_results(results),             // merge function
//! )?;
//! // Creates: prescan_0..3 (workers) + prescan_merge (merger)
//! ```

use std::collections::HashMap;

use crate::node::{Node, NodeContext, PortDef};
use crate::port::{PortType, PortValue};
use crate::Dag;

/// Generic merge node: collects N typed inputs and applies a merge function.
pub struct MergeNode<T, F>
where
    T: Send + 'static,
    F: FnOnce(Vec<T>) -> T + Send,
{
    num_inputs: usize,
    merge_fn: Option<F>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T, F> MergeNode<T, F>
where
    T: Send + 'static,
    F: FnOnce(Vec<T>) -> T + Send,
{
    pub fn new(num_inputs: usize, merge_fn: F) -> Self {
        Self {
            num_inputs,
            merge_fn: Some(merge_fn),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T, F> Node for MergeNode<T, F>
where
    T: Send + Sync + 'static,
    F: FnOnce(Vec<T>) -> T + Send,
{
    fn node_type(&self) -> &'static str { "merge" }

    fn inputs(&self) -> Vec<PortDef> {
        (0..self.num_inputs)
            .map(|i| PortDef::optional(
                Box::leak(format!("in_{i}").into_boxed_str()),
                PortType::of::<T>(),
            ))
            .collect()
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("merged", PortType::of::<T>())]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let merge_fn = self.merge_fn.take()
            .ok_or("merge function already consumed")?;

        let mut items = Vec::with_capacity(self.num_inputs);
        for i in 0..self.num_inputs {
            let port = format!("in_{i}");
            if let Some(value) = ctx.take_input(&port) {
                if let Some(item) = value.take::<T>() {
                    items.push(item);
                }
            }
        }

        ctx.metric("inputs_received", items.len() as f64);
        let merged = merge_fn(items);
        ctx.set_output("merged", PortValue::new(merged));
        Ok(())
    }
}

/// Extension trait for Dag: adds fan_out_merge helper.
impl Dag {
    /// Create N parallel worker nodes + a merge node.
    ///
    /// Workers are named `{prefix}_0` .. `{prefix}_{N-1}`.
    /// Merge node is named `{prefix}_merge`.
    /// Workers' `output_port` is connected to merge's `in_0` .. `in_{N-1}`.
    ///
    /// Returns the merge node name for further connections.
    pub fn fan_out_merge<T: Send + Sync + 'static>(
        &mut self,
        prefix: &str,
        count: usize,
        node_factory: impl Fn(usize) -> Box<dyn Node>,
        output_port: &str,
        merge_fn: impl FnOnce(Vec<T>) -> T + Send + 'static,
    ) -> Result<String, String> {
        // Add worker nodes
        for i in 0..count {
            let name = format!("{prefix}_{i}");
            self.add_node_boxed(Box::leak(name.clone().into_boxed_str()), node_factory(i));
        }

        // Add merge node
        let merge_name = format!("{prefix}_merge");
        self.add_node(
            Box::leak(merge_name.clone().into_boxed_str()),
            MergeNode::<T, _>::new(count, merge_fn),
        );

        // Wire workers → merge
        for i in 0..count {
            let worker_name = format!("{prefix}_{i}");
            let merge_port = format!("in_{i}");
            self.connect(
                Box::leak(worker_name.into_boxed_str()), output_port,
                Box::leak(merge_name.clone().into_boxed_str()),
                Box::leak(merge_port.into_boxed_str()),
            )?;
        }

        Ok(merge_name)
    }
}
