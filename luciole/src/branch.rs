//! BranchNode — conditional routing in a DAG.
//!
//! Evaluates a boolean condition and triggers exactly one of two outputs:
//! `"then"` if true, `"else"` if false.
//!
//! Nodes connected to the inactive branch are automatically skipped by the
//! runtime (their trigger input won't be satisfied).
//!
//! ```ignore
//! dag.add_node("check", BranchNode::new(|| some_condition));
//! dag.connect("upstream", "done", "check", "trigger")?;
//! dag.connect("check", "then", "heavy_work", "trigger")?;
//! dag.connect("check", "else", "lightweight", "trigger")?;
//! ```

use crate::node::{Node, NodeContext, PortDef};

/// A node that evaluates a condition and triggers one of two output paths.
pub struct BranchNode<F: FnMut() -> bool + Send> {
    condition: F,
}

impl<F: FnMut() -> bool + Send> BranchNode<F> {
    pub fn new(condition: F) -> Self {
        Self { condition }
    }
}

impl<F: FnMut() -> bool + Send> Node for BranchNode<F> {
    fn node_type(&self) -> &'static str { "branch" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::trigger("trigger")]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::trigger("then"),
            PortDef::trigger("else"),
        ]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if (self.condition)() {
            ctx.trigger("then");
            ctx.metric("branch", 1.0);
        } else {
            ctx.trigger("else");
            ctx.metric("branch", 0.0);
        }
        Ok(())
    }
}
