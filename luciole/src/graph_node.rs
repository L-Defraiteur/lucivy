use crate::dag::Dag;
use crate::node::{Node, NodeContext, PortDef};
use crate::port::{PortType, PortValue};
use crate::runtime::{execute_dag, DagResult};

// ---------------------------------------------------------------------------
// GraphNode — a sub-DAG exposed as a single Node
// ---------------------------------------------------------------------------

/// Wraps a `Dag` as a `Node` for hierarchical composition.
///
/// Inputs are injected via `Dag::set_initial_input()` before execution.
/// Outputs are extracted via `DagResult::take_output()` after execution.
///
/// ```ignore
/// let mut inner = Dag::new();
/// inner.add_node("double", DoubleNode);
/// inner.add_node("add", AddNode { add: 10 });
/// inner.connect("double", "out", "add", "in").unwrap();
///
/// let graph = GraphNode::builder(inner)
///     .input("value", "double", "in", PortType::of::<i32>())
///     .output("result", "add", "out", PortType::of::<i32>())
///     .build();
/// ```
pub struct GraphNode {
    dag: Dag,
    input_map: Vec<PortMapping>,
    output_map: Vec<PortMapping>,
    last_result: Option<DagResult>,
}

#[derive(Clone)]
struct PortMapping {
    outer_name: String,
    inner_node: String,
    inner_port: String,
    port_type: PortType,
    required: bool,
}

impl GraphNode {
    pub fn builder(dag: Dag) -> GraphNodeBuilder {
        GraphNodeBuilder {
            dag,
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// Access the result of the last execution.
    pub fn last_result(&self) -> Option<&DagResult> {
        self.last_result.as_ref()
    }
}

impl Node for GraphNode {
    fn node_type(&self) -> &'static str { "graph" }

    fn inputs(&self) -> Vec<PortDef> {
        self.input_map.iter().map(|p| {
            if p.required {
                PortDef::required(
                    Box::leak(p.outer_name.clone().into_boxed_str()),
                    p.port_type,
                )
            } else {
                PortDef::optional(
                    Box::leak(p.outer_name.clone().into_boxed_str()),
                    p.port_type,
                )
            }
        }).collect()
    }

    fn outputs(&self) -> Vec<PortDef> {
        self.output_map.iter().map(|p| {
            PortDef::required(
                Box::leak(p.outer_name.clone().into_boxed_str()),
                p.port_type,
            )
        }).collect()
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // 1. Inject outer inputs into the sub-DAG as initial_inputs
        for input in &self.input_map {
            if let Some(value) = ctx.take_input(&input.outer_name) {
                self.dag.set_initial_input(&input.inner_node, &input.inner_port, value);
            }
        }

        // 2. Execute the sub-DAG
        let mut result = execute_dag(&mut self.dag, None)?;

        // 3. Extract outputs and forward to outer context
        for output in &self.output_map {
            if let Some(value) = result.outputs.remove(
                &(output.inner_node.clone(), output.inner_port.clone())
            ) {
                ctx.set_output(&output.outer_name, value);
            }
        }

        // 4. Forward inner metrics with prefix
        for (node_name, nr) in &result.node_results {
            for (key, value) in &nr.metrics {
                ctx.metric(&format!("{}.{}", node_name, key), *value);
            }
        }
        ctx.metric("inner_duration_ms", result.duration_ms as f64);
        ctx.metric("inner_nodes", result.node_results.len() as f64);

        self.last_result = Some(result);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GraphNodeBuilder
// ---------------------------------------------------------------------------

pub struct GraphNodeBuilder {
    dag: Dag,
    inputs: Vec<PortMapping>,
    outputs: Vec<PortMapping>,
}

impl GraphNodeBuilder {
    /// Map an outer input port to an inner node's input port.
    pub fn input(
        mut self,
        outer_name: &str,
        inner_node: &str,
        inner_port: &str,
        port_type: PortType,
    ) -> Self {
        self.inputs.push(PortMapping {
            outer_name: outer_name.to_string(),
            inner_node: inner_node.to_string(),
            inner_port: inner_port.to_string(),
            port_type,
            required: true,
        });
        self
    }

    /// Map an outer output port to an inner node's output port.
    pub fn output(
        mut self,
        outer_name: &str,
        inner_node: &str,
        inner_port: &str,
        port_type: PortType,
    ) -> Self {
        self.outputs.push(PortMapping {
            outer_name: outer_name.to_string(),
            inner_node: inner_node.to_string(),
            inner_port: inner_port.to_string(),
            port_type,
            required: true,
        });
        self
    }

    pub fn build(self) -> GraphNode {
        GraphNode {
            dag: self.dag,
            input_map: self.inputs,
            output_map: self.outputs,
            last_result: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Dag;
    use crate::node::NodeContext;
    use crate::port::{PortType, PortValue};
    use crate::runtime::execute_dag;

    struct DoubleNode;
    impl Node for DoubleNode {
        fn node_type(&self) -> &'static str { "double" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let v = *ctx.input("in").unwrap().downcast::<i32>().unwrap();
            ctx.set_output("out", PortValue::new(v * 2));
            ctx.metric("doubled", v as f64);
            Ok(())
        }
    }

    struct AddNode { add: i32 }
    impl Node for AddNode {
        fn node_type(&self) -> &'static str { "add" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let v = *ctx.input("in").unwrap().downcast::<i32>().unwrap();
            ctx.set_output("out", PortValue::new(v + self.add));
            Ok(())
        }
    }

    struct EmitNode { value: i32 }
    impl Node for EmitNode {
        fn node_type(&self) -> &'static str { "emit" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            ctx.set_output("out", PortValue::new(self.value));
            Ok(())
        }
    }

    struct SinkNode;
    impl Node for SinkNode {
        fn node_type(&self) -> &'static str { "sink" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let v = ctx.input("in").and_then(|v| v.downcast::<i32>().copied()).unwrap_or(0);
            ctx.metric("received", v as f64);
            Ok(())
        }
    }

    #[test]
    fn graph_node_basic() {
        // Inner DAG: double → add(10)
        let mut inner = Dag::new();
        inner.add_node("double", DoubleNode);
        inner.add_node("add", AddNode { add: 10 });
        inner.connect("double", "out", "add", "in").unwrap();

        let graph = GraphNode::builder(inner)
            .input("value", "double", "in", PortType::of::<i32>())
            .output("result", "add", "out", PortType::of::<i32>())
            .build();

        // Use in outer DAG
        let mut outer = Dag::new();
        outer.add_node("source", EmitNode { value: 5 });
        outer.add_node("compute", graph);
        outer.add_node("sink", SinkNode);

        outer.connect("source", "out", "compute", "value").unwrap();
        outer.connect("compute", "result", "sink", "in").unwrap();

        let result = execute_dag(&mut outer, None).unwrap();

        // 5 * 2 + 10 = 20
        let sink = result.get("sink").unwrap();
        assert_eq!(sink.metrics[0].1, 20.0);

        // Inner metrics forwarded with prefix
        let compute = result.get("compute").unwrap();
        assert!(compute.metrics.iter().any(|(k, _)| k == "double.doubled"));
    }

    #[test]
    fn graph_node_nested() {
        let mut inner = Dag::new();
        inner.add_node("d", DoubleNode);
        let g1 = GraphNode::builder(inner)
            .input("x", "d", "in", PortType::of::<i32>())
            .output("y", "d", "out", PortType::of::<i32>())
            .build();

        let mut outer = Dag::new();
        outer.add_node("emit", EmitNode { value: 7 });
        outer.add_node("g1", g1);
        outer.add_node("sink", SinkNode);
        outer.connect("emit", "out", "g1", "x").unwrap();
        outer.connect("g1", "y", "sink", "in").unwrap();

        let result = execute_dag(&mut outer, None).unwrap();
        let sink = result.get("sink").unwrap();
        assert_eq!(sink.metrics[0].1, 14.0); // 7 * 2
    }
}
