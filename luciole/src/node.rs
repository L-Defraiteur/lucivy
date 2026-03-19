use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use crate::port::{PortType, PortValue};

// ---------------------------------------------------------------------------
// ServiceRegistry — optional shared services for nodes
// ---------------------------------------------------------------------------

/// String-keyed registry of shared services accessible by nodes.
///
/// Optional — lucivy nodes capture context at construction (no registry needed).
/// Rag3weaver nodes use this for dependency injection (DB connections, etc.).
pub struct ServiceRegistry {
    services: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self { services: HashMap::new() }
    }

    pub fn register<T: Send + Sync + 'static>(&mut self, key: &str, value: T) {
        self.services.insert(key.to_string(), Box::new(value));
    }

    pub fn get<T: 'static>(&self, key: &str) -> Option<&T> {
        self.services.get(key)?.downcast_ref()
    }
}

// ---------------------------------------------------------------------------
// PortDef — static port declaration
// ---------------------------------------------------------------------------

/// Declares an input or output port on a node.
#[derive(Debug, Clone)]
pub struct PortDef {
    pub name: &'static str,
    pub port_type: PortType,
    pub required: bool,
}

impl PortDef {
    pub fn required(name: &'static str, port_type: PortType) -> Self {
        Self { name, port_type, required: true }
    }

    pub fn optional(name: &'static str, port_type: PortType) -> Self {
        Self { name, port_type, required: false }
    }

    pub fn trigger(name: &'static str) -> Self {
        Self { name, port_type: PortType::Trigger, required: true }
    }
}

// ---------------------------------------------------------------------------
// Node trait
// ---------------------------------------------------------------------------

/// A synchronous DAG node.
///
/// Nodes declare typed input/output ports and execute a unit of work.
/// The runtime populates inputs before calling `execute` and collects
/// outputs after it returns.
pub trait Node: Send {
    /// Unique node name within the DAG (set by the DAG, not the node).
    fn node_type(&self) -> &'static str;

    /// Input port declarations.
    fn inputs(&self) -> Vec<PortDef> {
        vec![]
    }

    /// Output port declarations.
    fn outputs(&self) -> Vec<PortDef> {
        vec![]
    }

    /// Execute the node. Read from `ctx.input()` / `ctx.take_input()`,
    /// write to `ctx.set_output()`, emit metrics via `ctx.metric()`.
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;

    /// Downcast to concrete type (used by GraphNode to access InjectNode/CollectNode).
    fn as_any_mut(&mut self) -> &mut dyn Any { unimplemented!("as_any_mut not supported for this node") }

    /// Whether this node supports undo. Default: false.
    fn can_undo(&self) -> bool { false }

    /// Capture undo context after successful execute(). Called by the runtime.
    /// Return Some(data) to enable rollback of this node.
    fn undo_context(&self) -> Option<Box<dyn Any + Send>> { None }

    /// Reverse the effects of execute() using the captured context.
    /// Called in reverse topological order when a downstream node fails.
    fn undo(&mut self, _ctx: Box<dyn Any + Send>) -> Result<(), String> {
        Err("undo not supported".to_string())
    }
}

// ---------------------------------------------------------------------------
// PollNode — cooperative yielding for long-running nodes
// ---------------------------------------------------------------------------

/// Result of a single poll step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePoll {
    /// The node has completed.
    Ready,
    /// The node has more work. The runtime will call `poll_execute` again.
    Pending,
}

/// A node that executes incrementally via cooperative polling.
///
/// Use this for long-running work (merges, large I/O) that should yield
/// periodically. In multi-thread mode, yields are near-free. In WASM
/// single-thread mode, yields let other work items (actors, other nodes)
/// run between steps.
///
/// PollNodes are automatically adapted to `Node` via `PollNodeAdapter`.
pub trait PollNode: Send {
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> Vec<PortDef> { vec![] }
    fn outputs(&self) -> Vec<PortDef> { vec![] }

    /// Advance one step. Called repeatedly until `NodePoll::Ready`.
    ///
    /// The context is the same across all calls — outputs and metrics
    /// accumulate. Set outputs when ready.
    fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String>;
}

/// Adapter: wraps a `PollNode` into a `Node` by looping until Ready.
pub struct PollNodeAdapter<N> {
    inner: N,
}

impl<N: PollNode> PollNodeAdapter<N> {
    pub fn new(node: N) -> Self {
        Self { inner: node }
    }
}

impl<N: PollNode> Node for PollNodeAdapter<N> {
    fn node_type(&self) -> &'static str { self.inner.node_type() }
    fn inputs(&self) -> Vec<PortDef> { self.inner.inputs() }
    fn outputs(&self) -> Vec<PortDef> { self.inner.outputs() }

    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        loop {
            match self.inner.poll_execute(ctx)? {
                NodePoll::Ready => return Ok(()),
                NodePoll::Pending => std::thread::yield_now(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LogLevel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

// ---------------------------------------------------------------------------
// NodeContext — sandbox for node execution
// ---------------------------------------------------------------------------

/// Execution context handed to a node during `execute()`.
///
/// Provides access to input values, a place to store output values,
/// and structured metrics/logs.
pub struct NodeContext {
    inputs: HashMap<String, PortValue>,
    outputs: HashMap<String, PortValue>,
    metrics: Vec<(String, f64)>,
    logs: Vec<(LogLevel, String)>,
    services: Option<Arc<ServiceRegistry>>,
}

impl NodeContext {
    pub(crate) fn new(inputs: HashMap<String, PortValue>) -> Self {
        Self {
            inputs,
            outputs: HashMap::new(),
            metrics: Vec::new(),
            logs: Vec::new(),
            services: None,
        }
    }

    pub(crate) fn with_services(inputs: HashMap<String, PortValue>, services: Arc<ServiceRegistry>) -> Self {
        Self {
            inputs,
            outputs: HashMap::new(),
            metrics: Vec::new(),
            logs: Vec::new(),
            services: Some(services),
        }
    }

    // -- inputs --

    /// Borrow an input value by port name.
    pub fn input(&self, port: &str) -> Option<&PortValue> {
        self.inputs.get(port)
    }

    /// Move an input value out (consumed). Useful for large payloads.
    pub fn take_input(&mut self, port: &str) -> Option<PortValue> {
        self.inputs.remove(port)
    }

    /// Check if a trigger input is present.
    pub fn has_trigger(&self, port: &str) -> bool {
        self.inputs.get(port).map(|v| v.is_trigger()).unwrap_or(false)
    }

    // -- outputs --

    /// Set an output port value.
    pub fn set_output(&mut self, port: &str, value: PortValue) {
        self.outputs.insert(port.to_string(), value);
    }

    /// Emit a trigger signal on an output port.
    pub fn trigger(&mut self, port: &str) {
        self.outputs.insert(port.to_string(), PortValue::Trigger);
    }

    // -- observability --

    /// Record a numeric metric (e.g. `ctx.metric("docs_merged", 1250.0)`).
    pub fn metric(&mut self, key: &str, value: f64) {
        self.metrics.push((key.to_string(), value));
    }

    pub fn debug(&mut self, msg: &str) {
        self.logs.push((LogLevel::Debug, msg.to_string()));
    }

    pub fn info(&mut self, msg: &str) {
        self.logs.push((LogLevel::Info, msg.to_string()));
    }

    pub fn warn(&mut self, msg: &str) {
        self.logs.push((LogLevel::Warn, msg.to_string()));
    }

    pub fn error(&mut self, msg: &str) {
        self.logs.push((LogLevel::Error, msg.to_string()));
    }

    // -- services --

    /// Access a shared service by key. Returns None if no registry
    /// is configured or the key/type doesn't match.
    pub fn service<T: 'static>(&self, key: &str) -> Option<&T> {
        self.services.as_ref()?.get(key)
    }

    // -- accessors for the runtime --

    pub(crate) fn take_outputs(&mut self) -> HashMap<String, PortValue> {
        std::mem::take(&mut self.outputs)
    }

    pub fn metrics(&self) -> &[(String, f64)] {
        &self.metrics
    }

    pub fn logs(&self) -> &[(LogLevel, String)] {
        &self.logs
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::PortValue;

    struct AddOneNode;

    impl Node for AddOneNode {
        fn node_type(&self) -> &'static str { "add_one" }

        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("value", PortType::of::<i32>())]
        }

        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("result", PortType::of::<i32>())]
        }

        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let val = *ctx.input("value")
                .ok_or("missing input 'value'")?
                .downcast::<i32>()
                .ok_or("wrong type for 'value'")?;
            ctx.metric("input_value", val as f64);
            ctx.set_output("result", PortValue::new(val + 1));
            ctx.info(&format!("{} -> {}", val, val + 1));
            Ok(())
        }
    }

    #[test]
    fn node_execute_and_context() {
        let mut node = AddOneNode;
        let mut inputs = HashMap::new();
        inputs.insert("value".to_string(), PortValue::new(41i32));
        let mut ctx = NodeContext::new(inputs);

        node.execute(&mut ctx).unwrap();

        let out = ctx.take_outputs();
        let result = out.get("result").unwrap().downcast::<i32>().unwrap();
        assert_eq!(*result, 42);
        assert_eq!(ctx.metrics().len(), 1);
        assert_eq!(ctx.metrics()[0].0, "input_value");
        assert_eq!(ctx.metrics()[0].1, 41.0);
        assert_eq!(ctx.logs().len(), 1);
        assert_eq!(ctx.logs()[0].0, LogLevel::Info);
    }

    #[test]
    fn port_def_constructors() {
        let p = PortDef::trigger("done");
        assert_eq!(p.name, "done");
        assert_eq!(p.port_type, PortType::Trigger);
        assert!(p.required);

        let p = PortDef::optional("extra", PortType::Any);
        assert!(!p.required);
    }

    #[test]
    fn context_trigger_helpers() {
        let mut inputs = HashMap::new();
        inputs.insert("go".to_string(), PortValue::Trigger);
        let mut ctx = NodeContext::new(inputs);

        assert!(ctx.has_trigger("go"));
        assert!(!ctx.has_trigger("nope"));

        ctx.trigger("done");
        let out = ctx.take_outputs();
        assert!(out.get("done").unwrap().is_trigger());
    }

    #[test]
    fn poll_node_incremental() {
        struct CountdownNode {
            remaining: u32,
            total: u32,
        }

        impl PollNode for CountdownNode {
            fn node_type(&self) -> &'static str { "countdown" }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("result", PortType::of::<u32>())]
            }
            fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String> {
                if self.remaining == 0 {
                    ctx.set_output("result", PortValue::new(self.total));
                    ctx.metric("steps", self.total as f64);
                    return Ok(NodePoll::Ready);
                }
                self.remaining -= 1;
                Ok(NodePoll::Pending)
            }
        }

        // Via adapter
        let mut node = PollNodeAdapter::new(CountdownNode { remaining: 5, total: 5 });
        let mut ctx = NodeContext::new(HashMap::new());
        node.execute(&mut ctx).unwrap();

        let out = ctx.take_outputs();
        assert_eq!(*out.get("result").unwrap().downcast::<u32>().unwrap(), 5);
        assert_eq!(ctx.metrics()[0].1, 5.0);
    }

    #[test]
    fn poll_node_in_dag() {
        use crate::dag::Dag;
        use crate::runtime::execute_dag;

        struct StepNode { steps: u32 }
        impl PollNode for StepNode {
            fn node_type(&self) -> &'static str { "stepper" }
            fn poll_execute(&mut self, ctx: &mut NodeContext) -> Result<NodePoll, String> {
                if self.steps == 0 {
                    ctx.metric("done", 1.0);
                    Ok(NodePoll::Ready)
                } else {
                    self.steps -= 1;
                    Ok(NodePoll::Pending)
                }
            }
        }

        let mut dag = Dag::new();
        dag.add_poll_node("stepper", StepNode { steps: 10 });

        let result = execute_dag(&mut dag, None).unwrap();
        let nr = result.get("stepper").unwrap();
        assert_eq!(nr.metrics[0].1, 1.0);
    }

    #[test]
    fn service_registry() {
        let mut reg = ServiceRegistry::new();
        reg.register("db_url", "postgres://localhost".to_string());
        reg.register("max_retries", 3u32);

        assert_eq!(reg.get::<String>("db_url"), Some(&"postgres://localhost".to_string()));
        assert_eq!(reg.get::<u32>("max_retries"), Some(&3u32));
        assert_eq!(reg.get::<u32>("db_url"), None); // wrong type
        assert_eq!(reg.get::<String>("missing"), None); // missing key
    }

    #[test]
    fn node_context_with_services() {
        let mut reg = ServiceRegistry::new();
        reg.register("answer", 42u64);

        let ctx = NodeContext::with_services(HashMap::new(), Arc::new(reg));
        assert_eq!(ctx.service::<u64>("answer"), Some(&42u64));
        assert_eq!(ctx.service::<u64>("nope"), None);
    }

    #[test]
    fn node_context_without_services() {
        let ctx = NodeContext::new(HashMap::new());
        assert_eq!(ctx.service::<u64>("anything"), None);
    }
}
