use std::collections::HashMap;

use crate::port::{PortType, PortValue};

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
}

impl NodeContext {
    pub(crate) fn new(inputs: HashMap<String, PortValue>) -> Self {
        Self {
            inputs,
            outputs: HashMap::new(),
            metrics: Vec::new(),
            logs: Vec::new(),
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
}
