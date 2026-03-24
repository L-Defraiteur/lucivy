//! GateNode — conditional pass/block in a DAG.
//!
//! Evaluates a condition. If true, triggers "pass" and forwards data.
//! If false, produces no output — downstream nodes are skipped.
//!
//! Different from SwitchNode: a Gate doesn't route to alternatives,
//! it simply blocks or passes through.
//!
//! ```ignore
//! dag.add_node("need_flush", GateNode::new(|| handle.has_uncommitted()));
//! dag.connect("drain", "done", "need_flush", "trigger")?;
//! dag.connect("need_flush", "pass", "flush", "trigger")?;
//! ```

use crate::node::{Node, NodeContext, PortDef};
use crate::port::PortType;

/// A node that passes through or blocks based on a condition.
pub struct GateNode<F: FnMut() -> bool + Send> {
    condition: F,
}

impl<F: FnMut() -> bool + Send> GateNode<F> {
    pub fn new(condition: F) -> Self {
        Self { condition }
    }
}

impl<F: FnMut() -> bool + Send> Node for GateNode<F> {
    fn node_type(&self) -> &'static str { "gate" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::trigger("trigger"),
            PortDef::optional("data", PortType::Any),
        ]
    }

    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::trigger("pass"),
            PortDef::optional("data_out", PortType::Any),
        ]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if (self.condition)() {
            ctx.trigger("pass");
            if let Some(data) = ctx.take_input("data") {
                ctx.set_output("data_out", data);
            }
            ctx.metric("gate", 1.0);
        } else {
            ctx.metric("gate", 0.0);
        }
        Ok(())
    }
}
