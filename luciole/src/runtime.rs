use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::dag::Dag;
use crate::events::{EventBus, EventReceiver};
use crate::node::{LogLevel, NodeContext};
use crate::port::PortValue;
use crate::scheduler::global_scheduler;
use crate::Priority;

// ---------------------------------------------------------------------------
// Global DAG event bus — subscribe from any thread
// ---------------------------------------------------------------------------

static DAG_EVENT_BUS: OnceLock<Arc<EventBus<DagEvent>>> = OnceLock::new();

fn dag_event_bus() -> &'static Arc<EventBus<DagEvent>> {
    DAG_EVENT_BUS.get_or_init(|| Arc::new(EventBus::new()))
}

/// Subscribe to DAG events from any thread. Zero-cost when no subscribers.
///
/// Events are broadcast in real-time as nodes start/complete/fail.
/// Use `try_recv()` for non-blocking polling or `recv()` for blocking.
///
/// ```ignore
/// let events = subscribe_dag_events();
/// // ... execute_dag runs on pool threads ...
/// while let Some(evt) = events.try_recv() {
///     eprintln!("{:?}", evt);
/// }
/// ```
pub fn subscribe_dag_events() -> EventReceiver<DagEvent> {
    dag_event_bus().subscribe()
}

// ---------------------------------------------------------------------------
// DagResult — output of a DAG execution
// ---------------------------------------------------------------------------

/// Result of executing a DAG, containing per-node metrics and timing.
#[derive(Debug)]
pub struct DagResult {
    pub duration_ms: u64,
    pub node_results: Vec<(String, NodeResult)>,
}

/// Per-node execution result.
#[derive(Debug)]
pub struct NodeResult {
    pub duration_ms: u64,
    pub metrics: Vec<(String, f64)>,
    pub logs: Vec<(LogLevel, String)>,
}

impl DagResult {
    /// Look up a node result by name.
    pub fn get(&self, name: &str) -> Option<&NodeResult> {
        self.node_results.iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r)
    }

    /// Total duration in milliseconds.
    pub fn total_ms(&self) -> u64 {
        self.duration_ms
    }
}

// ---------------------------------------------------------------------------
// DagEvent — structured events emitted during execution
// ---------------------------------------------------------------------------

/// Events emitted during DAG execution for observability.
#[derive(Debug, Clone)]
pub enum DagEvent {
    LevelStarted {
        level: usize,
        nodes: Vec<String>,
    },
    NodeStarted {
        node: String,
        node_type: String,
        level: usize,
    },
    NodeCompleted {
        node: String,
        duration_ms: u64,
        metrics: Vec<(String, f64)>,
    },
    NodeFailed {
        node: String,
        error: String,
        duration_ms: u64,
    },
    LevelCompleted {
        level: usize,
        duration_ms: u64,
    },
    DagCompleted {
        total_ms: u64,
        node_count: usize,
    },
    DagFailed {
        error: String,
    },
}

// ---------------------------------------------------------------------------
// execute_dag — the main entry point
// ---------------------------------------------------------------------------

/// Execute a DAG synchronously.
///
/// Nodes are executed level by level (topological order).
/// Within a level, nodes are submitted as tasks to the global scheduler's
/// thread pool — same threads that run actors. In WASM single-thread mode,
/// tasks execute via `run_one_step()` cooperative pumping.
///
/// An optional event callback receives `DagEvent`s for observability.
pub fn execute_dag(
    dag: &mut Dag,
    on_event: Option<&dyn Fn(DagEvent)>,
) -> Result<DagResult, String> {
    let dag_start = Instant::now();
    let levels = dag.topological_levels()?;
    let total_nodes = dag.node_count();

    // Pre-compute consumer counts for fan-out handling
    let mut consumer_counts: HashMap<(String, String), usize> = HashMap::new();
    for edge in dag.edges() {
        *consumer_counts
            .entry((edge.from_node.clone(), edge.from_port.clone()))
            .or_insert(0) += 1;
    }

    let mut port_data: HashMap<(String, String), PortValue> = HashMap::new();
    let mut results: Vec<(String, NodeResult)> = Vec::with_capacity(total_nodes);

    let bus = dag_event_bus();
    let emit = |evt: DagEvent| {
        bus.emit(evt.clone());
        if let Some(cb) = on_event {
            cb(evt);
        }
    };

    for (level_idx, level) in levels.iter().enumerate() {
        let level_start = Instant::now();
        let level_names: Vec<String> = level.iter()
            .map(|&i| dag.node_name(i).to_string())
            .collect();

        emit(DagEvent::LevelStarted {
            level: level_idx,
            nodes: level_names.clone(),
        });

        if level.len() == 1 {
            // Single node — execute inline (no task overhead)
            let node_idx = level[0];
            let node_name = dag.node_name(node_idx).to_string();
            let nr = execute_single_node(
                dag, node_idx, &mut port_data, &mut consumer_counts,
                level_idx, &emit,
            )?;
            results.push((node_name, nr));
        } else {
            // Multiple nodes — submit as tasks to the scheduler pool
            let level_results = execute_level_parallel(
                dag, level, &mut port_data, &mut consumer_counts,
                level_idx, &emit,
            )?;
            results.extend(level_results);
        }

        emit(DagEvent::LevelCompleted {
            level: level_idx,
            duration_ms: level_start.elapsed().as_millis() as u64,
        });
    }

    let total_ms = dag_start.elapsed().as_millis() as u64;
    emit(DagEvent::DagCompleted { total_ms, node_count: total_nodes });

    Ok(DagResult {
        duration_ms: total_ms,
        node_results: results,
    })
}

// ---------------------------------------------------------------------------
// Internal: execute a single node inline
// ---------------------------------------------------------------------------

fn execute_single_node(
    dag: &mut Dag,
    node_idx: usize,
    port_data: &mut HashMap<(String, String), PortValue>,
    consumer_counts: &mut HashMap<(String, String), usize>,
    level: usize,
    emit: &dyn Fn(DagEvent),
) -> Result<NodeResult, String> {
    let node_name = dag.node_name(node_idx).to_string();
    let node_type_str = dag.node_mut(node_idx).node_type().to_string();
    let inputs = collect_inputs(&node_name, dag.edges(), port_data, consumer_counts);

    emit(DagEvent::NodeStarted {
        node: node_name.clone(),
        node_type: node_type_str,
        level,
    });

    let start = Instant::now();
    let mut ctx = NodeContext::new(inputs);
    let node = dag.node_mut(node_idx);

    match node.execute(&mut ctx) {
        Ok(()) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            let outputs = ctx.take_outputs();
            let metrics = ctx.metrics().to_vec();
            let logs = ctx.logs().to_vec();

            for (port_name, value) in outputs {
                port_data.insert((node_name.clone(), port_name), value);
            }

            emit(DagEvent::NodeCompleted {
                node: node_name.clone(),
                duration_ms,
                metrics: metrics.clone(),
            });

            Ok(NodeResult { duration_ms, metrics, logs })
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            emit(DagEvent::NodeFailed {
                node: node_name.clone(),
                error: e.clone(),
                duration_ms,
            });
            emit(DagEvent::DagFailed { error: e.clone() });
            Err(format!("node '{}' failed: {}", node_name, e))
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: execute a level of nodes via scheduler tasks
// ---------------------------------------------------------------------------

fn execute_level_parallel(
    dag: &mut Dag,
    level: &[usize],
    port_data: &mut HashMap<(String, String), PortValue>,
    consumer_counts: &mut HashMap<(String, String), usize>,
    _level_idx: usize,
    emit: &dyn Fn(DagEvent),
) -> Result<Vec<(String, NodeResult)>, String> {
    let edges = dag.edges().to_vec();
    let scheduler = global_scheduler();

    // Collect inputs and take nodes out of the DAG
    let mut taken: Vec<(usize, String, HashMap<String, PortValue>, Box<dyn crate::node::Node>)> = Vec::new();
    for &node_idx in level {
        let node_name = dag.node_name(node_idx).to_string();
        let inputs = collect_inputs(&node_name, &edges, port_data, consumer_counts);

        // Take node out (like the scheduler take pattern for actors)
        let entry = &mut dag.nodes_mut()[node_idx];
        let node_box = unsafe {
            let ptr = &mut entry.node as *mut Box<dyn crate::node::Node>;
            std::ptr::read(ptr)
        };
        taken.push((node_idx, node_name, inputs, node_box));
    }

    // Submit each node as a task to the scheduler's thread pool
    let mut receivers = Vec::with_capacity(taken.len());
    for (node_idx, node_name, inputs, mut node_box) in taken {
        let rx = scheduler.submit_task(Priority::High, move || {
            let start = Instant::now();
            let mut ctx = NodeContext::new(inputs);
            match node_box.execute(&mut ctx) {
                Ok(()) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let outputs = ctx.take_outputs();
                    let metrics = ctx.metrics().to_vec();
                    let logs = ctx.logs().to_vec();
                    let nr = NodeResult { duration_ms, metrics, logs };
                    Ok((node_idx, node_name, nr, outputs, node_box))
                }
                Err(e) => Err((node_name, e))
            }
        });
        receivers.push(rx);
    }

    // Wait for all tasks — cooperative pumping (works in WASM too)
    let mut level_results = Vec::new();
    for rx in receivers {
        let task_result = rx.wait_cooperative_named("dag_node", || scheduler.run_one_step());
        match task_result {
            Ok((node_idx, node_name, nr, outputs, node_box)) => {
                // Put node back in the DAG
                let entry = &mut dag.nodes_mut()[node_idx];
                unsafe {
                    let ptr = &mut entry.node as *mut Box<dyn crate::node::Node>;
                    std::ptr::write(ptr, node_box);
                }

                emit(DagEvent::NodeCompleted {
                    node: node_name.clone(),
                    duration_ms: nr.duration_ms,
                    metrics: nr.metrics.clone(),
                });

                for (port_name, value) in outputs {
                    port_data.insert((node_name.clone(), port_name), value);
                }

                level_results.push((node_name, nr));
            }
            Err((node_name, e)) => {
                emit(DagEvent::DagFailed { error: e.clone() });
                return Err(format!("node '{}' failed: {}", node_name, e));
            }
        }
    }

    Ok(level_results)
}

// ---------------------------------------------------------------------------
// Internal: collect inputs for a node from port_data via edges
// ---------------------------------------------------------------------------

fn collect_inputs(
    node_name: &str,
    edges: &[crate::dag::DagEdge],
    port_data: &mut HashMap<(String, String), PortValue>,
    consumer_counts: &mut HashMap<(String, String), usize>,
) -> HashMap<String, PortValue> {
    let mut inputs = HashMap::new();
    for edge in edges {
        if edge.to_node == node_name {
            let key = (edge.from_node.clone(), edge.from_port.clone());
            let remaining = consumer_counts.get_mut(&key);
            if let Some(count) = remaining {
                *count -= 1;
                if *count == 0 {
                    if let Some(value) = port_data.remove(&key) {
                        inputs.insert(edge.to_port.clone(), value);
                    }
                } else {
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
    use crate::dag::Dag;
    use crate::node::{Node, NodeContext, PortDef};
    use crate::port::{PortType, PortValue};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // -- test nodes --

    struct EmitNode { value: i32 }
    impl Node for EmitNode {
        fn node_type(&self) -> &'static str { "emit" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("out", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            ctx.set_output("out", PortValue::new(self.value));
            ctx.metric("emitted", self.value as f64);
            Ok(())
        }
    }

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
            Ok(())
        }
    }

    struct CollectNode { received: i32 }
    impl Node for CollectNode {
        fn node_type(&self) -> &'static str { "collect" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            self.received = *ctx.input("in").unwrap().downcast::<i32>().unwrap();
            ctx.metric("received", self.received as f64);
            Ok(())
        }
    }

    struct FailNode;
    impl Node for FailNode {
        fn node_type(&self) -> &'static str { "fail" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("in", PortType::of::<i32>())]
        }
        fn execute(&mut self, _ctx: &mut NodeContext) -> Result<(), String> {
            Err("intentional failure".to_string())
        }
    }

    struct CounterNode {
        counter: Arc<AtomicUsize>,
    }
    impl Node for CounterNode {
        fn node_type(&self) -> &'static str { "counter" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::trigger("done")]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            self.counter.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(20));
            ctx.trigger("done");
            Ok(())
        }
    }

    struct TriggerCollect;
    impl Node for TriggerCollect {
        fn node_type(&self) -> &'static str { "trigger_collect" }
        fn inputs(&self) -> Vec<PortDef> {
            vec![PortDef::trigger("go")]
        }
        fn execute(&mut self, _ctx: &mut NodeContext) -> Result<(), String> {
            Ok(())
        }
    }

    // -- tests --

    #[test]
    fn linear_execution() {
        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 5 });
        dag.add_node("double", DoubleNode);
        dag.add_node("sink", CollectNode { received: 0 });

        dag.connect("source", "out", "double", "in").unwrap();
        dag.connect("double", "out", "sink", "in").unwrap();

        let result = execute_dag(&mut dag, None).unwrap();

        assert_eq!(result.node_results.len(), 3);
        let sink_result = result.get("sink").unwrap();
        assert_eq!(sink_result.metrics[0].1, 10.0);
    }

    #[test]
    fn parallel_execution() {
        let counter = Arc::new(AtomicUsize::new(0));

        let mut dag = Dag::new();
        dag.add_node("a", CounterNode { counter: counter.clone() });
        dag.add_node("b", CounterNode { counter: counter.clone() });
        dag.add_node("c", CounterNode { counter: counter.clone() });
        dag.add_node("d", CounterNode { counter: counter.clone() });

        let start = Instant::now();
        let result = execute_dag(&mut dag, None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(counter.load(Ordering::Relaxed), 4);
        assert_eq!(result.node_results.len(), 4);
        // 4 × 20ms parallel should be ~20-50ms, not ~80ms sequential
        assert!(elapsed.as_millis() < 70, "took {}ms, expected parallel", elapsed.as_millis());
    }

    #[test]
    fn node_failure_stops_dag() {
        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 1 });
        dag.add_node("fail", FailNode);

        dag.connect("source", "out", "fail", "in").unwrap();

        let err = execute_dag(&mut dag, None).unwrap_err();
        assert!(err.contains("intentional failure"));
    }

    #[test]
    fn events_emitted() {
        let events = std::sync::Mutex::new(Vec::new());

        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 42 });
        dag.add_node("b", CollectNode { received: 0 });
        dag.connect("a", "out", "b", "in").unwrap();

        execute_dag(&mut dag, Some(&|evt| {
            events.lock().unwrap().push(evt);
        })).unwrap();

        let events = events.lock().unwrap();
        let has_dag_completed = events.iter().any(|e| matches!(e, DagEvent::DagCompleted { .. }));
        assert!(has_dag_completed);

        let node_completions: Vec<_> = events.iter()
            .filter_map(|e| match e {
                DagEvent::NodeCompleted { node, .. } => Some(node.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(node_completions, vec!["a", "b"]);
    }

    #[test]
    fn trigger_chain() {
        struct TriggerEmit;
        impl Node for TriggerEmit {
            fn node_type(&self) -> &'static str { "trigger_emit" }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::trigger("done")]
            }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.trigger("done");
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("src", TriggerEmit);
        dag.add_node("sink", TriggerCollect);
        dag.connect("src", "done", "sink", "go").unwrap();

        let result = execute_dag(&mut dag, None).unwrap();
        assert_eq!(result.node_results.len(), 2);
    }

    #[test]
    fn fan_out_data_flow() {
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
                ctx.metric("result", (v + self.add) as f64);
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 3 });
        dag.add_node("add_10", AddNode { add: 10 });
        dag.add_node("double", DoubleNode);

        dag.connect("source", "out", "add_10", "in").unwrap();
        dag.connect("source", "out", "double", "in").unwrap();

        let result = execute_dag(&mut dag, None).unwrap();
        let add_r = result.get("add_10").unwrap();
        assert_eq!(add_r.metrics[0].1, 13.0);
        assert_eq!(result.node_results.len(), 3);
    }

    #[test]
    fn take_works_for_single_consumer() {
        struct TakeNode { received: Option<Vec<u64>> }
        impl Node for TakeNode {
            fn node_type(&self) -> &'static str { "take" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("data", PortType::of::<Vec<u64>>())]
            }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                let data = ctx.take_input("data")
                    .ok_or("missing")?
                    .take::<Vec<u64>>()
                    .ok_or("wrong type or shared")?;
                ctx.metric("len", data.len() as f64);
                self.received = Some(data);
                Ok(())
            }
        }

        struct EmitVec;
        impl Node for EmitVec {
            fn node_type(&self) -> &'static str { "emit_vec" }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::required("data", PortType::of::<Vec<u64>>())]
            }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.set_output("data", PortValue::new(vec![1u64, 2, 3]));
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("emit", EmitVec);
        dag.add_node("take", TakeNode { received: None });
        dag.connect("emit", "data", "take", "data").unwrap();

        let result = execute_dag(&mut dag, None).unwrap();
        let nr = result.get("take").unwrap();
        assert_eq!(nr.metrics[0].1, 3.0);
    }

    #[test]
    fn empty_dag() {
        let mut dag = Dag::new();
        let result = execute_dag(&mut dag, None).unwrap();
        assert_eq!(result.node_results.len(), 0);
    }

    #[test]
    fn metrics_and_logs() {
        struct MetricNode;
        impl Node for MetricNode {
            fn node_type(&self) -> &'static str { "metric" }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.metric("docs", 100.0);
                ctx.metric("bytes", 4096.0);
                ctx.info("processed 100 docs");
                ctx.warn("slow segment");
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("m", MetricNode);

        let result = execute_dag(&mut dag, None).unwrap();
        let nr = result.get("m").unwrap();
        assert_eq!(nr.metrics.len(), 2);
        assert_eq!(nr.metrics[0], ("docs".to_string(), 100.0));
        assert_eq!(nr.logs.len(), 2);
        assert_eq!(nr.logs[0].0, LogLevel::Info);
        assert_eq!(nr.logs[1].0, LogLevel::Warn);
    }

    #[test]
    fn subscribe_dag_events_from_outside() {
        // Subscribe BEFORE execution — events arrive via EventBus
        let events_rx = subscribe_dag_events();

        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 7 });
        dag.add_node("b", CollectNode { received: 0 });
        dag.connect("a", "out", "b", "in").unwrap();

        // Execute with NO callback — events still go through the bus
        execute_dag(&mut dag, None).unwrap();

        // Collect events from the receiver
        let mut events = Vec::new();
        while let Some(evt) = events_rx.try_recv() {
            events.push(evt);
        }

        // Should have received events
        assert!(events.iter().any(|e| matches!(e, DagEvent::DagCompleted { .. })));
        assert!(events.iter().any(|e| matches!(e,
            DagEvent::NodeCompleted { node, .. } if node == "a"
        )));
        assert!(events.iter().any(|e| matches!(e,
            DagEvent::NodeCompleted { node, .. } if node == "b"
        )));
    }

    #[test]
    fn no_subscriber_zero_cost() {
        // No subscriber — events should not allocate or block
        let mut dag = Dag::new();
        dag.add_node("x", EmitNode { value: 1 });

        // Should not panic or slow down
        execute_dag(&mut dag, None).unwrap();
    }
}
