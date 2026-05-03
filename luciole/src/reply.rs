use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};

// ---------------------------------------------------------------------------
// ResumeHandle — fires when a reply arrives, re-schedules a suspended actor
// ---------------------------------------------------------------------------

/// Callback that re-schedules a suspended actor when fired.
///
/// Created by `ActorContext::resume_handle()` in the scheduler. Registered
/// on a `ReplyReceiver` via `set_resume()`. When `Reply::send()` is called,
/// the resume callback fires automatically, pushing the actor back into the
/// scheduler's ready queue.
pub struct ResumeHandle {
    inner: Arc<ResumeInner>,
}

struct ResumeInner {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ResumeHandle {
    /// Create a ResumeHandle that calls `f` when fired.
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            inner: Arc::new(ResumeInner {
                callback: Mutex::new(Some(Box::new(f))),
            }),
        }
    }

    /// Fire the resume callback. Idempotent — second call is a no-op.
    pub fn fire(&self) {
        if let Some(f) = self.inner.callback.lock().unwrap().take() {
            f();
        }
    }
}

impl Clone for ResumeHandle {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

// ---------------------------------------------------------------------------
// JoinResume — fires a ResumeHandle when N completions have arrived
// ---------------------------------------------------------------------------

/// Resume handle that fires only when all N sub-handles have been fired.
///
/// Used for scatter-then-resume: an actor sends N messages, creates a
/// JoinResume(N, ctx.resume_handle()), and gives each ReplyReceiver a
/// `one_shot()` handle. The actor returns Suspend. When the last reply
/// arrives, the actual ResumeHandle fires and the actor is re-scheduled.
pub struct JoinResume {
    remaining: std::sync::atomic::AtomicUsize,
    handle: ResumeHandle,
}

impl JoinResume {
    /// Create a JoinResume that expects `count` completions before firing.
    pub fn new(count: usize, handle: ResumeHandle) -> Arc<Self> {
        assert!(count >= 1, "JoinResume needs at least 1 completion");
        Arc::new(Self {
            remaining: std::sync::atomic::AtomicUsize::new(count),
            handle,
        })
    }

    /// Create a per-reply ResumeHandle. Each call to `fire()` decrements
    /// the counter. The last one (remaining → 0) fires the actual handle.
    pub fn one_shot(self: &Arc<Self>) -> ResumeHandle {
        let join = Arc::clone(self);
        ResumeHandle::new(move || {
            if join.remaining.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                join.handle.fire();
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Reply / ReplyReceiver — oneshot channel with optional resume
// ---------------------------------------------------------------------------

/// État interne partagé du oneshot.
struct Inner<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
    /// Optional resume handle — fired when the reply is sent or dropped.
    resume: Mutex<Option<ResumeHandle>>,
    /// WaitGraph edge ID for pipe_to. Cleaned up by Reply::send (normal)
    /// or Reply::drop (sender died). 0 = no edge.
    pipe_edge_id: AtomicU64,
}

struct State<T> {
    value: Option<T>,
    closed: bool,
    /// Optional pipe callback — called by Reply::send() instead of storing
    /// the value. Set by set_pipe(). Protected by the same Mutex as value
    /// to prevent race conditions (check value + set callback is atomic).
    on_send: Option<Box<dyn FnOnce(T) + Send>>,
}

/// Côté acteur : envoie la réponse (oneshot).
pub struct Reply<T> {
    inner: Arc<Inner<T>>,
}

/// Côté appelant : attend la réponse.
pub struct ReplyReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Reply<T> {
    /// Envoie la réponse. Consomme le Reply.
    pub fn send(self, value: T) {
        let mut state = self.inner.state.lock().unwrap();

        // Pipe path: if a pipe callback is registered, deliver the value
        // directly to it (pipe_to / collect_to). Skip the normal path.
        if let Some(pipe) = state.on_send.take() {
            state.closed = true;
            drop(state);
            self.inner.ready.notify_one();
            // Clear pipe edge — we're completing normally.
            // Reply::drop will see 0 and skip unregister (no double-free).
            let edge_id = self.inner.pipe_edge_id.swap(0, AtomicOrdering::AcqRel);
            if edge_id != 0 {
                crate::wait_graph::unregister(edge_id);
            }
            pipe(value);
            return;
        }

        // Normal path: store value, notify waiters, fire resume.
        state.value = Some(value);
        state.closed = true;
        self.inner.ready.notify_one();
        drop(state);
        if let Some(handle) = self.inner.resume.lock().unwrap().take() {
            handle.fire();
        }
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
        // Clean up WaitGraph edge if pipe was set but sender died without
        // calling send(). swap(0) is atomic — no double-free with send().
        let edge_id = self.inner.pipe_edge_id.swap(0, AtomicOrdering::AcqRel);
        if edge_id != 0 {
            crate::wait_graph::unregister(edge_id);
        }

        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.ready.notify_one();
        drop(state);
        // Fire resume even on drop (actor died without replying — the
        // suspended actor should be woken to discover the error).
        if let Some(handle) = self.inner.resume.lock().unwrap().take() {
            handle.fire();
        }
    }
}

impl<T> ReplyReceiver<T> {
    /// Register a ResumeHandle that will fire when the reply arrives.
    /// Used by actors that return `ActorStatus::Suspend` — the handle
    /// re-schedules them in the scheduler when the dependency completes.
    pub fn set_resume(&self, handle: ResumeHandle) {
        *self.inner.resume.lock().unwrap() = Some(handle);
    }

    /// Non-blocking value take. Returns the value if the reply has arrived.
    /// Used after resume to collect the result without blocking.
    pub fn take_value(&self) -> Option<T> {
        self.inner.state.lock().unwrap().value.take()
    }

    /// Returns true if the reply has arrived (value is available).
    pub fn is_ready(&self) -> bool {
        let state = self.inner.state.lock().unwrap();
        state.value.is_some() || state.closed
    }

    /// Attente bloquante (mode multi-thread).
    /// Utilise Mutex + Condvar — compatible ASYNCIFY en WASM.
    pub fn wait_blocking(self) -> T {
        let _guard = crate::wait_graph::WaitGuard::current("(blocking)");
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(value) = state.value.take() {
                return value;
            }
            if state.closed {
                panic!("actor died without replying");
            }
            state = self.inner.ready.wait(state).unwrap();
        }
    }

    /// Blocking wait with periodic timeout. Calls `on_timeout` every `interval`
    /// while waiting. No thread spawning — safe for WASM.
    ///
    /// `on_timeout(elapsed_secs)` is called each time the wait times out.
    /// Returns the value when the reply arrives.
    pub fn wait_blocking_with_diag<F>(self, interval: std::time::Duration, mut on_timeout: F) -> T
    where
        F: FnMut(f64),
    {
        // No WaitGuard here — caller (scheduler.wait_blocking_diag) registers
        // with the specific label. Direct users should use wait_blocking() instead.
        let start = std::time::Instant::now();
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(value) = state.value.take() {
                return value;
            }
            if state.closed {
                panic!("actor died without replying");
            }
            let (new_state, timeout_result) = self.inner.ready.wait_timeout(state, interval).unwrap();
            state = new_state;
            if timeout_result.timed_out() {
                on_timeout(start.elapsed().as_secs_f64());
            }
        }
    }

    /// Attente non-bloquante. Retourne None si pas encore de réponse.
    pub fn try_recv(&self) -> Option<T> {
        let mut state = self.inner.state.lock().unwrap();
        state.value.take()
    }

    /// Attente coopérative sans label (backward compat).
    pub fn wait_cooperative<F>(self, run_step: F) -> T
    where
        F: FnMut() -> bool,
    {
        self.wait_cooperative_named("(unnamed)", run_step)
    }

    /// Attente coopérative avec label pour diagnostics.
    ///
    /// Pompe le scheduler entre chaque tentative. Émet un warning si le wait
    /// dépasse le seuil (LUCIVY_WAIT_WARN_SECS, défaut 10s).
    ///
    /// `run_step` retourne `true` si du travail a été effectué.
    pub fn wait_cooperative_named<F>(self, label: &str, mut run_step: F) -> T
    where
        F: FnMut() -> bool,
    {
        // ENFORCED: cooperative wait inside an actor handler is forbidden.
        // Use pipe_to / collect_to / task_pipe_to instead — the result
        // comes back as a message, no thread is ever blocked.
        if crate::scheduler::in_actor_handler() {
            panic!(
                "[luciole] FATAL: cooperative wait ({label:?}) inside actor handler. \
                 Use pipe_to/collect_to/task_pipe_to instead of blocking waits in handlers."
            );
        }

        // Track nesting depth so execute_dag can force inline execution
        // (avoids recursive cooperative waits that cause stack overflow in WASM).
        crate::scheduler::enter_cooperative_wait();
        let _guard = crate::wait_graph::WaitGuard::current(label.to_string());
        if let Some(info) = crate::scheduler::current_thread_info() {
            info.enter_wait(label);
        }
        let result = self.wait_cooperative_inner(label, &mut run_step);
        if let Some(info) = crate::scheduler::current_thread_info() {
            info.leave_wait();
        }
        drop(_guard);
        crate::scheduler::leave_cooperative_wait();
        result
    }

    fn wait_cooperative_inner<F>(self, label: &str, run_step: &mut F) -> T
    where
        F: FnMut() -> bool,
    {

        use std::time::{Duration, Instant};
        use std::sync::atomic::{AtomicU64, Ordering};

        static WARN_SECS: AtomicU64 = AtomicU64::new(u64::MAX);

        let threshold_secs = {
            let v = WARN_SECS.load(Ordering::Relaxed);
            if v == u64::MAX {
                let secs = std::env::var("LUCIVY_WAIT_WARN_SECS")
                    .ok().and_then(|v| v.parse().ok())
                    .unwrap_or(10u64);
                WARN_SECS.store(secs, Ordering::Relaxed);
                secs
            } else {
                v
            }
        };
        let warn_threshold = Duration::from_secs(threshold_secs);

        let start = Instant::now();
        let mut warn_count = 0u32;

        loop {
            {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
                if state.closed {
                    panic!("[luciole] actor died without replying (wait {:?}, {:.1}s)",
                        label, start.elapsed().as_secs_f64());
                }
            }

            // Periodic warning with mermaid diagnostic
            let elapsed = start.elapsed();
            if elapsed >= warn_threshold * (warn_count + 1) {
                warn_count += 1;
                let sched = crate::scheduler::global_scheduler();
                let wait_graph = crate::wait_graph::dump_text();
                if warn_count <= 3 {
                    let mermaid = sched.dump_mermaid();
                    eprintln!("[luciole] WARNING: {:?} waiting {:.1}s (warn #{})\n```mermaid\n{}\n```\n{}",
                        label, elapsed.as_secs_f64(), warn_count, mermaid, wait_graph);
                } else {
                    let dump = sched.dump_state();
                    eprintln!("[luciole] WARNING: {:?} waiting {:.1}s (warn #{})\n{}\n{}",
                        label, elapsed.as_secs_f64(), warn_count, dump, wait_graph);
                }
            }

            if !run_step() {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
                if state.closed {
                    panic!("[luciole] actor died without replying (wait {:?}, {:.1}s)",
                        label, start.elapsed().as_secs_f64());
                }
                let (mut state, _) = self
                    .inner
                    .ready
                    .wait_timeout(state, Duration::from_millis(1))
                    .unwrap();
                if let Some(value) = state.value.take() {
                    if warn_count > 0 {
                        eprintln!("[luciole] {:?} resolved after {:.1}s",
                            label, start.elapsed().as_secs_f64());
                    }
                    return value;
                }
            }
        }
    }
}

impl<T: Send + 'static> ReplyReceiver<T> {
    /// Set a pipe callback. When Reply::send() fires, the value goes to this
    /// callback instead of being stored.
    ///
    /// **Race-free**: if the value already arrived (Reply::send was called
    /// before set_pipe), the callback is called immediately. Both the value
    /// check and the callback store are under the same Mutex.
    ///
    /// Returns `true` if the callback was called immediately (value was
    /// already available).
    ///
    /// Crate-private — public API goes through ActorRef::pipe_to /
    /// Pool::collect_to / Scheduler::task_pipe_to.
    pub(crate) fn set_pipe(&self, callback: impl FnOnce(T) + Send + 'static) -> bool {
        let mut state = self.inner.state.lock().unwrap();
        // If value already arrived, call callback immediately.
        if let Some(value) = state.value.take() {
            drop(state);
            callback(value);
            return true;
        }
        // Otherwise, store for later — Reply::send will call it.
        state.on_send = Some(Box::new(callback));
        false
    }

    /// Set the WaitGraph edge ID for this pipe. Cleaned up automatically
    /// by Reply::send (normal completion) or Reply::drop (sender died).
    ///
    /// Only meaningful for pipe_to (single receiver). For collect_to,
    /// the edge is managed by CollectState, not by individual receivers.
    pub(crate) fn set_pipe_edge(&self, edge_id: u64) {
        self.inner.pipe_edge_id.store(edge_id, AtomicOrdering::Release);
    }
}

// ---------------------------------------------------------------------------
// collect_replies_to — N results → 1 message
// ---------------------------------------------------------------------------

/// Shared state for collect_replies_to.
struct CollectState<T, M: Send + 'static> {
    results: Mutex<Vec<Option<T>>>,
    remaining: std::sync::atomic::AtomicUsize,
    target: crate::mailbox::ActorRef<M>,
    map: Mutex<Option<Box<dyn FnOnce(Vec<T>) -> M + Send>>>,
    edge_id: AtomicU64,
}

impl<T, M: Send + 'static> Drop for CollectState<T, M> {
    fn drop(&mut self) {
        // Safety net: if not all replies arrived (some senders died),
        // clean up the WaitGraph edge.
        let edge_id = self.edge_id.swap(0, AtomicOrdering::AcqRel);
        if edge_id != 0 {
            crate::wait_graph::unregister(edge_id);
        }
    }
}

/// Collect N reply results and send a single message when all are done.
///
/// Race-free: if some values already arrived before this call, they are
/// handled immediately. The shared state coordinates completion.
///
/// `results[i]` corresponds to `rxs[i]` (order preserved).
///
/// WaitGraph edge is registered automatically and cleaned up when the
/// last result arrives (or when all senders die).
pub fn collect_replies_to<T, M>(
    rxs: Vec<ReplyReceiver<T>>,
    target: &crate::mailbox::ActorRef<M>,
    label: &str,
    map: impl FnOnce(Vec<T>) -> M + Send + 'static,
) where
    T: Send + 'static,
    M: Send + 'static,
{
    let n = rxs.len();
    if n == 0 {
        let _ = target.send(map(vec![]));
        return;
    }

    let edge_id = crate::wait_graph::register(
        crate::wait_graph::current_waiter(),
        format!("{label} (0/{n})"),
    );

    let collect = Arc::new(CollectState {
        results: Mutex::new((0..n).map(|_| None).collect()),
        remaining: std::sync::atomic::AtomicUsize::new(n),
        target: target.clone(),
        map: Mutex::new(Some(Box::new(map))),
        edge_id: AtomicU64::new(edge_id),
    });

    for (i, rx) in rxs.into_iter().enumerate() {
        let collect = Arc::clone(&collect);
        rx.set_pipe(move |value: T| {
            collect.results.lock().unwrap()[i] = Some(value);
            if collect.remaining.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                // Last result — collect all and send.
                let eid = collect.edge_id.swap(0, AtomicOrdering::AcqRel);
                if eid != 0 {
                    crate::wait_graph::unregister(eid);
                }
                let collected: Vec<T> = collect.results.lock().unwrap()
                    .iter_mut()
                    .map(|opt| opt.take().unwrap())
                    .collect();
                if let Some(f) = collect.map.lock().unwrap().take() {
                    let _ = collect.target.send(f(collected));
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// reply() — factory
// ---------------------------------------------------------------------------

/// Crée une paire (Reply, ReplyReceiver).
pub fn reply<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            value: None,
            closed: false,
            on_send: None,
        }),
        ready: Condvar::new(),
        resume: Mutex::new(None),
        pipe_edge_id: AtomicU64::new(0),
    });
    (
        Reply {
            inner: inner.clone(),
        },
        ReplyReceiver { inner },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reply_send_recv() {
        let (tx, rx) = reply();
        tx.send(42u32);
        assert_eq!(rx.wait_blocking(), 42);
    }

    #[test]
    fn test_reply_try_recv_empty() {
        let (_tx, rx) = reply::<u32>();
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn test_reply_try_recv_after_send() {
        let (tx, rx) = reply();
        tx.send("hello");
        assert_eq!(rx.try_recv(), Some("hello"));
    }

    #[test]
    fn test_reply_cooperative() {
        let (tx, rx) = reply();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx.send(99);
        });
        let val = rx.wait_cooperative(|| false);
        assert_eq!(val, 99);
    }

    #[test]
    #[should_panic(expected = "actor died without replying")]
    fn test_reply_dropped_sender_panics() {
        let (tx, rx) = reply::<u32>();
        drop(tx);
        rx.wait_blocking();
    }

    #[test]
    fn test_join_resume_fires_on_last() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        let handle = ResumeHandle::new(move || {
            fired2.store(true, Ordering::Release);
        });

        let join = JoinResume::new(3, handle);
        let h1 = join.one_shot();
        let h2 = join.one_shot();
        let h3 = join.one_shot();

        h1.fire();
        assert!(!fired.load(Ordering::Acquire));
        h2.fire();
        assert!(!fired.load(Ordering::Acquire));
        h3.fire();
        assert!(fired.load(Ordering::Acquire));
    }

    #[test]
    fn test_join_resume_with_reply() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = fired.clone();
        let handle = ResumeHandle::new(move || {
            fired2.store(true, Ordering::Release);
        });

        let join = JoinResume::new(2, handle);

        let (tx1, rx1) = reply::<u32>();
        let (tx2, rx2) = reply::<u32>();
        rx1.set_resume(join.one_shot());
        rx2.set_resume(join.one_shot());

        tx1.send(10);
        assert!(!fired.load(Ordering::Acquire));
        tx2.send(20);
        assert!(fired.load(Ordering::Acquire));
    }

    // -------------------------------------------------------------------
    // pipe_to / collect_to tests
    // -------------------------------------------------------------------

    #[test]
    fn test_set_pipe_basic() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let received = Arc::new(AtomicU32::new(0));
        let received2 = received.clone();

        let (tx, rx) = reply::<u32>();
        rx.set_pipe(move |value| {
            received2.store(value, Ordering::Release);
        });
        tx.send(42);
        assert_eq!(received.load(Ordering::Acquire), 42);
    }

    #[test]
    fn test_set_pipe_value_already_arrived() {
        // Reply::send before set_pipe — the "race" case.
        // set_pipe must detect the value and call callback immediately.
        use std::sync::atomic::{AtomicU32, Ordering};
        let received = Arc::new(AtomicU32::new(0));
        let received2 = received.clone();

        let (tx, rx) = reply::<u32>();
        tx.send(99);  // Value arrives BEFORE set_pipe
        let immediate = rx.set_pipe(move |value| {
            received2.store(value, Ordering::Release);
        });
        assert!(immediate, "should detect value and call immediately");
        assert_eq!(received.load(Ordering::Acquire), 99);
    }

    #[test]
    fn test_set_pipe_sender_dies() {
        // Reply dropped without send — pipe callback never fires.
        // WaitGraph edge must be cleaned up by Reply::drop.
        use std::sync::atomic::{AtomicBool, Ordering};
        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();

        let (tx, rx) = reply::<u32>();
        let edge_before = crate::wait_graph::len();
        rx.set_pipe_edge(crate::wait_graph::register(
            crate::wait_graph::WaiterKind::Thread("test".into()),
            "test_pipe",
        ));
        rx.set_pipe(move |_| { called2.store(true, Ordering::Release); });
        let edge_during = crate::wait_graph::len();
        assert_eq!(edge_during, edge_before + 1);

        drop(tx);  // Sender dies without sending

        assert!(!called.load(Ordering::Acquire), "callback should not fire");
        // Edge should be cleaned up by Reply::drop
        assert_eq!(crate::wait_graph::len(), edge_before);
    }

    #[test]
    fn test_collect_replies_to_basic() {
        // Collect 3 results into a single Vec.
        let scheduler = crate::Scheduler::new(2);
        let _handle = scheduler.start();

        // Create a target actor that receives the collected results.
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let result_store = Arc::new(Mutex::new(Vec::<u32>::new()));
        let result_store2 = result_store.clone();

        struct Collector {
            done: Arc<AtomicBool>,
            results: Arc<Mutex<Vec<u32>>>,
        }
        enum CollMsg { Results(Vec<u32>) }
        impl crate::Actor for Collector {
            type Msg = CollMsg;
            fn name(&self) -> &'static str { "collector" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: CollMsg, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg {
                    CollMsg::Results(v) => {
                        *self.results.lock().unwrap() = v;
                        self.done.store(true, Ordering::Release);
                    }
                }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<CollMsg>(64);
        scheduler.spawn(Collector { done: done2, results: result_store2 }, mb, &mut ar, 64);

        // Create 3 reply pairs, collect them.
        let (tx0, rx0) = reply::<u32>();
        let (tx1, rx1) = reply::<u32>();
        let (tx2, rx2) = reply::<u32>();

        collect_replies_to(
            vec![rx0, rx1, rx2],
            &ar,
            "test_collect",
            |results| CollMsg::Results(results),
        );

        // Send results out of order.
        tx2.send(200);
        tx0.send(100);
        tx1.send(150);

        // Wait for the collector actor to process.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(done.load(Ordering::Acquire), "collector should be done");
        let results = result_store.lock().unwrap().clone();
        assert_eq!(results, vec![100, 150, 200], "order must match rxs index");
    }

    #[test]
    fn test_collect_replies_to_empty() {
        let scheduler = crate::Scheduler::new(1);
        let _handle = scheduler.start();

        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();

        struct Sink { done: Arc<AtomicBool> }
        enum SinkMsg { Got(Vec<u32>) }
        impl crate::Actor for Sink {
            type Msg = SinkMsg;
            fn name(&self) -> &'static str { "sink" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: SinkMsg, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg {
                    SinkMsg::Got(v) => {
                        assert!(v.is_empty());
                        self.done.store(true, Ordering::Release);
                    }
                }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<SinkMsg>(64);
        scheduler.spawn(Sink { done: done2 }, mb, &mut ar, 64);

        // Empty collect — should send immediately.
        collect_replies_to(
            vec![],
            &ar,
            "empty",
            |v| SinkMsg::Got(v),
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    fn test_collect_replies_to_values_already_arrived() {
        // All values arrive BEFORE collect_replies_to — should still work.
        let scheduler = crate::Scheduler::new(1);
        let _handle = scheduler.start();

        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let result_store = Arc::new(Mutex::new(Vec::<u32>::new()));
        let result_store2 = result_store.clone();

        struct Coll { done: Arc<AtomicBool>, r: Arc<Mutex<Vec<u32>>> }
        enum CMsg { R(Vec<u32>) }
        impl crate::Actor for Coll {
            type Msg = CMsg;
            fn name(&self) -> &'static str { "coll" }
            fn priority(&self) -> crate::Priority { crate::Priority::Medium }
            fn handle(&mut self, msg: CMsg, _ctx: &crate::ActorContext) -> crate::ActorStatus {
                match msg { CMsg::R(v) => { *self.r.lock().unwrap() = v; self.done.store(true, Ordering::Release); } }
                crate::ActorStatus::Continue
            }
        }

        let (mb, mut ar) = crate::mailbox::<CMsg>(64);
        scheduler.spawn(Coll { done: done2, r: result_store2 }, mb, &mut ar, 64);

        let (tx0, rx0) = reply::<u32>();
        let (tx1, rx1) = reply::<u32>();

        // Send BEFORE collect_replies_to
        tx0.send(10);
        tx1.send(20);

        collect_replies_to(
            vec![rx0, rx1],
            &ar, "pre_arrived",
            |v| CMsg::R(v),
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(done.load(Ordering::Acquire));
        assert_eq!(*result_store.lock().unwrap(), vec![10, 20]);
    }
}
