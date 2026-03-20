//! Scatter-gather DAG builder — N independent named tasks in parallel.
//!
//! ```text
//! task_a ──┐
//! task_b ──┼── collect ── "results": HashMap<String, PortValue>
//! task_c ──┘
//! ```
//!
//! Each task has a name and a closure. All tasks run in parallel.
//! The collect node gathers results into a named map.

use std::collections::HashMap;

use crate::node::{Node, NodeContext, PortDef};
use crate::port::{PortType, PortValue};
use crate::Dag;

// ---------------------------------------------------------------------------
// FnNode — a node that wraps a closure
// ---------------------------------------------------------------------------

struct FnNode<F: FnOnce() -> Result<PortValue, String> + Send + 'static> {
    f: Option<F>,
}

impl<F: FnOnce() -> Result<PortValue, String> + Send + 'static> Node for FnNode<F> {
    fn node_type(&self) -> &'static str { "scatter_task" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("out", PortType::Any)]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let f = self.f.take().ok_or("already executed")?;
        ctx.set_output("out", f()?);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CollectNode — gathers N named inputs into a HashMap<String, PortValue>
// ---------------------------------------------------------------------------

struct CollectNode {
    names: Vec<String>,
}

impl Node for CollectNode {
    fn node_type(&self) -> &'static str { "scatter_collect" }
    fn inputs(&self) -> Vec<PortDef> {
        self.names.iter()
            .map(|name| PortDef::required(
                Box::leak(name.clone().into_boxed_str()),
                PortType::Any,
            ))
            .collect()
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("results", PortType::of::<HashMap<String, PortValue>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let mut results = HashMap::with_capacity(self.names.len());
        for name in &self.names {
            let value = ctx.take_input(name)
                .ok_or_else(|| format!("missing scatter result '{name}'"))?;
            results.insert(name.clone(), value);
        }
        ctx.set_output("results", PortValue::new(results));
        Ok(())
    }
}

/// Result map from a scatter DAG. Wraps `HashMap<String, PortValue>`.
pub struct ScatterResults {
    map: HashMap<String, PortValue>,
}

impl From<HashMap<String, PortValue>> for ScatterResults {
    fn from(map: HashMap<String, PortValue>) -> Self {
        Self { map }
    }
}

impl ScatterResults {
    /// Take a result by name, consuming it.
    pub fn take<T: Send + Sync + 'static>(&mut self, name: &str) -> Option<T> {
        self.map.remove(name)?.take::<T>()
    }

    /// Number of results.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Iterate over names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(|s| s.as_str())
    }

    /// Drain all results as (name, PortValue) pairs.
    pub fn drain(&mut self) -> impl Iterator<Item = (String, PortValue)> + '_ {
        self.map.drain()
    }
}

// ---------------------------------------------------------------------------
// build_scatter_dag
// ---------------------------------------------------------------------------

/// Build a scatter-gather DAG from named tasks.
///
/// Each (name, closure) pair runs as an independent node in parallel.
/// The collect node gathers results into a named map.
///
/// ```ignore
/// let dag = build_scatter_dag(vec![
///     ("field_0", || Ok(PortValue::new(data_0))),
///     ("field_1", || Ok(PortValue::new(data_1))),
/// ]);
/// let mut result = execute_dag(&mut dag, None)?;
/// let map = result.take_output::<HashMap<String, PortValue>>("collect", "results")?;
/// let mut scatter = ScatterResults { map };
/// let val = scatter.take::<MyType>("field_0").unwrap();
/// ```
pub fn build_scatter_dag<F>(tasks: Vec<(&str, F)>) -> Dag
where
    F: FnOnce() -> Result<PortValue, String> + Send + 'static,
{
    let names: Vec<String> = tasks.iter().map(|(name, _)| name.to_string()).collect();
    let mut dag = Dag::new();

    for (name, f) in tasks {
        dag.add_node(name, FnNode { f: Some(f) });
    }

    dag.add_node("collect", CollectNode { names: names.clone() });

    for name in &names {
        dag.connect(name, "out", "collect", name).unwrap();
    }

    dag
}
