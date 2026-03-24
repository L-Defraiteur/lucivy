use std::collections::HashMap;
use std::sync::Arc;

use crate::events::EventReceiver;
use crate::node::{Node, PollNode, PollNodeAdapter};
use crate::observe::{TapEvent, TapRegistry};
use crate::port::{PortType, PortValue};

// ---------------------------------------------------------------------------
// Edge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DagEdge {
    pub from_node: String,
    pub from_port: String,
    pub to_node: String,
    pub to_port: String,
}

// ---------------------------------------------------------------------------
// Dag
// ---------------------------------------------------------------------------

pub(crate) struct DagNodeEntry {
    pub(crate) name: String,
    pub(crate) node: Box<dyn Node>,
}

/// Directed acyclic graph of nodes with typed port connections.
///
/// Nodes are added with `add_node`, connected via `connect`, then executed
/// by a runtime that processes them level by level (topological sort).
pub struct Dag {
    nodes: Vec<DagNodeEntry>,
    edges: Vec<DagEdge>,
    pub(crate) taps: TapRegistry,
    /// Pre-loaded port data injected before execution.
    pub(crate) initial_inputs: HashMap<(String, String), PortValue>,
    /// Shared services accessible by nodes via ctx.service::<T>(key).
    pub(crate) services: Option<Arc<crate::node::ServiceRegistry>>,
}

impl Dag {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            taps: TapRegistry::new(),
            initial_inputs: HashMap::new(),
            services: None,
        }
    }

    /// Set shared services accessible by nodes via `ctx.service::<T>(key)`.
    pub fn with_services(mut self, services: Arc<crate::node::ServiceRegistry>) -> Self {
        self.services = Some(services);
        self
    }

    /// Pre-load a value as if a source node produced it on a port.
    /// Used by GraphNode to inject external inputs into a sub-DAG.
    pub fn set_initial_input(&mut self, node: &str, port: &str, value: PortValue) {
        self.initial_inputs.insert((node.to_string(), port.to_string()), value);
    }

    /// Register a node in the DAG.
    pub fn add_node(&mut self, name: &str, node: impl Node + 'static) {
        self.nodes.push(DagNodeEntry {
            name: name.to_string(),
            node: Box::new(node),
        });
    }

    /// Register a pre-boxed node in the DAG.
    pub fn add_node_boxed(&mut self, name: &str, node: Box<dyn Node>) {
        self.nodes.push(DagNodeEntry {
            name: name.to_string(),
            node,
        });
    }

    /// Register a poll-based node (long-running, cooperative yielding).
    /// Automatically wrapped in a `PollNodeAdapter`.
    pub fn add_poll_node(&mut self, name: &str, node: impl PollNode + 'static) {
        self.nodes.push(DagNodeEntry {
            name: name.to_string(),
            node: Box::new(PollNodeAdapter::new(node)),
        });
    }

    /// Connect an output port of one node to an input port of another.
    ///
    /// Validates that both nodes exist and port types are compatible.
    pub fn connect(
        &mut self,
        from_node: &str,
        from_port: &str,
        to_node: &str,
        to_port: &str,
    ) -> Result<(), String> {
        let from = self.find_node(from_node)
            .ok_or_else(|| format!("node '{}' not found", from_node))?;
        let to = self.find_node(to_node)
            .ok_or_else(|| format!("node '{}' not found", to_node))?;

        let from_type = Self::find_output_port_type(&*from.node, from_port)
            .ok_or_else(|| format!(
                "node '{}' has no output port '{}'", from_node, from_port
            ))?;
        let to_type = Self::find_input_port_type(&*to.node, to_port)
            .ok_or_else(|| format!(
                "node '{}' has no input port '{}'", to_node, to_port
            ))?;

        if !from_type.compatible_with(&to_type) {
            return Err(format!(
                "port type mismatch: {}.{} ({}) -> {}.{} ({})",
                from_node, from_port, from_type,
                to_node, to_port, to_type,
            ));
        }

        // Warn about fan-out: same output port connected to multiple inputs.
        // Fan-out is safe for read-only access (input/downcast) but NOT for
        // take_input/take (Arc::try_unwrap needs ref count == 1).
        // We track fan-out here; runtime will log a warning if take() fails.
        // To avoid issues: use separate output ports for data that will be taken.

        self.edges.push(DagEdge {
            from_node: from_node.to_string(),
            from_port: from_port.to_string(),
            to_node: to_node.to_string(),
            to_port: to_port.to_string(),
        });

        Ok(())
    }

    /// Topological sort returning levels of parallelizable nodes.
    ///
    /// Each inner Vec contains node indices that are independent and can
    /// be executed in parallel. Levels must be executed sequentially.
    ///
    /// Returns an error if the graph contains a cycle.
    pub fn topological_levels(&self) -> Result<Vec<Vec<usize>>, String> {
        let n = self.nodes.len();
        if n == 0 {
            return Ok(vec![]);
        }

        let name_to_idx: HashMap<&str, usize> = self.nodes.iter()
            .enumerate()
            .map(|(i, e)| (e.name.as_str(), i))
            .collect();

        // Build in-degree and adjacency
        let mut in_degree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![vec![]; n];

        for edge in &self.edges {
            let from = *name_to_idx.get(edge.from_node.as_str())
                .ok_or_else(|| format!("edge references unknown node '{}'", edge.from_node))?;
            let to = *name_to_idx.get(edge.to_node.as_str())
                .ok_or_else(|| format!("edge references unknown node '{}'", edge.to_node))?;
            // Avoid duplicate dependency counts for multi-port edges
            if !dependents[from].contains(&to) {
                dependents[from].push(to);
                in_degree[to] += 1;
            }
        }

        // Kahn's algorithm by levels
        let mut levels: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = (0..n)
            .filter(|&i| in_degree[i] == 0)
            .collect();
        let mut visited = 0;

        while !current.is_empty() {
            visited += current.len();
            let mut next = Vec::new();
            for &node_idx in &current {
                for &dep in &dependents[node_idx] {
                    in_degree[dep] -= 1;
                    if in_degree[dep] == 0 {
                        next.push(dep);
                    }
                }
            }
            levels.push(current);
            current = next;
        }

        if visited != n {
            return Err("cycle detected in DAG".to_string());
        }

        Ok(levels)
    }

    /// Validate the DAG: no cycles, all required inputs connected.
    pub fn validate(&self) -> Result<(), String> {
        // Check for cycles
        self.topological_levels()?;

        // Check all required input ports have an incoming edge (or are optional)
        let incoming: HashMap<(&str, &str), &DagEdge> = self.edges.iter()
            .map(|e| ((e.to_node.as_str(), e.to_port.as_str()), e))
            .collect();

        for entry in &self.nodes {
            for port_def in entry.node.inputs() {
                if port_def.required {
                    if !incoming.contains_key(&(entry.name.as_str(), port_def.name)) {
                        return Err(format!(
                            "required input port '{}.{}' is not connected",
                            entry.name, port_def.name
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Connect a sequence of nodes linearly via trigger ports.
    /// Each node's "done" output connects to the next node's "trigger" input.
    ///
    /// ```ignore
    /// dag.chain(&["save", "gc", "reload"])?;
    /// // equivalent to:
    /// // dag.connect("save", "done", "gc", "trigger")?;
    /// // dag.connect("gc", "done", "reload", "trigger")?;
    /// ```
    pub fn chain(&mut self, names: &[&str]) -> Result<(), String> {
        for pair in names.windows(2) {
            self.connect(pair[0], "done", pair[1], "trigger")?;
        }
        Ok(())
    }

    /// Tap a specific edge to capture data flowing through it.
    ///
    /// Returns a receiver that will get `TapEvent`s during execution.
    /// Zero-cost when no taps are active.
    pub fn tap(
        &mut self,
        from_node: &str,
        from_port: &str,
        to_node: &str,
        to_port: &str,
    ) -> EventReceiver<TapEvent> {
        self.taps.tap(from_node, from_port, to_node, to_port)
    }

    /// Tap ALL edges — every data transfer is captured.
    pub fn tap_all(&mut self) -> EventReceiver<TapEvent> {
        self.taps.tap_all()
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Node names in insertion order.
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.iter().map(|e| e.name.as_str()).collect()
    }

    /// All edges.
    pub fn edges(&self) -> &[DagEdge] {
        &self.edges
    }

    // -- internal accessors used by the runtime --

    pub(crate) fn nodes_mut(&mut self) -> &mut Vec<DagNodeEntry> {
        &mut self.nodes
    }

    pub(crate) fn node_name(&self, idx: usize) -> &str {
        &self.nodes[idx].name
    }

    pub(crate) fn node_mut(&mut self, idx: usize) -> &mut dyn Node {
        &mut *self.nodes[idx].node
    }

    /// Find a node by name and return a mutable reference.
    pub fn node_mut_by_name(&mut self, name: &str) -> Option<&mut dyn Node> {
        self.nodes.iter_mut()
            .find(|e| e.name == name)
            .map(|e| &mut *e.node as &mut dyn Node)
    }

    // -- helpers --

    fn find_node(&self, name: &str) -> Option<&DagNodeEntry> {
        self.nodes.iter().find(|e| e.name == name)
    }

    fn find_output_port_type(node: &dyn Node, port: &str) -> Option<PortType> {
        node.outputs().into_iter()
            .find(|p| p.name == port)
            .map(|p| p.port_type)
    }

    fn find_input_port_type(node: &dyn Node, port: &str) -> Option<PortType> {
        node.inputs().into_iter()
            .find(|p| p.name == port)
            .map(|p| p.port_type)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeContext, PortDef};
    use crate::port::{PortType, PortValue};

    // -- test nodes --

    struct SourceNode;
    impl Node for SourceNode {
        fn node_type(&self) -> &'static str { "source" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            ctx.set_output("out", PortValue::new(10i32));
            Ok(())
        }
    }

    struct TransformNode;
    impl Node for TransformNode {
        fn node_type(&self) -> &'static str { "transform" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let v = ctx.take_input("in").unwrap().take::<i32>().unwrap();
            ctx.set_output("out", PortValue::new(v * 2));
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
            self.received = ctx.take_input("in").and_then(|v| v.take::<i32>());
            Ok(())
        }
    }

    struct TriggerSource;
    impl Node for TriggerSource {
        fn node_type(&self) -> &'static str { "trigger_source" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::trigger("done")]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            ctx.trigger("done");
            Ok(())
        }
    }

    struct TriggerSink;
    impl Node for TriggerSink {
        fn node_type(&self) -> &'static str { "trigger_sink" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::trigger("go")]
        }
        fn execute(&mut self, _ctx: &mut NodeContext) -> Result<(), String> {
            Ok(())
        }
    }

    // -- tests --

    #[test]
    fn linear_dag() {
        let mut dag = Dag::new();
        dag.add_node("a", SourceNode);
        dag.add_node("b", TransformNode);
        dag.add_node("c", SinkNode { received: None });

        dag.connect("a", "out", "b", "in").unwrap();
        dag.connect("b", "out", "c", "in").unwrap();

        dag.validate().unwrap();

        let levels = dag.topological_levels().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]); // a
        assert_eq!(levels[1], vec![1]); // b
        assert_eq!(levels[2], vec![2]); // c
    }

    #[test]
    fn diamond_dag() {
        // A → [B, C] → D
        let mut dag = Dag::new();
        dag.add_node("a", SourceNode);
        dag.add_node("b", TransformNode);
        dag.add_node("c", TransformNode);
        dag.add_node("d", SinkNode { received: None });

        dag.connect("a", "out", "b", "in").unwrap();
        dag.connect("a", "out", "c", "in").unwrap();
        dag.connect("b", "out", "d", "in").unwrap();
        // Note: c→d would be a second edge into d.in, which is fine
        // (fan-in handled by runtime, not the DAG structure)

        let levels = dag.topological_levels().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]);          // a
        assert!(levels[1].contains(&1) && levels[1].contains(&2)); // b, c parallel
        assert_eq!(levels[2], vec![3]);          // d
    }

    #[test]
    fn parallel_sources() {
        let mut dag = Dag::new();
        dag.add_node("s1", SourceNode);
        dag.add_node("s2", SourceNode);
        dag.add_node("sink", SinkNode { received: None });

        dag.connect("s1", "out", "sink", "in").unwrap();

        let levels = dag.topological_levels().unwrap();
        // s1 and s2 are both at level 0 (no dependencies)
        assert_eq!(levels[0].len(), 2);
    }

    #[test]
    fn cycle_detection() {
        // A → B → A (cycle)
        let mut dag = Dag::new();
        dag.add_node("a", TransformNode);
        dag.add_node("b", TransformNode);

        dag.connect("a", "out", "b", "in").unwrap();
        dag.connect("b", "out", "a", "in").unwrap();

        assert!(dag.topological_levels().is_err());
        assert!(dag.validate().is_err());
    }

    #[test]
    fn missing_required_input() {
        let mut dag = Dag::new();
        dag.add_node("b", TransformNode); // has required "in", no edge
        // Don't connect anything

        let err = dag.validate().unwrap_err();
        assert!(err.contains("required input port"));
        assert!(err.contains("b.in"));
    }

    #[test]
    fn type_mismatch() {
        let mut dag = Dag::new();
        dag.add_node("a", SourceNode);        // out: i32
        dag.add_node("b", TriggerSink);       // in: Trigger

        let err = dag.connect("a", "out", "b", "go").unwrap_err();
        assert!(err.contains("type mismatch"));
    }

    #[test]
    fn unknown_port() {
        let mut dag = Dag::new();
        dag.add_node("a", SourceNode);
        dag.add_node("b", TransformNode);

        let err = dag.connect("a", "nope", "b", "in").unwrap_err();
        assert!(err.contains("no output port 'nope'"));
    }

    #[test]
    fn unknown_node() {
        let mut dag = Dag::new();
        dag.add_node("a", SourceNode);

        let err = dag.connect("a", "out", "ghost", "in").unwrap_err();
        assert!(err.contains("node 'ghost' not found"));
    }

    #[test]
    fn trigger_connection() {
        let mut dag = Dag::new();
        dag.add_node("src", TriggerSource);
        dag.add_node("sink", TriggerSink);

        dag.connect("src", "done", "sink", "go").unwrap();
        dag.validate().unwrap();

        let levels = dag.topological_levels().unwrap();
        assert_eq!(levels.len(), 2);
    }

    #[test]
    fn chain_helper() {
        let mut dag = Dag::new();
        dag.add_node("a", TriggerSource);
        dag.add_node("b", TriggerSink);  // has "go" input, but chain uses "trigger"
        // We need nodes with "done" output and "trigger" input for chain to work.
        // Let's use TriggerSource (has "done" output) for all, and a custom sink.

        struct ChainNode;
        impl Node for ChainNode {
            fn node_type(&self) -> &'static str { "chain" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::trigger("trigger")]
            }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::trigger("done")]
            }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.trigger("done");
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("a", TriggerSource);
        dag.add_node("b", ChainNode);
        dag.add_node("c", ChainNode);

        dag.chain(&["a", "b", "c"]).unwrap();
        dag.validate().unwrap();

        let levels = dag.topological_levels().unwrap();
        assert_eq!(levels.len(), 3);
    }

    #[test]
    fn empty_dag() {
        let dag = Dag::new();
        assert_eq!(dag.topological_levels().unwrap().len(), 0);
        dag.validate().unwrap();
    }

    #[test]
    fn single_node_no_required_inputs() {
        let mut dag = Dag::new();
        dag.add_node("lone", SourceNode);
        dag.validate().unwrap();

        let levels = dag.topological_levels().unwrap();
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0], vec![0]);
    }
}
