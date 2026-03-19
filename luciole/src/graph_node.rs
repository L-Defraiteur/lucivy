use std::collections::HashMap;

use crate::dag::Dag;
use crate::node::{Node, NodeContext, PortDef};
use crate::port::{PortType, PortValue};
use crate::runtime::{execute_dag, DagResult};

// ---------------------------------------------------------------------------
// GraphNode — a sub-DAG exposed as a single Node
// ---------------------------------------------------------------------------

/// Wraps a `Dag` as a `Node` for hierarchical composition.
///
/// Input ports: unconnected input ports of inner nodes are exposed.
/// Output ports: designated output ports are collected after execution.
///
/// ```ignore
/// // Build inner DAG
/// let mut inner = Dag::new();
/// inner.add_node("double", DoubleNode);
/// inner.add_node("add", AddNode { add: 10 });
/// inner.connect("double", "out", "add", "in").unwrap();
///
/// // Wrap as GraphNode: expose double.in as input, add.out as output
/// let graph = GraphNode::builder(inner)
///     .input("value", "double", "in")
///     .output("result", "add", "out")
///     .build();
///
/// // Use in outer DAG
/// let mut outer = Dag::new();
/// outer.add_node("source", EmitNode { value: 5 });
/// outer.add_node("compute", graph);
/// outer.connect("source", "out", "compute", "value").unwrap();
/// ```
pub struct GraphNode {
    dag: Dag,
    /// Maps outer input port → (inner_node, inner_port)
    input_map: Vec<GraphPort>,
    /// Maps outer output port → (inner_node, inner_port)
    output_map: Vec<GraphPort>,
    /// Last execution result (for metrics/logs passthrough)
    last_result: Option<DagResult>,
}

#[derive(Clone)]
struct GraphPort {
    /// Port name as seen from outside
    outer_name: String,
    /// Node name inside the sub-DAG
    inner_node: String,
    /// Port name on the inner node
    inner_port: String,
    /// Port type
    port_type: PortType,
    /// Is this port required?
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

    /// Access the result of the last execution (metrics, logs, timing per inner node).
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
        // 1. Inject outer inputs into the sub-DAG as initial port_data
        //    We do this by creating wrapper "inject" nodes that emit the values.
        //    Simpler: we use a pre-execute hook to set values.
        //    Simplest: create InjectNodes dynamically.

        // For each mapped input, take the value from ctx and we need to pass it
        // into the sub-DAG. The cleanest way: add temporary InjectNode for each input.
        // But modifying the DAG each time is ugly.
        //
        // Better: execute the sub-DAG and manually seed port_data before execution.
        // But execute_dag doesn't expose that.
        //
        // Pragmatic solution: wrap inputs in InjectNode at build time.
        // The InjectNode stores an Option<PortValue> that we set before execute.

        // Set inject values
        for input in &self.input_map {
            if let Some(value) = ctx.take_input(&input.outer_name) {
                // Find the inject node and set its value
                let inject_name = format!("__inject_{}", input.outer_name);
                let node = self.dag.node_mut_by_name(&inject_name)
                    .ok_or_else(|| format!("inject node '{}' not found", inject_name))?;
                if let Some(inject) = node.as_any_mut().downcast_mut::<InjectNode>() {
                    inject.value = Some(value);
                }
            }
        }

        // 2. Execute the sub-DAG
        let result = execute_dag(&mut self.dag, None)?;

        // 3. Collect designated outputs and forward metrics
        for output in &self.output_map {
            // The output was stored in port_data by the inner node.
            // But execute_dag consumed it... We need another approach.
            // The inner node's output is in the DAG's port_data which is local to execute_dag.
            //
            // Solution: use a CollectNode at the end that stores the value.
            let collect_name = format!("__collect_{}", output.outer_name);
            let node = self.dag.node_mut_by_name(&collect_name)
                .ok_or_else(|| format!("collect node '{}' not found", collect_name))?;
            if let Some(collect) = node.as_any_mut().downcast_mut::<CollectNode>() {
                if let Some(value) = collect.value.take() {
                    ctx.set_output(&output.outer_name, value);
                }
            }
        }

        // 4. Forward inner metrics and logs
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
    inputs: Vec<(String, String, String, PortType, bool)>,
    outputs: Vec<(String, String, String, PortType)>,
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
        self.inputs.push((
            outer_name.to_string(),
            inner_node.to_string(),
            inner_port.to_string(),
            port_type,
            true,
        ));
        self
    }

    /// Map an outer trigger input.
    pub fn trigger_input(mut self, outer_name: &str, inner_node: &str, inner_port: &str) -> Self {
        self.inputs.push((
            outer_name.to_string(),
            inner_node.to_string(),
            inner_port.to_string(),
            PortType::Trigger,
            true,
        ));
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
        self.outputs.push((
            outer_name.to_string(),
            inner_node.to_string(),
            inner_port.to_string(),
            port_type,
        ));
        self
    }

    /// Map an outer trigger output.
    pub fn trigger_output(mut self, outer_name: &str, inner_node: &str, inner_port: &str) -> Self {
        self.outputs.push((
            outer_name.to_string(),
            inner_node.to_string(),
            inner_port.to_string(),
            PortType::Trigger,
        ));
        self
    }

    /// Build the GraphNode. Adds inject/collect adapter nodes to the inner DAG.
    pub fn build(mut self) -> GraphNode {
        let mut input_map = Vec::new();
        let mut output_map = Vec::new();

        // Add InjectNode for each input
        for (outer_name, inner_node, inner_port, port_type, required) in self.inputs {
            let inject_name = format!("__inject_{}", outer_name);
            let inject = InjectNode::new(&outer_name, port_type);
            self.dag.add_node(
                Box::leak(inject_name.clone().into_boxed_str()),
                inject,
            );
            let _ = self.dag.connect(
                &inject_name, &outer_name,
                &inner_node, &inner_port,
            );
            input_map.push(GraphPort {
                outer_name: outer_name.clone(),
                inner_node,
                inner_port,
                port_type,
                required,
            });
        }

        // Add CollectNode for each output
        for (outer_name, inner_node, inner_port, port_type) in self.outputs {
            let collect_name = format!("__collect_{}", outer_name);
            let collect = CollectNode::new(&outer_name, port_type);
            self.dag.add_node(
                Box::leak(collect_name.clone().into_boxed_str()),
                collect,
            );
            let _ = self.dag.connect(
                &inner_node, &inner_port,
                &collect_name, &outer_name,
            );
            output_map.push(GraphPort {
                outer_name,
                inner_node,
                inner_port,
                port_type,
                required: true,
            });
        }

        GraphNode {
            dag: self.dag,
            input_map,
            output_map,
            last_result: None,
        }
    }
}

// ---------------------------------------------------------------------------
// InjectNode — injects external values into the sub-DAG
// ---------------------------------------------------------------------------

struct InjectNode {
    port_name: String,
    port_type: PortType,
    value: Option<PortValue>,
}

impl InjectNode {
    fn new(port_name: &str, port_type: PortType) -> Self {
        Self {
            port_name: port_name.to_string(),
            port_type,
            value: None,
        }
    }
}

impl Node for InjectNode {
    fn node_type(&self) -> &'static str { "inject" }

    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required(
            Box::leak(self.port_name.clone().into_boxed_str()),
            self.port_type,
        )]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if let Some(value) = self.value.take() {
            ctx.set_output(&self.port_name, value);
        }
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
}

// ---------------------------------------------------------------------------
// CollectNode — captures output from the sub-DAG
// ---------------------------------------------------------------------------

struct CollectNode {
    port_name: String,
    port_type: PortType,
    value: Option<PortValue>,
}

impl CollectNode {
    fn new(port_name: &str, port_type: PortType) -> Self {
        Self {
            port_name: port_name.to_string(),
            port_type,
            value: None,
        }
    }
}

impl Node for CollectNode {
    fn node_type(&self) -> &'static str { "collect" }

    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required(
            Box::leak(self.port_name.clone().into_boxed_str()),
            self.port_type,
        )]
    }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        if let Some(value) = ctx.take_input(&self.port_name) {
            self.value = Some(value);
        }
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
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

    struct SinkNode { received: Option<i32> }
    impl Node for SinkNode {
        fn node_type(&self) -> &'static str { "sink" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            self.received = ctx.input("in").and_then(|v| v.downcast::<i32>().copied());
            ctx.metric("received", self.received.unwrap_or(0) as f64);
            Ok(())
        }
    }

    #[test]
    fn graph_node_basic() {
        // Inner DAG: double → add(10)
        // Input: "value" → double.in
        // Output: "result" ← add.out
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
        outer.add_node("sink", SinkNode { received: None });

        outer.connect("source", "out", "compute", "value").unwrap();
        outer.connect("compute", "result", "sink", "in").unwrap();

        let result = execute_dag(&mut outer, None).unwrap();

        // 5 * 2 + 10 = 20
        let sink = result.get("sink").unwrap();
        assert_eq!(sink.metrics[0].1, 20.0);

        // Inner metrics forwarded with prefix
        let compute = result.get("compute").unwrap();
        assert!(compute.metrics.iter().any(|(k, _)| k == "double.doubled"));
        assert!(compute.metrics.iter().any(|(k, _)| k == "inner_nodes"));
    }

    #[test]
    fn graph_node_nested() {
        // Inner: double
        let mut inner1 = Dag::new();
        inner1.add_node("d", DoubleNode);
        let g1 = GraphNode::builder(inner1)
            .input("x", "d", "in", PortType::of::<i32>())
            .output("y", "d", "out", PortType::of::<i32>())
            .build();

        // Outer: emit → g1 → sink
        let mut outer = Dag::new();
        outer.add_node("emit", EmitNode { value: 7 });
        outer.add_node("g1", g1);
        outer.add_node("sink", SinkNode { received: None });
        outer.connect("emit", "out", "g1", "x").unwrap();
        outer.connect("g1", "y", "sink", "in").unwrap();

        let result = execute_dag(&mut outer, None).unwrap();
        let sink = result.get("sink").unwrap();
        assert_eq!(sink.metrics[0].1, 14.0); // 7 * 2
    }
}
