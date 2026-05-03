use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::checkpoint::CheckpointStore;
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

/// Result of executing a DAG, containing per-node metrics, timing,
/// and remaining outputs (from leaf nodes not consumed by downstream edges).
#[derive(Debug)]
pub struct DagResult {
    pub duration_ms: u64,
    pub node_results: Vec<(String, NodeResult)>,
    /// Remaining port data: outputs from leaf nodes not consumed by any edge.
    /// Key: (node_name, port_name). Use `output()` to extract typed values.
    pub outputs: HashMap<(String, String), PortValue>,
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

    /// Extract a typed output value from a leaf node.
    ///
    /// ```ignore
    /// let results: Vec<SearchResult> = dag_result
    ///     .take_output::<Vec<SearchResult>>("merge", "results")
    ///     .expect("merge node should produce results");
    /// ```
    pub fn take_output<T: Send + Sync + 'static>(
        &mut self,
        node: &str,
        port: &str,
    ) -> Option<T> {
        let key = (node.to_string(), port.to_string());
        self.outputs.remove(&key)?.take::<T>()
    }

    /// Sum a metric across all nodes.
    ///
    /// ```ignore
    /// let total_docs = result.total("docs_merged");
    /// ```
    pub fn total(&self, metric: &str) -> f64 {
        self.node_results.iter()
            .flat_map(|(_, nr)| nr.metrics.iter())
            .filter(|(name, _)| name == metric)
            .map(|(_, value)| value)
            .sum()
    }

    /// Human-readable summary for post-mortem / bench output.
    pub fn display_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("DAG completed in {}ms ({} nodes)",
            self.duration_ms, self.node_results.len()));

        for (name, nr) in &self.node_results {
            let metrics_str = nr.metrics.iter()
                .map(|(k, v)| {
                    if *v == ((*v) as u64) as f64 {
                        format!("{}={}", k, *v as u64)
                    } else {
                        format!("{}={:.1}", k, v)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("  {:30} {:>6}ms  {}",
                name, nr.duration_ms, metrics_str));

            for (level, text) in &nr.logs {
                let tag = match level {
                    LogLevel::Debug => "DBG",
                    LogLevel::Info => "INF",
                    LogLevel::Warn => "WRN",
                    LogLevel::Error => "ERR",
                };
                lines.push(format!("    [{}] {}", tag, text));
            }
        }
        lines.join("\n")
    }
}

impl std::fmt::Display for DagResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_summary())
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
    /// A log message emitted by a node during execution.
    NodeLog {
        node: String,
        node_type: String,
        level: LogLevel,
        text: String,
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

    // Seed port_data with any initial inputs set on the DAG
    let mut port_data: HashMap<(String, String), PortValue> =
        std::mem::take(&mut dag.initial_inputs);
    let mut results: Vec<(String, NodeResult)> = Vec::with_capacity(total_nodes);
    // Undo contexts: (node_idx, undo_data) in execution order
    let mut undo_stack: Vec<(usize, Box<dyn std::any::Any + Send>)> = Vec::new();

    let bus = dag_event_bus();
    let emit = |evt: DagEvent| {
        bus.emit(evt.clone());
        if let Some(cb) = on_event {
            cb(evt);
        }
    };

    let execute_result: Result<(), String> = (|| {
        for (level_idx, level) in levels.iter().enumerate() {
            let level_start = Instant::now();
            let level_names: Vec<String> = level.iter()
                .map(|&i| dag.node_name(i).to_string())
                .collect();

            emit(DagEvent::LevelStarted {
                level: level_idx,
                nodes: level_names.clone(),
            });

            // If on a scheduler thread, execute ALL nodes inline (sequentially)
            // to avoid thread pool starvation deadlocks. Parallel execution is
            // only safe when called from a non-scheduler thread.
            // Run inline when: on a scheduler thread (avoid pool starvation),
            // inside an actor handler (avoid nested cooperative waits),
            // inside a cooperative wait (avoid recursive nesting), or
            // single-node level (no benefit from parallelism).
            let inline = crate::scheduler::is_scheduler_thread()
                || crate::scheduler::in_actor_handler()
                || crate::scheduler::in_cooperative_wait()
                || level.len() == 1;

            if inline {
                for &node_idx in level {
                    let node_name = dag.node_name(node_idx).to_string();
                    let nr = execute_single_node(
                        dag, node_idx, &mut port_data, &mut consumer_counts,
                        level_idx, &emit,
                    )?;
                    if dag.node_mut(node_idx).can_undo() {
                        if let Some(undo_ctx) = dag.node_mut(node_idx).undo_context() {
                            undo_stack.push((node_idx, undo_ctx));
                        }
                    }
                    results.push((node_name, nr));
                }
            } else {
                let level_results = execute_level_parallel(
                    dag, level, &mut port_data, &mut consumer_counts,
                    level_idx, &emit,
                )?;
                // Capture undo contexts for parallel nodes
                for &node_idx in level {
                    if dag.node_mut(node_idx).can_undo() {
                        if let Some(undo_ctx) = dag.node_mut(node_idx).undo_context() {
                            undo_stack.push((node_idx, undo_ctx));
                        }
                    }
                }
                results.extend(level_results);
            }

            emit(DagEvent::LevelCompleted {
                level: level_idx,
                duration_ms: level_start.elapsed().as_millis() as u64,
            });
        }
        Ok(())
    })();

    if let Err(ref error) = execute_result {
        // Rollback: call undo in reverse order on completed nodes
        if !undo_stack.is_empty() {
            for (node_idx, undo_ctx) in undo_stack.into_iter().rev() {
                let node_name = dag.node_name(node_idx).to_string();
                if let Err(undo_err) = dag.node_mut(node_idx).undo(undo_ctx) {
                    emit(DagEvent::NodeFailed {
                        node: format!("undo:{}", node_name),
                        error: undo_err,
                        duration_ms: 0,
                    });
                }
            }
        }
        emit(DagEvent::DagFailed { error: error.clone() });
        return Err(execute_result.unwrap_err());
    }

    let total_ms = dag_start.elapsed().as_millis() as u64;
    emit(DagEvent::DagCompleted { total_ms, node_count: total_nodes });

    Ok(DagResult {
        duration_ms: total_ms,
        node_results: results,
        outputs: port_data,
    })
}

/// Execute a DAG with checkpoint persistence.
///
/// Same as `execute_dag` but saves progress to a `CheckpointStore`.
/// After each node completes, it's recorded. On crash + restart,
/// the caller can load the checkpoint to see which nodes finished
/// and build a smaller DAG with only the remaining work.
pub fn execute_dag_with_checkpoint(
    dag: &mut Dag,
    dag_id: &str,
    store: &dyn CheckpointStore,
    on_event: Option<&dyn Fn(DagEvent)>,
) -> Result<DagResult, String> {
    let dag_start = Instant::now();
    let levels = dag.topological_levels()?;
    let total_nodes = dag.node_count();

    // Load existing checkpoint to skip completed nodes
    let skip: HashSet<String> = store.load(dag_id)
        .map(|cp| cp.completed_nodes.into_iter().collect())
        .unwrap_or_default();

    let mut consumer_counts: HashMap<(String, String), usize> = HashMap::new();
    for edge in dag.edges() {
        *consumer_counts
            .entry((edge.from_node.clone(), edge.from_port.clone()))
            .or_insert(0) += 1;
    }

    let mut port_data: HashMap<(String, String), PortValue> = HashMap::new();
    let mut results: Vec<(String, NodeResult)> = Vec::with_capacity(total_nodes);
    // Track undo contexts for rollback on failure (node_idx, undo_data)
    let mut undo_stack: Vec<(usize, Box<dyn std::any::Any + Send>)> = Vec::new();

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

        for &node_idx in level {
            let node_name = dag.node_name(node_idx).to_string();

            // Skip if checkpointed OR if required trigger inputs are unsatisfied
            // (e.g. downstream of a BranchNode that took the other path).
            let trigger_unsatisfied = if !skip.contains(&node_name) {
                let node_inputs = dag.node_mut(node_idx).inputs();
                node_inputs.iter().any(|port| {
                    port.required && port.port_type == crate::port::PortType::Trigger && {
                        // Check if any edge delivers this trigger
                        let has_trigger = dag.edges().iter().any(|e| {
                            e.to_node == node_name && e.to_port == port.name
                                && port_data.contains_key(&(e.from_node.clone(), e.from_port.clone()))
                        });
                        !has_trigger
                    }
                })
            } else {
                false
            };

            if skip.contains(&node_name) || trigger_unsatisfied {
                emit(DagEvent::NodeCompleted {
                    node: node_name.clone(),
                    duration_ms: 0,
                    metrics: vec![("skipped".to_string(), 1.0)],
                });
                results.push((node_name, NodeResult {
                    duration_ms: 0,
                    metrics: vec![("skipped".to_string(), 1.0)],
                    logs: vec![],
                }));
                continue;
            }

            match execute_single_node(
                dag, node_idx, &mut port_data, &mut consumer_counts,
                level_idx, &emit,
            ) {
                Ok(nr) => {
                    let node = dag.node_mut(node_idx);
                    let node_type_str = node.node_type().to_string();
                    if node.can_undo() {
                        if let Some(undo_ctx) = node.undo_context() {
                            undo_stack.push((node_idx, undo_ctx));
                        }
                    }
                    store.save_node_completed(dag_id, &node_name, &node_type_str);
                    results.push((node_name, nr));
                }
                Err(e) => {
                    // Rollback completed nodes in reverse order
                    rollback_undo_stack_by_idx(dag, &mut undo_stack, &emit);
                    store.save_node_failed(dag_id, &node_name, &e);
                    store.mark_failed(dag_id, &e);
                    emit(DagEvent::DagFailed { error: e.clone() });
                    return Err(e);
                }
            }
        }

        emit(DagEvent::LevelCompleted {
            level: level_idx,
            duration_ms: level_start.elapsed().as_millis() as u64,
        });
    }

    let total_ms = dag_start.elapsed().as_millis() as u64;
    store.mark_completed(dag_id);
    emit(DagEvent::DagCompleted { total_ms, node_count: total_nodes });

    Ok(DagResult {
        duration_ms: total_ms,
        node_results: results,
        outputs: port_data,
    })
}

// ---------------------------------------------------------------------------
// display_progress — ASCII tree of DAG execution state
// ---------------------------------------------------------------------------

/// Render the DAG as an ASCII tree showing execution progress.
///
/// Uses a `DagResult` (or None for pre-execution view) to show status.
///
/// ```text
/// DAG commit (125ms)
/// ├── [✓] plan_merges (2ms)
/// ├─┬ level 1 (parallel)
/// │ ├── [✓] merge_0 (45ms) docs=1250 sfx_ms=30
/// │ ├── [✓] merge_1 (38ms) docs=1000 sfx_ms=25
/// │ └── [✗] merge_2 FAILED: out of disk
/// ├── [~] save_metas (pending)
/// ├── [ ] gc
/// └── [ ] reload
/// ```
pub fn display_progress(dag: &Dag, result: Option<&DagResult>) -> String {
    let levels = match dag.topological_levels() {
        Ok(l) => l,
        Err(e) => return format!("DAG error: {}", e),
    };

    let completed: HashMap<&str, &NodeResult> = result
        .map(|r| r.node_results.iter().map(|(n, nr)| (n.as_str(), nr)).collect())
        .unwrap_or_default();

    let total_ms = result.map(|r| r.duration_ms).unwrap_or(0);
    let mut lines = Vec::new();
    lines.push(if total_ms > 0 {
        format!("DAG ({}ms, {} nodes)", total_ms, dag.node_count())
    } else {
        format!("DAG ({} nodes)", dag.node_count())
    });

    let total_levels = levels.len();
    for (level_idx, level) in levels.iter().enumerate() {
        let is_last_level = level_idx == total_levels - 1;
        let prefix_branch = if is_last_level { "└── " } else { "├── " };
        let prefix_cont = if is_last_level { "    " } else { "│   " };

        if level.len() == 1 {
            let node_name = dag.node_name(level[0]);
            let line = format_node_line(node_name, completed.get(node_name));
            lines.push(format!("{}{}", prefix_branch, line));
        } else {
            // Parallel level
            lines.push(format!("{}┬ level {} ({} parallel)", prefix_branch.replace("── ", "─"), level_idx, level.len()));
            for (i, &node_idx) in level.iter().enumerate() {
                let is_last_node = i == level.len() - 1;
                let node_prefix = if is_last_node { "└── " } else { "├── " };
                let node_name = dag.node_name(node_idx);
                let line = format_node_line(node_name, completed.get(node_name));
                lines.push(format!("{}{}{}", prefix_cont, node_prefix, line));
            }
        }
    }

    lines.join("\n")
}

fn format_node_line(name: &str, result: Option<&&NodeResult>) -> String {
    match result {
        Some(nr) => {
            let has_error = nr.logs.iter().any(|(l, _)| *l == LogLevel::Error);
            let icon = if has_error { "✗" } else { "✓" };
            let metrics_str = nr.metrics.iter()
                .map(|(k, v)| {
                    if *v == (*v as u64) as f64 {
                        format!("{}={}", k, *v as u64)
                    } else {
                        format!("{}={:.1}", k, v)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if metrics_str.is_empty() {
                format!("[{}] {} ({}ms)", icon, name, nr.duration_ms)
            } else {
                format!("[{}] {} ({}ms) {}", icon, name, nr.duration_ms, metrics_str)
            }
        }
        None => format!("[ ] {}", name),
    }
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
        node_type: node_type_str.clone(),
        level,
    });

    let start = Instant::now();
    let mut ctx = NodeContext::new(inputs);
    if let Some(ref services) = dag.services {
        ctx = ctx.with_services(Arc::clone(services));
    }
    let node = dag.node_mut(node_idx);

    match node.execute(&mut ctx) {
        Ok(()) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            let outputs = ctx.take_outputs();
            let metrics = ctx.metrics().to_vec();
            let logs = ctx.logs().to_vec();

            // Emit individual log events
            for (level, text) in &logs {
                emit(DagEvent::NodeLog {
                    node: node_name.clone(),
                    node_type: node_type_str.clone(),
                    level: *level,
                    text: text.clone(),
                });
            }

            for (port_name, value) in outputs {
                // Emit tap events for edges leaving this port
                if dag.taps.is_active() {
                    for edge in dag.edges() {
                        if edge.from_node == node_name && edge.from_port == port_name {
                            dag.taps.check_and_emit(edge, &value);
                        }
                    }
                }
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

    // Collect inputs and take nodes out of the DAG.
    // Skip nodes whose required trigger inputs are unsatisfied (BranchNode inactive path).
    let mut taken: Vec<(usize, String, HashMap<String, PortValue>, Box<dyn crate::node::Node>)> = Vec::new();
    let mut skipped_results: Vec<(String, NodeResult)> = Vec::new();
    for &node_idx in level {
        let node_name = dag.node_name(node_idx).to_string();

        let trigger_unsatisfied = {
            let node_inputs = dag.node_mut(node_idx).inputs();
            node_inputs.iter().any(|port| {
                port.required && port.port_type == crate::port::PortType::Trigger && {
                    let has_trigger = edges.iter().any(|e| {
                        e.to_node == node_name && e.to_port == port.name
                            && port_data.contains_key(&(e.from_node.clone(), e.from_port.clone()))
                    });
                    !has_trigger
                }
            })
        };

        if trigger_unsatisfied {
            emit(DagEvent::NodeCompleted {
                node: node_name.clone(),
                duration_ms: 0,
                metrics: vec![("skipped".to_string(), 1.0)],
            });
            skipped_results.push((node_name, NodeResult {
                duration_ms: 0,
                metrics: vec![("skipped".to_string(), 1.0)],
                logs: vec![],
            }));
            continue;
        }

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
    let services = dag.services.clone();
    let mut receivers = Vec::with_capacity(taken.len());
    for (node_idx, node_name, inputs, mut node_box) in taken {
        let svc = services.clone();
        let rx = scheduler.submit_task(Priority::High, move || {
            let start = Instant::now();
            let mut ctx = NodeContext::new(inputs);
            if let Some(s) = svc {
                ctx = ctx.with_services(s);
            }
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

    // Wait for all tasks
    let mut level_results = Vec::new();
    for rx in receivers {
        let task_result = scheduler.wait(rx, "dag_node");
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
                    if dag.taps.is_active() {
                        for edge in dag.edges() {
                            if edge.from_node == node_name && edge.from_port == port_name {
                                dag.taps.check_and_emit(edge, &value);
                            }
                        }
                    }
                    port_data.insert((node_name.clone(), port_name), value);
                }

                // Emit logs from parallel nodes
                for (level, text) in &nr.logs {
                    emit(DagEvent::NodeLog {
                        node: node_name.clone(),
                        node_type: String::new(), // type not available here
                        level: *level,
                        text: text.clone(),
                    });
                }

                level_results.push((node_name, nr));
            }
            Err((node_name, e)) => {
                emit(DagEvent::DagFailed { error: e.clone() });
                return Err(format!("node '{}' failed: {}", node_name, e));
            }
        }
    }

    level_results.extend(skipped_results);
    Ok(level_results)
}

// ---------------------------------------------------------------------------
// Internal: rollback completed nodes on failure
// ---------------------------------------------------------------------------

fn rollback_undo_stack_by_idx(
    dag: &mut Dag,
    undo_stack: &mut Vec<(usize, Box<dyn std::any::Any + Send>)>,
    emit: &dyn Fn(DagEvent),
) {
    while let Some((node_idx, undo_ctx)) = undo_stack.pop() {
        let node_name = dag.node_name(node_idx).to_string();
        let t0 = Instant::now();
        match dag.node_mut(node_idx).undo(undo_ctx) {
            Ok(()) => {
                let ms = t0.elapsed().as_millis() as u64;
                emit(DagEvent::NodeLog {
                    node: node_name,
                    node_type: String::new(),
                    level: crate::node::LogLevel::Info,
                    text: format!("undo completed in {}ms", ms),
                });
            }
            Err(e) => {
                emit(DagEvent::NodeLog {
                    node: node_name,
                    node_type: String::new(),
                    level: crate::node::LogLevel::Error,
                    text: format!("undo failed: {}", e),
                });
            }
        }
    }
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

    // 1. Check initial_inputs: values keyed by (node_name, port_name) directly
    let direct_keys: Vec<(String, String)> = port_data.keys()
        .filter(|(n, _)| n == node_name)
        .cloned()
        .collect();
    for key in direct_keys {
        // Only take if no edge produces this port (initial_inputs have no edges)
        let has_edge = edges.iter().any(|e| e.to_node == node_name && e.to_port == key.1);
        if !has_edge {
            if let Some(value) = port_data.remove(&key) {
                inputs.insert(key.1, value);
            }
        }
    }

    // 2. Collect from edges (upstream node outputs)
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

// ===========================================================================
// execute_dag_async — DAG driven by pipe_to, no thread ever blocked
// ===========================================================================

/// Result of a single node task (parallel execution).
struct NodeTaskResult {
    node_idx: usize,
    node_name: String,
    result: Result<(NodeResult, HashMap<String, PortValue>, Box<dyn crate::node::Node>), String>,
}

// Needed because Box<dyn Node> is Send but NodeTaskResult must be too.
unsafe impl Send for NodeTaskResult {}

/// Messages for the DagExecutor actor.
enum DagExecMsg {
    /// Begin DAG execution.
    Start,
    /// A level completed — all node tasks finished.
    LevelDone {
        level_idx: usize,
        results: Vec<NodeTaskResult>,
    },
}

/// Ephemeral actor that drives a DAG level-by-level via pipe_to.
///
/// Spawned by `execute_dag_async`, self-destructs when the DAG finishes.
/// No thread is ever blocked — parallel nodes are submit_task'd and
/// results are collected via collect_replies_to.
struct DagExecutor<R: Send + 'static> {
    dag: crate::dag::Dag,
    levels: Vec<Vec<usize>>,
    port_data: HashMap<(String, String), PortValue>,
    consumer_counts: HashMap<(String, String), usize>,
    node_results: Vec<(String, NodeResult)>,
    undo_stack: Vec<(usize, Box<dyn std::any::Any + Send>)>,
    /// Where to deliver the final result.
    target: crate::mailbox::ActorRef<R>,
    map: Option<Box<dyn FnOnce(Result<DagResult, String>) -> R + Send>>,
    self_ref: Option<crate::mailbox::ActorRef<DagExecMsg>>,
    dag_start: Instant,
}

impl<R: Send + 'static> crate::Actor for DagExecutor<R> {
    type Msg = DagExecMsg;

    fn name(&self) -> &'static str { "dag_executor" }

    fn priority(&self) -> crate::Priority { crate::Priority::High }

    fn on_start(&mut self, self_ref: crate::mailbox::ActorRef<DagExecMsg>) {
        self.self_ref = Some(self_ref.clone());
        let _ = self_ref.send(DagExecMsg::Start);
    }

    fn handle(&mut self, msg: DagExecMsg, _ctx: &crate::ActorContext) -> crate::ActorStatus {
        match msg {
            DagExecMsg::Start => {
                if self.levels.is_empty() {
                    self.finish(Ok(()));
                    return crate::ActorStatus::Stop;
                }
                self.schedule_level(0);
            }
            DagExecMsg::LevelDone { level_idx, results } => {
                // Process results for this level.
                if let Err(e) = self.process_level_results(level_idx, results) {
                    self.rollback();
                    self.finish(Err(e));
                    return crate::ActorStatus::Stop;
                }

                // Next level or finish.
                let next = level_idx + 1;
                if next < self.levels.len() {
                    self.schedule_level(next);
                } else {
                    self.finish(Ok(()));
                    return crate::ActorStatus::Stop;
                }
            }
        }
        crate::ActorStatus::Continue
    }
}

impl<R: Send + 'static> DagExecutor<R> {
    /// Schedule all nodes in a level as tasks, collect results via collect_replies_to.
    fn schedule_level(&mut self, level_idx: usize) {
        let level = &self.levels[level_idx];
        let edges = self.dag.edges().to_vec();
        let scheduler = global_scheduler();
        let self_ref = self.self_ref.as_ref().unwrap();

        let mut task_rxs: Vec<crate::reply::ReplyReceiver<NodeTaskResult>> = Vec::new();
        let mut immediate_results: Vec<NodeTaskResult> = Vec::new();

        for &node_idx in level {
            let node_name = self.dag.node_name(node_idx).to_string();

            // Check triggers — skip nodes whose required trigger is unsatisfied.
            let trigger_unsatisfied = {
                let node_inputs = self.dag.node_mut(node_idx).inputs();
                node_inputs.iter().any(|port| {
                    port.required
                        && port.port_type == crate::port::PortType::Trigger
                        && !edges.iter().any(|e| {
                            e.to_node == node_name
                                && e.to_port == port.name
                                && self.port_data.contains_key(&(
                                    e.from_node.clone(),
                                    e.from_port.clone(),
                                ))
                        })
                })
            };

            if trigger_unsatisfied {
                immediate_results.push(NodeTaskResult {
                    node_idx,
                    node_name,
                    result: Ok((
                        NodeResult {
                            duration_ms: 0,
                            metrics: vec![("skipped".to_string(), 1.0)],
                            logs: vec![],
                        },
                        HashMap::new(),
                        // Skipped node — take it out and put it back immediately.
                        // We need the Box<dyn Node> in the result for uniform handling.
                        unsafe {
                            let entry = &mut self.dag.nodes_mut()[node_idx];
                            let ptr = &mut entry.node as *mut Box<dyn crate::node::Node>;
                            std::ptr::read(ptr)
                        },
                    )),
                });
                continue;
            }

            // Collect inputs for this node.
            let inputs = collect_inputs(
                &node_name,
                &edges,
                &mut self.port_data,
                &mut self.consumer_counts,
            );

            // Take node out of the DAG (like execute_level_parallel).
            let node_box = unsafe {
                let entry = &mut self.dag.nodes_mut()[node_idx];
                let ptr = &mut entry.node as *mut Box<dyn crate::node::Node>;
                std::ptr::read(ptr)
            };

            let services = self.dag.services.clone();

            // Submit as task — runs on a pool thread.
            let rx = scheduler.submit_task(crate::Priority::High, move || {
                let start = Instant::now();
                let mut ctx = crate::node::NodeContext::new(inputs);
                if let Some(s) = services {
                    ctx = ctx.with_services(s);
                }
                let mut node = node_box;
                match node.execute(&mut ctx) {
                    Ok(()) => {
                        let duration_ms = start.elapsed().as_millis() as u64;
                        let outputs = ctx.take_outputs();
                        let metrics = ctx.metrics().to_vec();
                        let logs = ctx.logs().to_vec();
                        NodeTaskResult {
                            node_idx,
                            node_name,
                            result: Ok((
                                NodeResult { duration_ms, metrics, logs },
                                outputs,
                                node,
                            )),
                        }
                    }
                    Err(e) => NodeTaskResult {
                        node_idx,
                        node_name: node_name.clone(),
                        result: Err(format!("node '{}' failed: {}", node_name, e)),
                    },
                }
            });

            task_rxs.push(rx);
        }

        if task_rxs.is_empty() {
            // All nodes were skipped or level is empty — advance immediately.
            let _ = self_ref.send(DagExecMsg::LevelDone {
                level_idx,
                results: immediate_results,
            });
            return;
        }

        // Collect all task results via collect_replies_to.
        // When the last task finishes, LevelDone is sent to this actor.
        let immediate = immediate_results;
        crate::reply::collect_replies_to(
            task_rxs,
            self_ref,
            &format!("dag_level_{level_idx}"),
            move |mut task_results| {
                // Prepend skipped nodes (immediate_results) to maintain order.
                let mut all = immediate;
                all.append(&mut task_results);
                DagExecMsg::LevelDone {
                    level_idx,
                    results: all,
                }
            },
        );
    }

    /// Process results from a completed level. Returns Err if any node failed.
    fn process_level_results(
        &mut self,
        _level_idx: usize,
        results: Vec<NodeTaskResult>,
    ) -> Result<(), String> {
        let bus = dag_event_bus();

        for ntr in results {
            match ntr.result {
                Ok((nr, outputs, node_box)) => {
                    // Put node back in the DAG.
                    let entry = &mut self.dag.nodes_mut()[ntr.node_idx];
                    unsafe {
                        let ptr = &mut entry.node as *mut Box<dyn crate::node::Node>;
                        std::ptr::write(ptr, node_box);
                    }

                    // Undo context.
                    if self.dag.node_mut(ntr.node_idx).can_undo() {
                        if let Some(undo_ctx) = self.dag.node_mut(ntr.node_idx).undo_context() {
                            self.undo_stack.push((ntr.node_idx, undo_ctx));
                        }
                    }

                    // Store outputs in port_data.
                    for (port_name, value) in outputs {
                        if self.dag.taps.is_active() {
                            for edge in self.dag.edges() {
                                if edge.from_node == ntr.node_name && edge.from_port == port_name {
                                    self.dag.taps.check_and_emit(edge, &value);
                                }
                            }
                        }
                        self.port_data
                            .insert((ntr.node_name.clone(), port_name), value);
                    }

                    // Emit logs.
                    for (level, text) in &nr.logs {
                        bus.emit(DagEvent::NodeLog {
                            node: ntr.node_name.clone(),
                            node_type: String::new(),
                            level: *level,
                            text: text.clone(),
                        });
                    }

                    bus.emit(DagEvent::NodeCompleted {
                        node: ntr.node_name.clone(),
                        duration_ms: nr.duration_ms,
                        metrics: nr.metrics.clone(),
                    });

                    self.node_results.push((ntr.node_name, nr));
                }
                Err(e) => {
                    bus.emit(DagEvent::NodeFailed {
                        node: ntr.node_name.clone(),
                        error: e.clone(),
                        duration_ms: 0,
                    });
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Rollback undo stack in reverse order.
    fn rollback(&mut self) {
        let bus = dag_event_bus();
        while let Some((node_idx, undo_ctx)) = self.undo_stack.pop() {
            let node_name = self.dag.node_name(node_idx).to_string();
            if let Err(e) = self.dag.node_mut(node_idx).undo(undo_ctx) {
                bus.emit(DagEvent::NodeFailed {
                    node: format!("undo:{node_name}"),
                    error: e,
                    duration_ms: 0,
                });
            }
        }
    }

    /// Build final DagResult and deliver to target.
    fn finish(&mut self, exec_result: Result<(), String>) {
        let bus = dag_event_bus();
        let total_ms = self.dag_start.elapsed().as_millis() as u64;

        let result = match exec_result {
            Ok(()) => {
                bus.emit(DagEvent::DagCompleted {
                    total_ms,
                    node_count: self.node_results.len(),
                });
                Ok(DagResult {
                    duration_ms: total_ms,
                    node_results: std::mem::take(&mut self.node_results),
                    outputs: std::mem::take(&mut self.port_data),
                })
            }
            Err(e) => {
                bus.emit(DagEvent::DagFailed { error: e.clone() });
                Err(e)
            }
        };

        if let Some(map) = self.map.take() {
            let _ = self.target.send(map(result));
        }
    }
}

/// Execute a DAG asynchronously. Result delivered as a message to target.
///
/// Spawns an ephemeral DagExecutor actor that processes the DAG level by level.
/// Parallel nodes within a level are submitted as tasks and collected via
/// `collect_replies_to`. **No thread is ever blocked.**
///
/// When the DAG completes (success or error), `map(result)` constructs
/// a message sent to `target`.
///
/// ```ignore
/// execute_dag_async(dag, &self.self_ref, "commit_dag", |result| {
///     ShardMsg::DagDone { result }
/// });
/// return ActorStatus::Continue;
/// ```
pub fn execute_dag_async<R: Send + 'static>(
    mut dag: crate::dag::Dag,
    target: &crate::mailbox::ActorRef<R>,
    _label: &str,
    map: impl FnOnce(Result<DagResult, String>) -> R + Send + 'static,
) {
    let levels = match dag.topological_levels() {
        Ok(l) => l,
        Err(e) => {
            let _ = target.send(map(Err(e)));
            return;
        }
    };

    // Pre-compute consumer counts.
    let mut consumer_counts: HashMap<(String, String), usize> = HashMap::new();
    for edge in dag.edges() {
        *consumer_counts
            .entry((edge.from_node.clone(), edge.from_port.clone()))
            .or_insert(0) += 1;
    }

    let port_data = std::mem::take(&mut dag.initial_inputs);

    let executor = DagExecutor {
        dag,
        levels,
        port_data,
        consumer_counts,
        node_results: Vec::new(),
        undo_stack: Vec::new(),
        target: target.clone(),
        map: Some(Box::new(map)),
        self_ref: None,
        dag_start: Instant::now(),
    };

    let scheduler = global_scheduler();
    let (mb, mut ar) = crate::mailbox::mailbox::<DagExecMsg>(64);
    scheduler.spawn(executor, mb, &mut ar, 64);
    // on_start sends Start message — execution begins automatically.
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

    #[test]
    fn node_log_events_emitted() {
        struct LoggyNode;
        impl Node for LoggyNode {
            fn node_type(&self) -> &'static str { "loggy" }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.info("starting work");
                ctx.warn("something odd");
                ctx.metric("items", 42.0);
                Ok(())
            }
        }

        let rx = subscribe_dag_events();
        let mut dag = Dag::new();
        dag.add_node("loggy", LoggyNode);
        execute_dag(&mut dag, None).unwrap();

        let mut events = Vec::new();
        while let Some(evt) = rx.try_recv() {
            events.push(evt);
        }

        let log_events: Vec<_> = events.iter().filter(|e| matches!(e, DagEvent::NodeLog { .. })).collect();
        assert_eq!(log_events.len(), 2);
        assert!(matches!(&log_events[0], DagEvent::NodeLog { level: LogLevel::Info, text, .. } if text == "starting work"));
        assert!(matches!(&log_events[1], DagEvent::NodeLog { level: LogLevel::Warn, text, .. } if text == "something odd"));
    }

    #[test]
    fn edge_tap_captures_data() {
        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 77 });
        dag.add_node("sink", CollectNode { received: 0 });
        dag.connect("source", "out", "sink", "in").unwrap();

        let tap_rx = dag.tap("source", "out", "sink", "in");

        execute_dag(&mut dag, None).unwrap();

        let evt = tap_rx.try_recv().expect("should have captured edge data");
        assert_eq!(evt.from_node, "source");
        assert_eq!(evt.to_node, "sink");
        assert_eq!(*evt.value.downcast::<i32>().unwrap(), 77);
        assert!(tap_rx.try_recv().is_none()); // only one edge
    }

    #[test]
    fn tap_all_captures_everything() {
        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 1 });
        dag.add_node("b", DoubleNode);
        dag.add_node("c", CollectNode { received: 0 });
        dag.connect("a", "out", "b", "in").unwrap();
        dag.connect("b", "out", "c", "in").unwrap();

        let tap_rx = dag.tap_all();

        execute_dag(&mut dag, None).unwrap();

        let mut taps = Vec::new();
        while let Some(evt) = tap_rx.try_recv() {
            taps.push(evt);
        }
        // a→b and b→c = 2 edges
        assert_eq!(taps.len(), 2);
        assert_eq!(taps[0].from_node, "a");
        assert_eq!(taps[1].from_node, "b");
    }

    #[test]
    fn tap_inactive_zero_cost() {
        // No taps — should not slow down
        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 1 });
        dag.add_node("b", CollectNode { received: 0 });
        dag.connect("a", "out", "b", "in").unwrap();

        execute_dag(&mut dag, None).unwrap();
    }

    #[test]
    fn display_summary() {
        struct WorkNode;
        impl Node for WorkNode {
            fn node_type(&self) -> &'static str { "work" }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.metric("docs", 100.0);
                ctx.metric("duration_ms", 42.5);
                ctx.info("done");
                Ok(())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("worker", WorkNode);
        let result = execute_dag(&mut dag, None).unwrap();

        let summary = result.display_summary();
        assert!(summary.contains("DAG completed"));
        assert!(summary.contains("worker"));
        assert!(summary.contains("docs=100"));
        assert!(summary.contains("duration_ms=42.5"));
        assert!(summary.contains("[INF] done"));
    }

    #[test]
    fn total_metric_aggregation() {
        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 1 });
        dag.add_node("b", EmitNode { value: 2 });

        let result = execute_dag(&mut dag, None).unwrap();
        let total = result.total("emitted");
        assert_eq!(total, 3.0); // 1 + 2
    }

    #[test]
    fn rollback_on_failure() {
        use std::sync::Mutex;

        let undo_log = Arc::new(Mutex::new(Vec::<String>::new()));

        struct UndoableNode {
            name: String,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Node for UndoableNode {
            fn node_type(&self) -> &'static str { "undoable" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::optional("trigger", PortType::Trigger)]
            }
            fn outputs(&self) -> Vec<PortDef> {
                vec![PortDef::trigger("done")]
            }
            fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
                ctx.trigger("done");
                Ok(())
            }
            fn can_undo(&self) -> bool { true }
            fn undo_context(&self) -> Option<Box<dyn std::any::Any + Send>> {
                Some(Box::new(self.name.clone()))
            }
            fn undo(&mut self, ctx: Box<dyn std::any::Any + Send>) -> Result<(), String> {
                let name = ctx.downcast::<String>().map_err(|_| "bad type")?;
                self.log.lock().unwrap().push(format!("undo:{}", name));
                Ok(())
            }
        }

        struct FailTrigger;
        impl Node for FailTrigger {
            fn node_type(&self) -> &'static str { "fail_trigger" }
            fn inputs(&self) -> Vec<PortDef> {
                vec![PortDef::trigger("trigger")]
            }
            fn execute(&mut self, _ctx: &mut NodeContext) -> Result<(), String> {
                Err("boom".to_string())
            }
        }

        let mut dag = Dag::new();
        dag.add_node("step1", UndoableNode {
            name: "step1".into(), log: undo_log.clone(),
        });
        dag.add_node("step2", UndoableNode {
            name: "step2".into(), log: undo_log.clone(),
        });
        dag.add_node("fail", FailTrigger);

        dag.connect("step1", "done", "step2", "trigger").unwrap();
        dag.connect("step2", "done", "fail", "trigger").unwrap();

        let err = execute_dag(&mut dag, None).unwrap_err();
        assert!(err.contains("boom"));

        // Undo should have been called in reverse order: step2 then step1
        let log = undo_log.lock().unwrap();
        assert_eq!(*log, vec!["undo:step2", "undo:step1"]);
    }

    #[test]
    fn no_rollback_when_no_undo() {
        // Nodes without can_undo → no rollback attempt, no panic
        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 1 });
        dag.add_node("fail", FailNode);
        dag.connect("source", "out", "fail", "in").unwrap();

        let err = execute_dag(&mut dag, None).unwrap_err();
        assert!(err.contains("intentional failure"));
    }

    #[test]
    fn checkpoint_records_progress() {
        use crate::checkpoint::{MemoryCheckpointStore, CheckpointStatus};

        let store = MemoryCheckpointStore::new();

        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 1 });
        dag.add_node("b", CollectNode { received: 0 });
        dag.connect("a", "out", "b", "in").unwrap();

        let result = execute_dag_with_checkpoint(&mut dag, "test1", &store, None).unwrap();
        assert_eq!(result.node_results.len(), 2);

        let cp = store.load("test1").unwrap();
        assert_eq!(cp.completed_nodes, vec!["a", "b"]);
        assert_eq!(cp.status, CheckpointStatus::Completed);
    }

    #[test]
    fn checkpoint_records_failure() {
        use crate::checkpoint::{MemoryCheckpointStore, CheckpointStatus};

        let store = MemoryCheckpointStore::new();

        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 1 });
        dag.add_node("fail", FailNode);
        dag.connect("source", "out", "fail", "in").unwrap();

        let err = execute_dag_with_checkpoint(&mut dag, "test2", &store, None).unwrap_err();
        assert!(err.contains("intentional"));

        let cp = store.load("test2").unwrap();
        assert_eq!(cp.completed_nodes, vec!["source"]);
        assert_eq!(cp.status, CheckpointStatus::Failed);
        assert!(cp.failed_node.is_some());
    }

    #[test]
    fn checkpoint_skips_completed_nodes() {
        use crate::checkpoint::MemoryCheckpointStore;

        let store = MemoryCheckpointStore::new();
        // Simulate prior run that completed "a"
        store.save_node_completed("test3", "a", "emit");

        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 1 });
        dag.add_node("b", EmitNode { value: 2 });

        let result = execute_dag_with_checkpoint(&mut dag, "test3", &store, None).unwrap();
        let a = result.get("a").unwrap();
        assert_eq!(a.metrics[0], ("skipped".to_string(), 1.0));
        let b = result.get("b").unwrap();
        assert_eq!(b.metrics[0], ("emitted".to_string(), 2.0));
    }

    #[test]
    fn display_progress_ascii() {
        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 5 });
        dag.add_node("double", DoubleNode);
        dag.add_node("sink", CollectNode { received: 0 });
        dag.connect("source", "out", "double", "in").unwrap();
        dag.connect("double", "out", "sink", "in").unwrap();

        // Before execution
        let tree = display_progress(&dag, None);
        assert!(tree.contains("[ ] source"));
        assert!(tree.contains("[ ] double"));
        assert!(tree.contains("[ ] sink"));

        // After execution
        let result = execute_dag(&mut dag, None).unwrap();
        let tree = display_progress(&dag, Some(&result));
        assert!(tree.contains("[✓] source"));
        assert!(tree.contains("[✓] double"));
        assert!(tree.contains("[✓] sink"));
        assert!(tree.contains("received=10"));
    }

    #[test]
    fn display_progress_parallel() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut dag = Dag::new();
        dag.add_node("a", CounterNode { counter: counter.clone() });
        dag.add_node("b", CounterNode { counter: counter.clone() });

        let result = execute_dag(&mut dag, None).unwrap();
        let tree = display_progress(&dag, Some(&result));
        assert!(tree.contains("parallel"));
        assert!(tree.contains("[✓] a"));
        assert!(tree.contains("[✓] b"));
    }

    // -------------------------------------------------------------------
    // execute_dag_async tests
    // -------------------------------------------------------------------

    #[test]
    fn async_dag_linear() {
        // Linear DAG: emit → double → collect
        let scheduler = crate::Scheduler::new(2);
        let _handle = scheduler.start();

        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let result_store = Arc::new(std::sync::Mutex::new(None::<Result<DagResult, String>>));
        let result_store2 = result_store.clone();

        // Target actor receives the DagResult.
        struct ResultSink {
            done: Arc<AtomicBool>,
            store: Arc<std::sync::Mutex<Option<Result<DagResult, String>>>>,
        }
        enum SinkMsg { DagDone(Result<DagResult, String>) }
        impl crate::Actor for ResultSink {
            type Msg = SinkMsg;
            fn name(&self) -> &'static str { "sink" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: SinkMsg, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg {
                    SinkMsg::DagDone(r) => {
                        *self.store.lock().unwrap() = Some(r);
                        self.done.store(true, Ordering::Release);
                    }
                }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<SinkMsg>(64);
        scheduler.spawn(ResultSink { done: done2, store: result_store2 }, mb, &mut ar, 64);

        let mut dag = Dag::new();
        dag.add_node("emit", EmitNode { value: 7 });
        dag.add_node("double", DoubleNode);
        dag.add_node("collect", CollectNode { received: 0 });
        dag.connect("emit", "out", "double", "in").unwrap();
        dag.connect("double", "out", "collect", "in").unwrap();

        execute_dag_async(dag, &ar, "test_linear", |r| SinkMsg::DagDone(r));

        // Wait for result.
        for _ in 0..100 {
            if done.load(Ordering::Acquire) { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done.load(Ordering::Acquire), "dag should complete");

        let result = result_store.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(result.node_results.len(), 3);
        let collect_r = result.get("collect").unwrap();
        assert_eq!(collect_r.metrics[0], ("received".to_string(), 14.0)); // 7*2
    }

    #[test]
    fn async_dag_parallel_level() {
        // Two independent nodes (same level), then a merge.
        let scheduler = crate::Scheduler::new(4);
        let _handle = scheduler.start();

        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let result_store = Arc::new(std::sync::Mutex::new(None::<Result<DagResult, String>>));
        let result_store2 = result_store.clone();

        struct Sink {
            done: Arc<AtomicBool>,
            store: Arc<std::sync::Mutex<Option<Result<DagResult, String>>>>,
        }
        enum SM { Done(Result<DagResult, String>) }
        impl crate::Actor for Sink {
            type Msg = SM;
            fn name(&self) -> &'static str { "sink" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: SM, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg { SM::Done(r) => { *self.store.lock().unwrap() = Some(r); self.done.store(true, Ordering::Release); } }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<SM>(64);
        scheduler.spawn(Sink { done: done2, store: result_store2 }, mb, &mut ar, 64);

        let mut dag = Dag::new();
        dag.add_node("a", EmitNode { value: 10 });
        dag.add_node("b", EmitNode { value: 20 });

        execute_dag_async(dag, &ar, "test_parallel", |r| SM::Done(r));

        for _ in 0..100 {
            if done.load(Ordering::Acquire) { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done.load(Ordering::Acquire));
        let result = result_store.lock().unwrap().take().unwrap().unwrap();
        assert_eq!(result.node_results.len(), 2);
        let total = result.total("emitted");
        assert_eq!(total, 30.0);
    }

    #[test]
    fn async_dag_error_rollback() {
        use std::sync::Mutex as StdMutex;

        let scheduler = crate::Scheduler::new(2);
        let _handle = scheduler.start();

        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let result_store = Arc::new(StdMutex::new(None::<Result<DagResult, String>>));
        let result_store2 = result_store.clone();

        struct Sink {
            done: Arc<AtomicBool>,
            store: Arc<StdMutex<Option<Result<DagResult, String>>>>,
        }
        enum SM { Done(Result<DagResult, String>) }
        impl crate::Actor for Sink {
            type Msg = SM;
            fn name(&self) -> &'static str { "sink" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: SM, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg { SM::Done(r) => { *self.store.lock().unwrap() = Some(r); self.done.store(true, Ordering::Release); } }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<SM>(64);
        scheduler.spawn(Sink { done: done2, store: result_store2 }, mb, &mut ar, 64);

        let mut dag = Dag::new();
        dag.add_node("source", EmitNode { value: 1 });
        dag.add_node("fail", FailNode);
        dag.connect("source", "out", "fail", "in").unwrap();

        execute_dag_async(dag, &ar, "test_fail", |r| SM::Done(r));

        for _ in 0..100 {
            if done.load(Ordering::Acquire) { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done.load(Ordering::Acquire));
        let result = result_store.lock().unwrap().take().unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("intentional failure"));
    }
}
