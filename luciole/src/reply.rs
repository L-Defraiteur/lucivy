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
}

struct State<T> {
    value: Option<T>,
    closed: bool,
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
        state.value = Some(value);
        state.closed = true;
        self.inner.ready.notify_one();
        drop(state);
        // Fire resume handle if registered (re-schedules suspended actor).
        if let Some(handle) = self.inner.resume.lock().unwrap().take() {
            handle.fire();
        }
    }
}

impl<T> Drop for Reply<T> {
    fn drop(&mut self) {
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
        // Guard: cooperative wait inside an actor handler should use
        // ActorStatus::Suspend with ctx.resume_handle() instead.
        // Currently a warning — will become abort once all actors are migrated
        // (requires actor lifecycle management for proper Suspend cleanup).
        #[cfg(debug_assertions)]
        if crate::scheduler::in_actor_handler() {
            eprintln!(
                "[luciole] WARNING: cooperative wait ({label:?}) inside actor handler — \
                 consider using ActorStatus::Suspend with ctx.resume_handle()"
            );
        }

        // Track nesting depth so execute_dag can force inline execution
        // (avoids recursive cooperative waits that cause stack overflow in WASM).
        crate::scheduler::enter_cooperative_wait();
        if let Some(info) = crate::scheduler::current_thread_info() {
            info.enter_wait(label);
        }
        let result = self.wait_cooperative_inner(label, &mut run_step);
        if let Some(info) = crate::scheduler::current_thread_info() {
            info.leave_wait();
        }
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
                if warn_count <= 3 {
                    let mermaid = sched.dump_mermaid();
                    eprintln!("[luciole] WARNING: {:?} waiting {:.1}s (warn #{})\n```mermaid\n{}\n```",
                        label, elapsed.as_secs_f64(), warn_count, mermaid);
                } else {
                    let dump = sched.dump_state();
                    eprintln!("[luciole] WARNING: {:?} waiting {:.1}s (warn #{})\n{}",
                        label, elapsed.as_secs_f64(), warn_count, dump);
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

/// Crée une paire (Reply, ReplyReceiver).
pub fn reply<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            value: None,
            closed: false,
        }),
        ready: Condvar::new(),
        resume: Mutex::new(None),
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
}
