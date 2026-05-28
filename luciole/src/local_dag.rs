//! LocalDag — single-thread DAG execution with borrowed services.
//!
//! Like `Dag` + `execute_sequential`, but without `Send`, `Sync`, or `'static`
//! constraints. Nodes borrow from a service reference `&S` passed at execution.
//!
//! Use cases:
//! - Query-time pipelines with borrowed index data
//! - Tests (deterministic, no scheduling)
//! - WASM single-thread targets
//! - Any DAG where the data is borrowed, not owned

use std::any::Any;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::dag::DagEdge;
use crate::node::{LogLevel, PortDef};
use crate::port::PortType;
use crate::runtime::{DagResult, NodeResult};

// ---------------------------------------------------------------------------
// LocalPortValue — Rc<dyn Any>, no Send, fan-out via Rc::clone
// ---------------------------------------------------------------------------

/// Runtime value flowing through a LocalDag edge.
///
/// Uses `Rc` (not Arc) for cheap fan-out without Send/Sync constraints.
/// Same semantics as `PortValue` but single-thread only.
#[derive(Clone)]
pub enum LocalPortValue {
    Data(Rc<dyn Any>),
    Trigger,
}

impl LocalPortValue {
    pub fn new<T: 'static>(data: T) -> Self {
        LocalPortValue::Data(Rc::new(data))
    }

    pub fn downcast<T: 'static>(&self) -> Option<&T> {
        match self {
            LocalPortValue::Data(d) => d.downcast_ref(),
            _ => None,
        }
    }

    pub fn take<T: 'static>(self) -> Option<T> {
        match self {
            LocalPortValue::Data(d) => {
                // Rc::downcast: Rc<dyn Any> -> Rc<T> (stable since 1.29)
                let typed: Rc<T> = d.downcast::<T>().ok()?;
                match Rc::try_unwrap(typed) {
                    Ok(val) => Some(val),
                    Err(rc) => panic!(
                        "LocalPortValue::take() failed: {} outstanding references to {}. \
                         Use downcast() for fan-out read-only access.",
                        Rc::strong_count(&rc),
                        std::any::type_name::<T>(),
                    ),
                }
            }
            _ => None,
        }
    }

    pub fn is_trigger(&self) -> bool {
        matches!(self, LocalPortValue::Trigger)
    }
}

// ---------------------------------------------------------------------------
// LocalNodeCtx — context passed to each node during execute
// ---------------------------------------------------------------------------

/// Context for a LocalNode during execution.
pub struct LocalNodeCtx {
    inputs: HashMap<String, LocalPortValue>,
    outputs: HashMap<String, LocalPortValue>,
    metrics: Vec<(String, f64)>,
    logs: Vec<(LogLevel, String)>,
    /// Per-output-port annotations (JSON or human-readable summaries).
    /// Written by the node via `annotate_output()`, collected by the DAG runner.
    annotations: HashMap<String, String>,
    /// Whether explain mode is active (nodes should annotate their outputs).
    explain: bool,
}

impl LocalNodeCtx {
    fn new(inputs: HashMap<String, LocalPortValue>, explain: bool) -> Self {
        Self {
            inputs,
            outputs: HashMap::new(),
            metrics: Vec::new(),
            logs: Vec::new(),
            annotations: HashMap::new(),
            explain,
        }
    }

    // -- inputs --

    pub fn input<T: 'static>(&self, port: &str) -> Option<&T> {
        self.inputs.get(port)?.downcast()
    }

    pub fn take_input<T: 'static>(&mut self, port: &str) -> Option<T> {
        self.inputs.remove(port)?.take()
    }

    pub fn has_trigger(&self, port: &str) -> bool {
        self.inputs.get(port).map(|v| v.is_trigger()).unwrap_or(false)
    }

    // -- outputs --

    pub fn set_output<T: 'static>(&mut self, port: &str, value: T) {
        self.outputs.insert(port.to_string(), LocalPortValue::new(value));
    }

    pub fn trigger(&mut self, port: &str) {
        self.outputs.insert(port.to_string(), LocalPortValue::Trigger);
    }

    // -- observability --

    pub fn metric(&mut self, key: &str, value: f64) {
        self.metrics.push((key.to_string(), value));
    }

    pub fn info(&mut self, msg: &str) {
        self.logs.push((LogLevel::Info, msg.to_string()));
    }

    pub fn debug(&mut self, msg: &str) {
        self.logs.push((LogLevel::Debug, msg.to_string()));
    }

    pub fn warn(&mut self, msg: &str) {
        self.logs.push((LogLevel::Warn, msg.to_string()));
    }

    // -- edge annotations --

    /// Whether explain mode is active. Nodes should check this before
    /// doing expensive annotation work.
    pub fn explain(&self) -> bool {
        self.explain
    }

    /// Annotate an output port with a summary (JSON string, human-readable, etc.).
    /// Only meaningful when `explain()` is true. The DAG runner collects these
    /// and attaches them to the corresponding edge in the result.
    pub fn annotate_output(&mut self, port: &str, summary: String) {
        self.annotations.insert(port.to_string(), summary);
    }

    // -- internals --

    fn take_outputs(&mut self) -> HashMap<String, LocalPortValue> {
        std::mem::take(&mut self.outputs)
    }

    fn take_annotations(&mut self) -> HashMap<String, String> {
        std::mem::take(&mut self.annotations)
    }
}

// ---------------------------------------------------------------------------
// EdgeAnnotations — explain data collected during execution
// ---------------------------------------------------------------------------

/// A single edge annotation: the data summary that flowed on an edge.
#[derive(Debug, Clone)]
pub struct EdgeAnnotation {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
    /// Summary of the data that flowed on this edge (JSON, text, etc.)
    pub data: String,
}

/// Collection of edge annotations from an explained execution.
#[derive(Debug, Clone)]
pub struct EdgeAnnotations {
    pub entries: Vec<EdgeAnnotation>,
}

impl EdgeAnnotations {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Get annotation for a specific edge.
    pub fn get(&self, from_node: &str, from_port: &str) -> Option<&str> {
        self.entries.iter()
            .find(|e| e.from_node == from_node && e.from_port == from_port)
            .map(|e| e.data.as_str())
    }

    /// Dump all annotations as a JSON-like string.
    pub fn dump_json(&self) -> String {
        let mut out = String::from("{\n");
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str(&format!(
                "  \"{}.{} -> {}.{}\": {}{}",
                e.from_node, e.from_port, e.to_node, e.to_port,
                e.data,
                if i + 1 < self.entries.len() { ",\n" } else { "\n" },
            ));
        }
        out.push('}');
        out
    }
}

// ---------------------------------------------------------------------------
// LocalNode<S> — a node that borrows from service &S
// ---------------------------------------------------------------------------

/// A synchronous DAG node that borrows from a shared service `&S`.
///
/// Unlike `Node`, does not require `Send` or `'static`. The service
/// reference is passed at execution time, not stored in a registry.
pub trait LocalNode<S> {
    /// Human-readable type name for this node.
    fn node_type(&self) -> &'static str;

    /// Input port definitions.
    fn inputs(&self) -> Vec<PortDef> { vec![] }

    /// Output port definitions.
    fn outputs(&self) -> Vec<PortDef> { vec![] }

    /// Execute this node. Read inputs from `ctx`, write outputs to `ctx`.
    /// Access shared index data via `services`.
    fn execute(&mut self, services: &S, ctx: &mut LocalNodeCtx) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// LocalDag<S> — the DAG itself
// ---------------------------------------------------------------------------

struct LocalNodeEntry<S> {
    name: String,
    node: Box<dyn LocalNode<S>>,
}

/// A DAG of `LocalNode<S>` nodes, executed sequentially on the current thread.
///
/// Reuses `DagEdge` for topology and produces the same `DagResult` as
/// `execute_dag` / `execute_sequential`, so the explain is unified.
pub struct LocalDag<S> {
    nodes: Vec<LocalNodeEntry<S>>,
    edges: Vec<DagEdge>,
}

impl<S> LocalDag<S> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn add_node(&mut self, name: &str, node: impl LocalNode<S> + 'static) {
        self.nodes.push(LocalNodeEntry {
            name: name.to_string(),
            node: Box::new(node),
        });
    }

    pub fn connect(
        &mut self,
        from_node: &str,
        from_port: &str,
        to_node: &str,
        to_port: &str,
    ) -> Result<(), String> {
        // Validate nodes exist
        let from_idx = self.node_index(from_node)
            .ok_or_else(|| format!("node '{}' not found", from_node))?;
        let to_idx = self.node_index(to_node)
            .ok_or_else(|| format!("node '{}' not found", to_node))?;

        // Validate ports exist and types match
        let from_type = self.nodes[from_idx].node.outputs().iter()
            .find(|p| p.name == from_port)
            .map(|p| p.port_type)
            .ok_or_else(|| format!("output port '{}' not found on '{}'", from_port, from_node))?;
        let to_type = self.nodes[to_idx].node.inputs().iter()
            .find(|p| p.name == to_port)
            .map(|p| p.port_type)
            .ok_or_else(|| format!("input port '{}' not found on '{}'", to_port, to_node))?;

        if !from_type.compatible_with(&to_type) {
            return Err(format!(
                "port type mismatch: {}.{} ({}) -> {}.{} ({})",
                from_node, from_port, from_type, to_node, to_port, to_type
            ));
        }

        self.edges.push(DagEdge {
            from_node: from_node.to_string(),
            from_port: from_port.to_string(),
            to_node: to_node.to_string(),
            to_port: to_port.to_string(),
        });
        Ok(())
    }

    pub fn edges(&self) -> &[DagEdge] {
        &self.edges
    }

    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    /// Execute the DAG sequentially, passing `services` to every node.
    pub fn execute(&mut self, services: &S) -> Result<DagResult, String> {
        let (_, result, _) = self.execute_inner(services, false)?;
        Ok(result)
    }

    /// Execute and extract a typed output from a specific node port.
    pub fn execute_and_take<T: 'static>(
        &mut self,
        services: &S,
        node: &str,
        port: &str,
    ) -> Result<(T, DagResult), String> {
        let (mut port_data, result, _) = self.execute_inner(services, false)?;

        let key = (node.to_string(), port.to_string());
        let value = port_data.remove(&key)
            .ok_or_else(|| format!("output '{}.{}' not found", node, port))?
            .take::<T>()
            .ok_or_else(|| format!("type mismatch for '{}.{}'", node, port))?;

        Ok((value, result))
    }

    /// Execute with explain mode: nodes annotate their outputs and
    /// edge annotations are collected in the result.
    pub fn execute_and_take_explained<T: 'static>(
        &mut self,
        services: &S,
        node: &str,
        port: &str,
    ) -> Result<(T, DagResult, EdgeAnnotations), String> {
        let (mut port_data, result, annotations) = self.execute_inner(services, true)?;

        let key = (node.to_string(), port.to_string());
        let value = port_data.remove(&key)
            .ok_or_else(|| format!("output '{}.{}' not found", node, port))?
            .take::<T>()
            .ok_or_else(|| format!("type mismatch for '{}.{}'", node, port))?;

        Ok((value, result, annotations))
    }

    fn execute_inner(
        &mut self,
        services: &S,
        explain: bool,
    ) -> Result<(HashMap<(String, String), LocalPortValue>, DagResult, EdgeAnnotations), String> {
        let dag_start = Instant::now();
        let levels = self.topological_levels()?;

        // Pre-compute consumer counts for fan-out (same as runtime.rs)
        let mut consumer_counts: HashMap<(String, String), usize> = HashMap::new();
        for edge in &self.edges {
            *consumer_counts
                .entry((edge.from_node.clone(), edge.from_port.clone()))
                .or_insert(0) += 1;
        }

        let mut port_data: HashMap<(String, String), LocalPortValue> = HashMap::new();
        let mut results: Vec<(String, NodeResult)> = Vec::with_capacity(self.nodes.len());
        // (node_name, port_name) -> annotation string
        let mut port_annotations: HashMap<(String, String), String> = HashMap::new();

        for level in &levels {
            for &node_idx in level {
                let node_name = self.nodes[node_idx].name.clone();
                let node_start = Instant::now();

                let inputs = collect_local_inputs(
                    &node_name, &self.edges, &mut port_data, &mut consumer_counts,
                );
                let mut ctx = LocalNodeCtx::new(inputs, explain);

                self.nodes[node_idx].node.execute(services, &mut ctx)?;

                let duration_ms = node_start.elapsed().as_millis() as u64;
                let metrics = ctx.metrics.clone();
                let logs = ctx.logs.clone();

                // Collect output annotations (explain mode)
                if explain {
                    for (port_name, annotation) in ctx.take_annotations() {
                        port_annotations.insert(
                            (node_name.clone(), port_name), annotation,
                        );
                    }
                }

                for (port_name, value) in ctx.take_outputs() {
                    port_data.insert((node_name.clone(), port_name), value);
                }

                results.push((node_name, NodeResult { duration_ms, metrics, logs }));
            }
        }

        let total_ms = dag_start.elapsed().as_millis() as u64;
        let dag_result = DagResult {
            duration_ms: total_ms,
            node_results: results,
            outputs: HashMap::new(),
        };

        // Map port annotations to edges + leaf outputs
        let mut edge_annotations = EdgeAnnotations::new();
        if explain {
            // Track which (node, port) have edges
            let mut has_edge: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

            for edge in &self.edges {
                let key = (edge.from_node.clone(), edge.from_port.clone());
                has_edge.insert(key.clone());
                if let Some(annotation) = port_annotations.get(&key) {
                    edge_annotations.entries.push(EdgeAnnotation {
                        from_node: edge.from_node.clone(),
                        from_port: edge.from_port.clone(),
                        to_node: edge.to_node.clone(),
                        to_port: edge.to_port.clone(),
                        data: annotation.clone(),
                    });
                }
            }

            // Leaf outputs (no outgoing edge) — still useful for explain
            for (key, annotation) in &port_annotations {
                if !has_edge.contains(key) {
                    edge_annotations.entries.push(EdgeAnnotation {
                        from_node: key.0.clone(),
                        from_port: key.1.clone(),
                        to_node: "(output)".to_string(),
                        to_port: key.1.clone(),
                        data: annotation.clone(),
                    });
                }
            }
        }

        Ok((port_data, dag_result, edge_annotations))
    }

    // -- internal --

    fn node_index(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    /// Kahn's algorithm — same as Dag::topological_levels().
    fn topological_levels(&self) -> Result<Vec<Vec<usize>>, String> {
        let n = self.nodes.len();
        let name_to_idx: HashMap<&str, usize> = self.nodes.iter()
            .enumerate()
            .map(|(i, entry)| (entry.name.as_str(), i))
            .collect();

        let mut in_degree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

        for edge in &self.edges {
            let from = *name_to_idx.get(edge.from_node.as_str())
                .ok_or_else(|| format!("edge references unknown node '{}'", edge.from_node))?;
            let to = *name_to_idx.get(edge.to_node.as_str())
                .ok_or_else(|| format!("edge references unknown node '{}'", edge.to_node))?;
            if !dependents[from].contains(&to) {
                dependents[from].push(to);
                in_degree[to] += 1;
            }
        }

        let mut levels = Vec::new();
        let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
        let mut visited = 0;

        while !queue.is_empty() {
            let level = std::mem::take(&mut queue);
            for &node_idx in &level {
                visited += 1;
                for &dep in &dependents[node_idx] {
                    in_degree[dep] -= 1;
                    if in_degree[dep] == 0 {
                        queue.push(dep);
                    }
                }
            }
            levels.push(level);
        }

        if visited != n {
            return Err("cycle detected in DAG".to_string());
        }
        Ok(levels)
    }

}

/// Collect inputs for a node from upstream outputs, with fan-out support.
/// Last consumer takes ownership (remove), earlier consumers clone (Rc::clone).
fn collect_local_inputs(
    node_name: &str,
    edges: &[DagEdge],
    port_data: &mut HashMap<(String, String), LocalPortValue>,
    consumer_counts: &mut HashMap<(String, String), usize>,
) -> HashMap<String, LocalPortValue> {
    let mut inputs = HashMap::new();
    for edge in edges {
        if edge.to_node == node_name {
            let key = (edge.from_node.clone(), edge.from_port.clone());
            if let Some(count) = consumer_counts.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    // Last consumer: take ownership
                    if let Some(value) = port_data.remove(&key) {
                        inputs.insert(edge.to_port.clone(), value);
                    }
                } else {
                    // Fan-out: clone (Rc::clone, cheap)
                    if let Some(value) = port_data.get(&key) {
                        inputs.insert(edge.to_port.clone(), value.clone());
                    }
                }
            }
        }
    }
    inputs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct EmitNode { value: i32 }
    impl LocalNode<()> for EmitNode {
        fn node_type(&self) -> &'static str { "emit" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, _: &(), ctx: &mut LocalNodeCtx) -> Result<(), String> {
            ctx.set_output("out", self.value);
            ctx.metric("emitted", self.value as f64);
            Ok(())
        }
    }

    struct DoubleNode;
    impl LocalNode<()> for DoubleNode {
        fn node_type(&self) -> &'static str { "double" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, _: &(), ctx: &mut LocalNodeCtx) -> Result<(), String> {
            let v = *ctx.input::<i32>("in").unwrap();
            ctx.set_output("out", v * 2);
            Ok(())
        }
    }

    struct CollectNode;
    impl LocalNode<()> for CollectNode {
        fn node_type(&self) -> &'static str { "collect" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn execute(&mut self, _: &(), ctx: &mut LocalNodeCtx) -> Result<(), String> {
            let v = *ctx.input::<i32>("in").unwrap();
            ctx.metric("received", v as f64);
            Ok(())
        }
    }

    #[test]
    fn local_linear() {
        let mut dag = LocalDag::<()>::new();
        dag.add_node("source", EmitNode { value: 7 });
        dag.add_node("double", DoubleNode);
        dag.add_node("sink", CollectNode);

        dag.connect("source", "out", "double", "in").unwrap();
        dag.connect("double", "out", "sink", "in").unwrap();

        let result = dag.execute(&()).unwrap();
        assert_eq!(result.node_results.len(), 3);
        let sink = result.get("sink").unwrap();
        assert_eq!(sink.metrics[0].1, 14.0);
    }

    #[test]
    fn local_with_services() {
        struct Multiplier(i32);

        struct MulNode;
        impl LocalNode<Multiplier> for MulNode {
            fn node_type(&self) -> &'static str { "mul" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("in", PortType::of::<i32>())]
            }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("out", PortType::of::<i32>())]
            }
            fn execute(&mut self, svc: &Multiplier, ctx: &mut LocalNodeCtx) -> Result<(), String> {
                let v = *ctx.input::<i32>("in").unwrap();
                ctx.set_output("out", v * svc.0);
                Ok(())
            }
        }

        struct EmitLocal(i32);
        impl LocalNode<Multiplier> for EmitLocal {
            fn node_type(&self) -> &'static str { "emit" }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("out", PortType::of::<i32>())]
            }
            fn execute(&mut self, _: &Multiplier, ctx: &mut LocalNodeCtx) -> Result<(), String> {
                ctx.set_output("out", self.0);
                Ok(())
            }
        }

        let mut dag = LocalDag::new();
        dag.add_node("source", EmitLocal(5));
        dag.add_node("mul", MulNode);
        dag.connect("source", "out", "mul", "in").unwrap();

        let (result, dag_result) = dag.execute_and_take::<i32>(&Multiplier(3), "mul", "out").unwrap();
        assert_eq!(result, 15);
        assert_eq!(dag_result.node_results.len(), 2);
    }

    #[test]
    fn local_execute_and_take() {
        let mut dag = LocalDag::<()>::new();
        dag.add_node("source", EmitNode { value: 21 });
        dag.add_node("double", DoubleNode);
        dag.connect("source", "out", "double", "in").unwrap();

        let (value, result) = dag.execute_and_take::<i32>(&(), "double", "out").unwrap();
        assert_eq!(value, 42);
        assert_eq!(result.node_results.len(), 2);
    }

    #[test]
    fn local_fan_out() {
        // source -> double1, source -> double2 (fan-out via Rc::clone)
        // double1 + double2 -> sum
        struct SumNode;
        impl LocalNode<()> for SumNode {
            fn node_type(&self) -> &'static str { "sum" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![
                    PortDef::required("a", PortType::of::<i32>()),
                    PortDef::required("b", PortType::of::<i32>()),
                ]
            }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("out", PortType::of::<i32>())]
            }
            fn execute(&mut self, _: &(), ctx: &mut LocalNodeCtx) -> Result<(), String> {
                let a = *ctx.input::<i32>("a").unwrap();
                let b = *ctx.input::<i32>("b").unwrap();
                ctx.set_output("out", a + b);
                Ok(())
            }
        }

        let mut dag = LocalDag::<()>::new();
        dag.add_node("source", EmitNode { value: 5 });
        dag.add_node("d1", DoubleNode);
        dag.add_node("d2", DoubleNode);
        dag.add_node("sum", SumNode);

        dag.connect("source", "out", "d1", "in").unwrap();
        dag.connect("source", "out", "d2", "in").unwrap();
        dag.connect("d1", "out", "sum", "a").unwrap();
        dag.connect("d2", "out", "sum", "b").unwrap();

        let (value, _) = dag.execute_and_take::<i32>(&(), "sum", "out").unwrap();
        assert_eq!(value, 20); // (5*2) + (5*2)
    }

    #[test]
    fn local_failure() {
        struct FailNode;
        impl LocalNode<()> for FailNode {
            fn node_type(&self) -> &'static str { "fail" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("in", PortType::of::<i32>())]
            }
            fn execute(&mut self, _: &(), _ctx: &mut LocalNodeCtx) -> Result<(), String> {
                Err("boom".to_string())
            }
        }

        let mut dag = LocalDag::<()>::new();
        dag.add_node("source", EmitNode { value: 1 });
        dag.add_node("fail", FailNode);
        dag.connect("source", "out", "fail", "in").unwrap();

        let err = dag.execute(&()).unwrap_err();
        assert!(err.contains("boom"));
    }

    #[test]
    fn local_cycle_detection() {
        struct PassNode;
        impl LocalNode<()> for PassNode {
            fn node_type(&self) -> &'static str { "pass" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("in", PortType::of::<i32>())]
            }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("out", PortType::of::<i32>())]
            }
            fn execute(&mut self, _: &(), ctx: &mut LocalNodeCtx) -> Result<(), String> {
                let v = *ctx.input::<i32>("in").unwrap();
                ctx.set_output("out", v);
                Ok(())
            }
        }

        let mut dag = LocalDag::<()>::new();
        dag.add_node("a", PassNode);
        dag.add_node("b", PassNode);
        dag.connect("a", "out", "b", "in").unwrap();
        dag.connect("b", "out", "a", "in").unwrap();

        let err = dag.execute(&()).unwrap_err();
        assert!(err.contains("cycle"));
    }
}
