use std::sync::atomic::{AtomicUsize, Ordering};

use crate::mailbox::{mailbox, ActorRef};
use crate::reply::{reply, Reply};
use crate::scheduler::global_scheduler;
use crate::{Actor, Mailbox};

// ---------------------------------------------------------------------------
// Pool — N identical actors with dispatch strategies
// ---------------------------------------------------------------------------

/// A pool of N identical actors with configurable dispatch.
///
/// Abstracts the common pattern of spawning multiple identical workers
/// and distributing work among them (round-robin, key-routed, or broadcast).
pub struct Pool<M: Send + 'static> {
    workers: Vec<ActorRef<M>>,
    next: AtomicUsize,
}

impl<M: Send + 'static> Clone for Pool<M> {
    fn clone(&self) -> Self {
        Pool {
            workers: self.workers.clone(),
            next: AtomicUsize::new(self.next.load(Ordering::Relaxed)),
        }
    }
}

impl<M: Send + 'static> Pool<M> {
    /// Spawn `count` actors using the factory function.
    /// Each actor gets its index (0..count) for identification.
    pub fn spawn<A>(
        count: usize,
        capacity: usize,
        make_actor: impl Fn(usize) -> A,
    ) -> Self
    where
        A: Actor<Msg = M>,
    {
        assert!(count >= 1, "pool needs at least 1 worker");
        let scheduler = global_scheduler();
        let mut workers = Vec::with_capacity(count);

        for i in 0..count {
            let actor = make_actor(i);
            let (mb, mut ar) = mailbox::<M>(capacity);
            scheduler.spawn(actor, mb, &mut ar, capacity);
            workers.push(ar);
        }

        Pool {
            workers,
            next: AtomicUsize::new(0),
        }
    }

    /// Number of workers in the pool.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Send a message to the next worker (round-robin).
    pub fn send(&self, msg: M) -> Result<(), String> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].send(msg).map_err(|_| "worker disconnected".to_string())
    }

    /// Send a message to a specific worker by key (e.g. shard_id).
    pub fn send_to(&self, key: usize, msg: M) -> Result<(), String> {
        let idx = key % self.workers.len();
        self.workers[idx].send(msg).map_err(|_| "worker disconnected".to_string())
    }

    /// Get a reference to a specific worker.
    pub fn worker(&self, index: usize) -> &ActorRef<M> {
        &self.workers[index % self.workers.len()]
    }

    /// Broadcast a message to all workers.
    pub fn broadcast(&self, make_msg: impl Fn() -> M) -> Result<(), String> {
        for worker in &self.workers {
            worker.send(make_msg()).map_err(|_| "worker disconnected".to_string())?;
        }
        Ok(())
    }

    /// Send a request to one worker (round-robin), wait for the reply.
    pub fn request<R, F>(&self, make_msg: F, label: &str) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(Reply<R>) -> M,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        self.workers[idx].request(make_msg, label)
    }

    /// Send a request to a specific worker by key, wait for the reply.
    pub fn request_to<R, F>(&self, key: usize, make_msg: F, label: &str) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(Reply<R>) -> M,
    {
        let idx = key % self.workers.len();
        self.workers[idx].request(make_msg, label)
    }

    /// Scatter a request to ALL workers in parallel, collect all replies.
    ///
    /// Each worker receives a message (with its own Reply). All replies are
    /// waited on cooperatively. Returns results in worker order (0..N).
    pub fn scatter<R, F>(&self, make_msg: F, label: &str) -> Vec<R>
    where
        R: Send + 'static,
        F: Fn(Reply<R>) -> M,
    {
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.workers.len());

        for worker in &self.workers {
            let (tx, rx) = reply::<R>();
            let _ = worker.send(make_msg(tx));
            receivers.push(rx);
        }

        receivers.into_iter()
            .map(|rx| rx.wait_cooperative_named(label, || scheduler.run_one_step()))
            .collect()
    }

    /// Drain: wait until all workers have processed their pending messages.
    ///
    /// Sends a "ping" reply to each worker and waits for all responses.
    /// When all respond, their mailboxes were empty at that point.
    pub fn drain(&self, label: &str)
    where
        M: From<DrainMsg>,
    {
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.workers.len());

        for worker in &self.workers {
            let (tx, rx) = reply::<()>();
            let _ = worker.send(DrainMsg(tx).into());
            receivers.push(rx);
        }

        for (i, rx) in receivers.into_iter().enumerate() {
            rx.wait_cooperative_named(
                &format!("{}_worker_{}", label, i),
                || scheduler.run_one_step(),
            );
        }
    }

    /// Send a shutdown message to all workers and wait for them to stop.
    ///
    /// Like `drain`, requires `M: From<ShutdownMsg>`. Each worker receives
    /// the shutdown message after processing its pending work (FIFO).
    pub fn shutdown(&self, label: &str)
    where
        M: From<ShutdownMsg>,
    {
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.workers.len());

        for worker in &self.workers {
            let (tx, rx) = reply::<()>();
            let _ = worker.send(ShutdownMsg(tx).into());
            receivers.push(rx);
        }

        for (i, rx) in receivers.into_iter().enumerate() {
            rx.wait_cooperative_named(
                &format!("{}_worker_{}", label, i),
                || scheduler.run_one_step(),
            );
        }
    }

    /// Wrap existing ActorRefs into a Pool (useful for migration).
    pub fn from_refs(refs: Vec<ActorRef<M>>) -> Self {
        assert!(!refs.is_empty(), "pool needs at least 1 worker");
        Pool {
            workers: refs,
            next: AtomicUsize::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Drainable impl for Pool
// ---------------------------------------------------------------------------

impl<M: Send + 'static> crate::scope::Drainable for Pool<M>
where
    M: From<DrainMsg>,
{
    fn drain(&self, label: &str) {
        Pool::drain(self, label);
    }
}

// ---------------------------------------------------------------------------
// DrainMsg / ShutdownMsg — protocol messages for pool lifecycle
// ---------------------------------------------------------------------------

/// Message sent by `Pool::drain()`. The actor should reply immediately.
/// Include this variant in your actor's message enum:
///
/// ```ignore
/// enum MyMsg {
///     Work(Data),
///     Drain(DrainMsg),
/// }
/// impl From<DrainMsg> for MyMsg {
///     fn from(d: DrainMsg) -> Self { MyMsg::Drain(d) }
/// }
/// ```
pub struct DrainMsg(pub Reply<()>);

impl DrainMsg {
    /// Acknowledge the drain (reply to the waiter).
    pub fn ack(self) {
        self.0.send(());
    }
}

/// Message sent by `Pool::shutdown()`. The actor should finish current work,
/// reply, then return `ActorStatus::Stop`.
///
/// ```ignore
/// enum MyMsg {
///     Shutdown(ShutdownMsg),
/// }
/// impl From<ShutdownMsg> for MyMsg {
///     fn from(s: ShutdownMsg) -> Self { MyMsg::Shutdown(s) }
/// }
/// // In handler:
/// MyMsg::Shutdown(s) => { s.ack(); ActorStatus::Stop }
/// ```
pub struct ShutdownMsg(pub Reply<()>);

impl ShutdownMsg {
    /// Acknowledge shutdown (reply to the waiter). Call before returning Stop.
    pub fn ack(self) {
        self.0.send(());
    }
}

// ---------------------------------------------------------------------------
// DrainableRef — Drainable wrapper for a single ActorRef
// ---------------------------------------------------------------------------

/// Wraps a single `ActorRef<M>` to make it `Drainable`.
///
/// Useful when a Scope contains both Pools and individual actors.
///
/// ```ignore
/// let mut scope = Scope::new("commit");
/// scope.add("workers", pool);  // Pool implements Drainable
/// scope.add("updater", DrainableRef::new(updater_ref));  // single actor
/// ```
pub struct DrainableRef<M: Send + 'static> {
    actor_ref: ActorRef<M>,
}

impl<M: Send + 'static> DrainableRef<M> {
    pub fn new(actor_ref: ActorRef<M>) -> Self {
        Self { actor_ref }
    }
}

impl<M: Send + 'static> crate::scope::Drainable for DrainableRef<M>
where
    M: From<DrainMsg>,
{
    fn drain(&self, label: &str) {
        let scheduler = global_scheduler();
        let (tx, rx) = reply::<()>();
        let _ = self.actor_ref.send(DrainMsg(tx).into());
        rx.wait_cooperative_named(label, || scheduler.run_one_step());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Drainable;
    use crate::{ActorStatus, Priority};
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    // -- test actor --

    struct Worker {
        id: usize,
        count: Arc<AtomicU32>,
    }

    enum WorkerMsg {
        Inc,
        GetCount(Reply<u32>),
        GetId(Reply<usize>),
        Drain(DrainMsg),
        Shutdown(ShutdownMsg),
    }

    impl From<DrainMsg> for WorkerMsg {
        fn from(d: DrainMsg) -> Self { WorkerMsg::Drain(d) }
    }

    impl From<ShutdownMsg> for WorkerMsg {
        fn from(s: ShutdownMsg) -> Self { WorkerMsg::Shutdown(s) }
    }

    impl Actor for Worker {
        type Msg = WorkerMsg;
        fn name(&self) -> &'static str { "worker" }
        fn handle(&mut self, msg: WorkerMsg) -> ActorStatus {
            match msg {
                WorkerMsg::Inc => {
                    self.count.fetch_add(1, Ordering::Relaxed);
                    ActorStatus::Continue
                }
                WorkerMsg::GetCount(r) => {
                    r.send(self.count.load(Ordering::Relaxed));
                    ActorStatus::Continue
                }
                WorkerMsg::GetId(r) => {
                    r.send(self.id);
                    ActorStatus::Continue
                }
                WorkerMsg::Drain(d) => {
                    d.ack();
                    ActorStatus::Continue
                }
                WorkerMsg::Shutdown(s) => {
                    s.ack();
                    ActorStatus::Stop
                }
            }
        }
        fn priority(&self) -> Priority { Priority::Medium }
    }

    fn make_pool(n: usize, count: Arc<AtomicU32>) -> Pool<WorkerMsg> {
        Pool::spawn(n, 256, |i| Worker { id: i, count: count.clone() })
    }

    // -- tests --

    #[test]
    fn test_pool_send_round_robin() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(4, count.clone());

        assert_eq!(pool.len(), 4);

        for _ in 0..100 {
            pool.send(WorkerMsg::Inc).unwrap();
        }

        // Drain to ensure all processed
        pool.drain("test");
        assert_eq!(count.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn test_pool_send_to() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(4, count.clone());

        // Send all to worker 2
        for _ in 0..10 {
            pool.send_to(2, WorkerMsg::Inc).unwrap();
        }

        pool.drain("test");
        assert_eq!(count.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_pool_broadcast() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(4, count.clone());

        pool.broadcast(|| WorkerMsg::Inc).unwrap();
        pool.drain("test");

        // All 4 workers got Inc
        assert_eq!(count.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn test_pool_request() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(4, count.clone());

        for _ in 0..20 {
            pool.send(WorkerMsg::Inc).unwrap();
        }
        pool.drain("test");

        // Shared atomic: all workers increment the same counter
        assert_eq!(count.load(Ordering::Relaxed), 20);

        // Request from one worker
        let c = pool.request(|r| WorkerMsg::GetCount(r), "count").unwrap();
        assert_eq!(c, 20); // shared counter
    }

    #[test]
    fn test_pool_scatter() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(4, count.clone());

        // Scatter GetId to all workers
        let ids = pool.scatter(|r| WorkerMsg::GetId(r), "ids");
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
    }

    #[test]
    fn test_pool_drain() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(2, count.clone());

        for _ in 0..500 {
            pool.send(WorkerMsg::Inc).unwrap();
        }

        pool.drain("drain_test");
        assert_eq!(count.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn test_pool_request_single() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(2, count.clone());

        // Send 5 to worker 0
        for _ in 0..5 {
            pool.send_to(0, WorkerMsg::Inc).unwrap();
        }

        // Request from worker 0 specifically
        let c = pool.request_to(0, |r| WorkerMsg::GetCount(r), "count_0").unwrap();
        assert_eq!(c, 5);
    }

    #[test]
    fn test_pool_shutdown() {
        let count = Arc::new(AtomicU32::new(0));
        let pool = make_pool(3, count.clone());

        for _ in 0..30 {
            pool.send(WorkerMsg::Inc).unwrap();
        }

        pool.shutdown("shutdown_test");
        assert_eq!(count.load(Ordering::Relaxed), 30);
        // After shutdown, sends should fail (actors stopped)
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(pool.send(WorkerMsg::Inc).is_err());
    }

    #[test]
    fn test_pool_from_refs() {
        let count = Arc::new(AtomicU32::new(0));
        let scheduler = global_scheduler();

        let mut refs = Vec::new();
        for i in 0..3 {
            let (mb, mut ar) = mailbox::<WorkerMsg>(64);
            scheduler.spawn(Worker { id: i, count: count.clone() }, mb, &mut ar, 64);
            refs.push(ar);
        }

        let pool = Pool::from_refs(refs);
        assert_eq!(pool.len(), 3);

        for _ in 0..9 {
            pool.send(WorkerMsg::Inc).unwrap();
        }
        pool.drain("from_refs");
        assert_eq!(count.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn test_drainable_ref() {
        let count = Arc::new(AtomicU32::new(0));
        let scheduler = global_scheduler();

        let (mb, mut ar) = mailbox::<WorkerMsg>(64);
        scheduler.spawn(Worker { id: 0, count: count.clone() }, mb, &mut ar, 64);

        for _ in 0..10 {
            ar.send(WorkerMsg::Inc).unwrap();
        }

        let drainable = DrainableRef::new(ar);
        drainable.drain("single_actor");
        assert_eq!(count.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_drainable_ref_in_scope() {
        use crate::scope::Scope;

        let count = Arc::new(AtomicU32::new(0));
        let scheduler = global_scheduler();

        // A pool + a single actor in the same scope
        let pool = make_pool(2, count.clone());
        let (mb, mut ar) = mailbox::<WorkerMsg>(64);
        scheduler.spawn(Worker { id: 99, count: count.clone() }, mb, &mut ar, 64);

        for _ in 0..10 {
            pool.send(WorkerMsg::Inc).unwrap();
        }
        for _ in 0..5 {
            ar.send(WorkerMsg::Inc).unwrap();
        }

        let mut scope = Scope::new("mixed");
        scope.add("pool", pool);
        scope.add("single", DrainableRef::new(ar));

        scope.drain();
        assert_eq!(count.load(Ordering::Relaxed), 15);
    }
}
