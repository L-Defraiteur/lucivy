use crate::dag::Dag;
use crate::runtime::{execute_dag, DagEvent, DagResult};

// ---------------------------------------------------------------------------
// Drainable — trait for anything that can be drained
// ---------------------------------------------------------------------------

/// Trait for components that can drain their pending work.
///
/// Implement this for actors, pools, or any async worker that needs
/// to flush before a synchronization point (e.g. before a DAG commit).
pub trait Drainable: Send + Sync {
    /// Wait until all pending work is processed.
    fn drain(&self, label: &str);
}

// ---------------------------------------------------------------------------
// Scope — lifecycle manager for a group of drainables
// ---------------------------------------------------------------------------

/// Manages a group of actors/pools with ordered drain and DAG execution.
///
/// Stages are drained in registration order (first registered = first drained).
/// This is important for pipelines: drain readers before router before shards.
pub struct Scope {
    name: String,
    stages: Vec<(String, Box<dyn Drainable>)>,
}

impl Scope {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            stages: Vec::new(),
        }
    }

    /// Register a drainable stage. Stages are drained in registration order.
    pub fn add(&mut self, name: &str, stage: impl Drainable + 'static) {
        self.stages.push((name.to_string(), Box::new(stage)));
    }

    /// Drain all stages in registration order.
    pub fn drain(&self) {
        for (stage_name, stage) in &self.stages {
            stage.drain(&format!("{}:{}", self.name, stage_name));
        }
    }

    /// Drain all stages, then execute a DAG.
    ///
    /// This is the "drain → DAG → done" pattern used by commit:
    /// all actors finish their pending work, then the DAG runs
    /// (merges, save, GC, reload) on the same thread pool.
    pub fn execute_dag(
        &self,
        dag: &mut Dag,
        on_event: Option<&dyn Fn(DagEvent)>,
    ) -> Result<DagResult, String> {
        self.drain();
        execute_dag(dag, on_event)
    }

    /// Number of registered stages.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// True if no stages registered.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Node, NodeContext, PortDef};
    use crate::pool::{DrainMsg, Pool};
    use crate::port::{PortType, PortValue};
    use crate::reply::Reply;
    use crate::{Actor, ActorStatus, Priority};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // -- test actor --

    struct CountWorker {
        count: Arc<AtomicU32>,
    }

    enum CountMsg {
        Inc,
        Drain(DrainMsg),
    }

    impl From<DrainMsg> for CountMsg {
        fn from(d: DrainMsg) -> Self { CountMsg::Drain(d) }
    }

    impl Actor for CountWorker {
        type Msg = CountMsg;
        fn name(&self) -> &'static str { "count_worker" }
        fn handle(&mut self, msg: CountMsg) -> ActorStatus {
            match msg {
                CountMsg::Inc => {
                    self.count.fetch_add(1, Ordering::Relaxed);
                    ActorStatus::Continue
                }
                CountMsg::Drain(d) => {
                    d.ack();
                    ActorStatus::Continue
                }
            }
        }
        fn priority(&self) -> Priority { Priority::Medium }
    }

    // -- Drainable for Pool --

    impl<M: Send + 'static> Drainable for Pool<M>
    where
        M: From<DrainMsg>,
    {
        fn drain(&self, label: &str) {
            Pool::drain(self, label);
        }
    }

    // -- test DAG node --

    struct SumNode {
        result: Arc<AtomicU32>,
        add: u32,
    }

    impl Node for SumNode {
        fn node_type(&self) -> &'static str { "sum" }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            self.result.fetch_add(self.add, Ordering::Relaxed);
            ctx.metric("added", self.add as f64);
            Ok(())
        }
    }

    // -- tests --

    #[test]
    fn test_scope_drain_order() {
        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        struct LogDrain {
            name: String,
            log: Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl Drainable for LogDrain {
            fn drain(&self, _label: &str) {
                self.log.lock().unwrap().push(self.name.clone());
            }
        }

        let mut scope = Scope::new("test");
        scope.add("first", LogDrain { name: "A".into(), log: log.clone() });
        scope.add("second", LogDrain { name: "B".into(), log: log.clone() });
        scope.add("third", LogDrain { name: "C".into(), log: log.clone() });

        scope.drain();

        let order = log.lock().unwrap();
        assert_eq!(*order, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_scope_drain_pools() {
        let count = Arc::new(AtomicU32::new(0));

        let pool: Pool<CountMsg> = Pool::spawn(2, 256, |_| CountWorker {
            count: count.clone(),
        });

        for _ in 0..100 {
            pool.send(CountMsg::Inc).unwrap();
        }

        let mut scope = Scope::new("test");
        scope.add("workers", pool);

        scope.drain();
        assert_eq!(count.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn test_scope_execute_dag() {
        let count = Arc::new(AtomicU32::new(0));
        let dag_result = Arc::new(AtomicU32::new(0));

        let pool: Pool<CountMsg> = Pool::spawn(2, 256, |_| CountWorker {
            count: count.clone(),
        });

        // Send work to pool
        for _ in 0..50 {
            pool.send(CountMsg::Inc).unwrap();
        }

        let mut scope = Scope::new("commit");
        scope.add("workers", pool);

        // Build a simple DAG
        let mut dag = Dag::new();
        dag.add_node("sum", SumNode { result: dag_result.clone(), add: 42 });

        // execute_dag drains first, then runs DAG
        let result = scope.execute_dag(&mut dag, None).unwrap();

        // Pool was drained (50 incs processed)
        assert_eq!(count.load(Ordering::Relaxed), 50);
        // DAG ran (added 42)
        assert_eq!(dag_result.load(Ordering::Relaxed), 42);
        // Result has metrics
        assert_eq!(result.get("sum").unwrap().metrics[0].1, 42.0);
    }

    #[test]
    fn test_scope_empty() {
        let scope = Scope::new("empty");
        assert!(scope.is_empty());
        scope.drain(); // no-op, should not panic
    }
}
