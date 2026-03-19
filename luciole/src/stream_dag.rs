use std::collections::HashMap;

use crate::scope::Drainable;

// ---------------------------------------------------------------------------
// StreamDag — observable topology over existing actors
// ---------------------------------------------------------------------------

/// Declares the topology of a streaming pipeline of actors.
///
/// Unlike `Dag` (one-shot execution), `StreamDag` describes how existing
/// actors (spawned separately) are connected. It provides:
/// - Structured drain (in topological order, upstream first)
/// - Display of the pipeline topology
/// - Future: taps on edges between actors
///
/// The actors themselves are owned externally (as Pool, ActorRef, etc.).
/// StreamDag just knows the shape and the drain order.
///
/// ```ignore
/// let readers = Pool::spawn(4, 128, |i| ReaderActor::new(i));
/// let router = /* single actor */;
/// let shards = Pool::spawn(8, 256, |i| ShardActor::new(i));
///
/// let mut pipeline = StreamDag::new("ingestion");
/// pipeline.add_stage("readers", readers, 4);
/// pipeline.add_stage("router", router_drainable, 1);
/// pipeline.add_stage("shards", shards, 8);
/// pipeline.connect("readers", "router");
/// pipeline.connect("router", "shards");
///
/// // Drain in topological order: readers → router → shards
/// pipeline.drain();
///
/// // Display topology
/// println!("{}", pipeline.display());
/// ```
pub struct StreamDag {
    name: String,
    stages: Vec<Stage>,
    edges: Vec<(String, String)>,
}

struct Stage {
    name: String,
    drainable: Box<dyn Drainable>,
    num_workers: usize,
}

impl StreamDag {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            stages: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Add a stage (actor, pool, or any Drainable).
    pub fn add_stage(
        &mut self,
        name: &str,
        drainable: impl Drainable + 'static,
        num_workers: usize,
    ) {
        self.stages.push(Stage {
            name: name.to_string(),
            drainable: Box::new(drainable),
            num_workers,
        });
    }

    /// Declare a connection from one stage to another.
    /// This is for display/observability — the actual channel wiring
    /// is done by the actors themselves.
    pub fn connect(&mut self, from: &str, to: &str) {
        self.edges.push((from.to_string(), to.to_string()));
    }

    /// Drain all stages in topological order.
    /// Upstream stages are drained first so their output channels
    /// flush into downstream stages before those are drained.
    pub fn drain(&self) {
        let order = self.topological_order();
        for name in &order {
            if let Some(stage) = self.stages.iter().find(|s| s.name == *name) {
                stage.drainable.drain(&format!("{}:{}", self.name, name));
            }
        }
    }

    /// Display the pipeline topology as ASCII.
    ///
    /// ```text
    /// ingestion pipeline:
    ///   readers [4 workers]
    ///     ↓
    ///   router [1 worker]
    ///     ↓
    ///   shards [8 workers]
    /// ```
    pub fn display(&self) -> String {
        let order = self.topological_order();
        let mut lines = Vec::new();
        lines.push(format!("{} pipeline:", self.name));

        for (i, name) in order.iter().enumerate() {
            if let Some(stage) = self.stages.iter().find(|s| s.name == *name) {
                let worker_label = if stage.num_workers == 1 {
                    "1 worker".to_string()
                } else {
                    format!("{} workers", stage.num_workers)
                };
                lines.push(format!("  {} [{}]", name, worker_label));

                // Add arrow if not last
                if i < order.len() - 1 {
                    lines.push("    ↓".to_string());
                }
            }
        }

        lines.join("\n")
    }

    /// Number of stages.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Topological sort of stages based on edges.
    fn topological_order(&self) -> Vec<String> {
        let n = self.stages.len();
        if n == 0 {
            return vec![];
        }

        let name_to_idx: HashMap<&str, usize> = self.stages.iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();

        let mut in_degree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![vec![]; n];

        for (from, to) in &self.edges {
            if let (Some(&fi), Some(&ti)) = (name_to_idx.get(from.as_str()), name_to_idx.get(to.as_str())) {
                if !dependents[fi].contains(&ti) {
                    dependents[fi].push(ti);
                    in_degree[ti] += 1;
                }
            }
        }

        // Kahn's algorithm
        let mut result = Vec::with_capacity(n);
        let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();

        while let Some(idx) = queue.pop() {
            result.push(self.stages[idx].name.clone());
            for &dep in &dependents[idx] {
                in_degree[dep] -= 1;
                if in_degree[dep] == 0 {
                    queue.push(dep);
                }
            }
        }

        // Add any remaining stages (disconnected)
        for stage in &self.stages {
            if !result.contains(&stage.name) {
                result.push(stage.name.clone());
            }
        }

        result
    }
}

impl Drainable for StreamDag {
    fn drain(&self, _label: &str) {
        StreamDag::drain(self);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Pool, DrainMsg};
    use crate::{Actor, ActorStatus, Priority};
    use crate::reply::Reply;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    struct CountWorker { count: Arc<AtomicU32> }

    enum CountMsg {
        Inc,
        Drain(DrainMsg),
    }
    impl From<DrainMsg> for CountMsg {
        fn from(d: DrainMsg) -> Self { CountMsg::Drain(d) }
    }
    impl Actor for CountWorker {
        type Msg = CountMsg;
        fn name(&self) -> &'static str { "count" }
        fn handle(&mut self, msg: CountMsg) -> ActorStatus {
            match msg {
                CountMsg::Inc => { self.count.fetch_add(1, Ordering::Relaxed); ActorStatus::Continue }
                CountMsg::Drain(d) => { d.ack(); ActorStatus::Continue }
            }
        }
        fn priority(&self) -> Priority { Priority::Medium }
    }

    #[test]
    fn stream_dag_drain_order() {
        let count_a = Arc::new(AtomicU32::new(0));
        let count_b = Arc::new(AtomicU32::new(0));

        let pool_a: Pool<CountMsg> = Pool::spawn(2, 128, |_| CountWorker { count: count_a.clone() });
        let pool_b: Pool<CountMsg> = Pool::spawn(2, 128, |_| CountWorker { count: count_b.clone() });

        // Send work
        for _ in 0..50 { pool_a.send(CountMsg::Inc).unwrap(); }
        for _ in 0..30 { pool_b.send(CountMsg::Inc).unwrap(); }

        let mut pipeline = StreamDag::new("test");
        pipeline.add_stage("stage_a", pool_a, 2);
        pipeline.add_stage("stage_b", pool_b, 2);
        pipeline.connect("stage_a", "stage_b");

        // Drain in topological order
        pipeline.drain();

        assert_eq!(count_a.load(Ordering::Relaxed), 50);
        assert_eq!(count_b.load(Ordering::Relaxed), 30);
    }

    #[test]
    fn stream_dag_display() {
        // Just test display without real actors
        struct NoDrain;
        impl Drainable for NoDrain {
            fn drain(&self, _label: &str) {}
        }

        let mut pipeline = StreamDag::new("ingestion");
        pipeline.add_stage("readers", NoDrain, 4);
        pipeline.add_stage("router", NoDrain, 1);
        pipeline.add_stage("shards", NoDrain, 8);
        pipeline.connect("readers", "router");
        pipeline.connect("router", "shards");

        let display = pipeline.display();
        assert!(display.contains("readers [4 workers]"));
        assert!(display.contains("router [1 worker]"));
        assert!(display.contains("shards [8 workers]"));
        assert!(display.contains("↓"));
        eprintln!("{}", display);
    }

    #[test]
    fn stream_dag_is_drainable() {
        // StreamDag itself implements Drainable — can be used in Scope
        struct NoDrain;
        impl Drainable for NoDrain {
            fn drain(&self, _label: &str) {}
        }

        let mut pipeline = StreamDag::new("test");
        pipeline.add_stage("a", NoDrain, 1);

        let mut scope = crate::Scope::new("outer");
        scope.add("pipeline", pipeline);
        scope.drain(); // should not panic
    }
}
