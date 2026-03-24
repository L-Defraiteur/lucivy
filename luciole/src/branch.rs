//! SwitchNode — N-way conditional routing in a DAG.
//!
//! Evaluates a selector function and triggers exactly one output among N.
//! Nodes connected to inactive branches are automatically skipped by the
//! runtime (their trigger input won't be satisfied).
//!
//! ```ignore
//! // N-way switch
//! dag.add_node("route", SwitchNode::new(
//!     vec!["fast", "prescan", "distributed"],
//!     move || match mode { Fast => 0, Prescan => 1, Distributed => 2 },
//! ));
//!
//! // 2-way branch (convenience)
//! dag.add_node("check", BranchNode::new(|| some_condition));
//! dag.connect("check", "then", "heavy_work", "trigger")?;
//! dag.connect("check", "else", "lightweight", "trigger")?;
//! ```

use crate::node::{Node, NodeContext, PortDef};

/// A node that evaluates a selector and triggers one of N output paths.
pub struct SwitchNode<F: FnMut() -> usize + Send> {
    outputs: Vec<&'static str>,
    selector: F,
}

impl<F: FnMut() -> usize + Send> SwitchNode<F> {
    /// Create a switch with named outputs.
    /// The selector returns the index of the output to trigger.
    pub fn new(outputs: Vec<&'static str>, selector: F) -> Self {
        Self { outputs, selector }
    }
}

impl<F: FnMut() -> usize + Send> Node for SwitchNode<F> {
    fn node_type(&self) -> &'static str { "switch" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }

    fn outputs(&self) -> Vec<PortDef> {
        self.outputs.iter().map(|name| PortDef::trigger(name)).collect()
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let idx = (self.selector)();
        if idx < self.outputs.len() {
            ctx.trigger(self.outputs[idx]);
            ctx.metric("switch_index", idx as f64);
        } else {
            return Err(format!(
                "switch selector returned {} but only {} outputs defined",
                idx, self.outputs.len()
            ));
        }
        Ok(())
    }
}

/// Convenience: 2-way branch (if/else). Triggers "then" or "else".
pub fn BranchNode<F: FnMut() -> bool + Send + 'static>(
    mut condition: F,
) -> SwitchNode<impl FnMut() -> usize + Send> {
    SwitchNode::new(
        vec!["then", "else"],
        move || if condition() { 0 } else { 1 },
    )
}
