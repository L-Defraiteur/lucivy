//! Async executor integrated with luciole's actor scheduler.
//!
//! Spawns futures into the actor pool — no separate runtime, no extra threads.
//! Futures inherit the actor's priority (Idle for IO, High for search, etc.).
//!
//! ```ignore
//! let scope = AsyncScope::new(Priority::Idle);
//!
//! // Fire-and-forget
//! scope.spawn_detached(async { write_to_opfs(data).await });
//!
//! // With result
//! let handle = scope.spawn(async { fetch_delta(url).await });
//! let delta = handle.wait();
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake};

use crate::mailbox::{mailbox, ActorRef, Mailbox};
use crate::reply::{reply, Reply, ReplyReceiver};
use crate::scheduler::{global_scheduler, SchedulerNotifier};
use crate::{Actor, ActorStatus, Priority};

// ── Waker ──────────────────────────────────────────────────────────────

/// A std::task::Waker that wakes a luciole actor via the scheduler.
struct LucioleWaker {
    notifier: SchedulerNotifier,
}

impl Wake for LucioleWaker {
    fn wake(self: Arc<Self>) {
        self.notifier.wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notifier.wake();
    }
}

// ── Task storage ───────────────────────────────────────────────────────

trait PollableTask: Send {
    /// Poll the task. Returns true if completed.
    fn poll(&mut self, cx: &mut Context<'_>) -> bool;
}

struct SpawnedTask<F: Future + Send> {
    future: Pin<Box<F>>,
    reply: Option<Reply<F::Output>>,
}

impl<F> PollableTask for SpawnedTask<F>
where
    F: Future + Send,
    F::Output: Send + 'static,
{
    fn poll(&mut self, cx: &mut Context<'_>) -> bool {
        match self.future.as_mut().poll(cx) {
            Poll::Ready(result) => {
                if let Some(reply) = self.reply.take() {
                    reply.send(result);
                }
                true
            }
            Poll::Pending => false,
        }
    }
}

struct DetachedTask {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl PollableTask for DetachedTask {
    fn poll(&mut self, cx: &mut Context<'_>) -> bool {
        matches!(self.future.as_mut().poll(cx), Poll::Ready(()))
    }
}

// ── Messages ───────────────────────────────────────────────────────────

enum AsyncMsg {
    Spawn {
        task: Box<dyn PollableTask>,
    },
    Drain(crate::DrainMsg),
}

impl From<crate::DrainMsg> for AsyncMsg {
    fn from(d: crate::DrainMsg) -> Self {
        AsyncMsg::Drain(d)
    }
}

// ── AsyncActor ─────────────────────────────────────────────────────────

struct AsyncActor {
    tasks: Vec<Box<dyn PollableTask>>,
    priority: Priority,
    notifier: Option<SchedulerNotifier>,
}

impl AsyncActor {
    fn new(priority: Priority) -> Self {
        Self {
            tasks: Vec::new(),
            priority,
            notifier: None,
        }
    }
}

impl Actor for AsyncActor {
    type Msg = AsyncMsg;

    fn name(&self) -> &'static str { "async" }

    fn handle(&mut self, msg: AsyncMsg) -> ActorStatus {
        match msg {
            AsyncMsg::Spawn { task } => {
                self.tasks.push(task);
                ActorStatus::Continue
            }
            AsyncMsg::Drain(d) => {
                d.ack();
                ActorStatus::Continue
            }
        }
    }

    fn priority(&self) -> Priority {
        self.priority
    }

    fn poll_idle(&mut self) -> Poll<()> {
        if self.tasks.is_empty() {
            return Poll::Pending;
        }

        let waker = match &self.notifier {
            Some(n) => Arc::new(LucioleWaker { notifier: n.clone() }).into(),
            None => return Poll::Pending,
        };
        let mut cx = Context::from_waker(&waker);

        // Poll all pending tasks, remove completed ones.
        self.tasks.retain_mut(|task| !task.poll(&mut cx));

        if self.tasks.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(()) // re-schedule: still has pending tasks
        }
    }

    fn on_start(&mut self, self_ref: ActorRef<AsyncMsg>) {
        // Extract the notifier from the ActorRef's wake_handle.
        if let Some(wh) = self_ref.wake_handle() {
            self.notifier = Some(wh.notifier.clone());
        }
    }
}

// ── AsyncScope (public API) ────────────────────────────────────────────

/// Scope for submitting futures to the luciole actor pool.
///
/// Futures run on the shared thread pool at the specified priority.
/// Multiple AsyncScopes can coexist with different priorities.
pub struct AsyncScope {
    sender: ActorRef<AsyncMsg>,
}

impl AsyncScope {
    /// Create a new async scope with the given priority.
    /// Spawns an AsyncActor in the global scheduler.
    pub fn new(priority: Priority) -> Self {
        let actor = AsyncActor::new(priority);
        let (mb, mut actor_ref) = mailbox::<AsyncMsg>(256);
        let scheduler = global_scheduler();
        scheduler.spawn(actor, mb, &mut actor_ref, 256);
        Self { sender: actor_ref }
    }

    /// Spawn a future and get a handle to its result.
    pub fn spawn<F, T>(&self, future: F) -> FutureHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = reply();
        let task = SpawnedTask {
            future: Box::pin(future),
            reply: Some(tx),
        };
        let _ = self.sender.send(AsyncMsg::Spawn {
            task: Box::new(task),
        });
        FutureHandle { rx }
    }

    /// Spawn a future without waiting for its result.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = DetachedTask {
            future: Box::pin(future),
        };
        let _ = self.sender.send(AsyncMsg::Spawn {
            task: Box::new(task),
        });
    }
}

/// Handle to a spawned future's result.
pub struct FutureHandle<T> {
    rx: ReplyReceiver<T>,
}

impl<T: Send + 'static> FutureHandle<T> {
    /// Wait cooperatively for the result (doesn't block the scheduler).
    pub fn wait(self) -> T {
        self.rx.wait_cooperative_named("async_wait", || {
            global_scheduler().run_one_step()
        })
    }

    /// Check without blocking.
    pub fn try_get(&mut self) -> Option<T> {
        self.rx.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_and_wait() {
        let scope = AsyncScope::new(Priority::Idle);
        let handle = scope.spawn(async { 42 });
        assert_eq!(handle.wait(), 42);
    }

    #[test]
    fn test_spawn_detached() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();

        let scope = AsyncScope::new(Priority::Idle);
        scope.spawn_detached(async move {
            done2.store(true, Ordering::Release);
        });

        // Wait for it to complete via a dummy spawn.
        let _ = scope.spawn(async {}).wait();
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    fn test_spawn_multiple() {
        let scope = AsyncScope::new(Priority::Low);
        let h1 = scope.spawn(async { 10 });
        let h2 = scope.spawn(async { 20 });
        let h3 = scope.spawn(async { 30 });
        assert_eq!(h1.wait() + h2.wait() + h3.wait(), 60);
    }

    #[test]
    fn test_async_with_computation() {
        let scope = AsyncScope::new(Priority::Medium);
        let handle = scope.spawn(async {
            let mut sum = 0u64;
            for i in 0..1000 {
                sum += i;
            }
            sum
        });
        assert_eq!(handle.wait(), 999 * 1000 / 2);
    }
}
