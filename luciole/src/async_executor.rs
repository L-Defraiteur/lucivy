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
use std::sync::atomic::{AtomicU32, Ordering};
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

// ── SignalFuture — poll an AtomicU32 shared with external code ──────

/// A Future that completes when an external signal is set.
///
/// Used to bridge async operations from JS (Promises), OS, or other threads.
/// The signal is a shared AtomicU32:
/// - 0 = pending
/// - 1 = completed OK
/// - 2 = completed with error
///
/// For JS bridge: Rust creates the signal, passes the pointer to JS via FFI,
/// JS sets it to 1 when the Promise resolves. The AsyncActor re-polls and
/// the future completes.
pub struct SignalFuture {
    signal: Arc<AtomicU32>,
}

/// Status values for SignalFuture.
pub const SIGNAL_PENDING: u32 = 0;
pub const SIGNAL_OK: u32 = 1;
pub const SIGNAL_ERROR: u32 = 2;

impl SignalFuture {
    /// Create a new pending signal future.
    /// Returns (future, signal_ptr) — pass signal_ptr to the external code
    /// that will complete the operation.
    pub fn new() -> (Self, *const AtomicU32) {
        let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
        let ptr = Arc::as_ptr(&signal);
        (Self { signal }, ptr)
    }

    /// Create from an existing shared signal.
    pub fn from_signal(signal: Arc<AtomicU32>) -> Self {
        Self { signal }
    }

    /// Get a raw pointer to the signal (for FFI).
    pub fn signal_ptr(&self) -> *const AtomicU32 {
        Arc::as_ptr(&self.signal)
    }
}

impl Future for SignalFuture {
    type Output = Result<(), ()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.signal.load(Ordering::Acquire) {
            SIGNAL_PENDING => Poll::Pending,
            SIGNAL_OK => Poll::Ready(Ok(())),
            _ => Poll::Ready(Err(())),
        }
    }
}

/// A SignalFuture that also carries result data (for reads).
///
/// The external code writes data into a buffer, then sets the signal.
/// On completion, the future takes ownership of the data.
pub struct SignalDataFuture {
    signal: Arc<AtomicU32>,
    /// Pointer to the result data (set by external code before signaling).
    data_ptr: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl SignalDataFuture {
    /// Create a new pending data future.
    /// Returns (future, signal_ptr, data_setter).
    /// External code should: set data via data_setter, then signal.
    pub fn new() -> (Self, *const AtomicU32, Arc<std::sync::Mutex<Option<Vec<u8>>>>) {
        let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
        let data = Arc::new(std::sync::Mutex::new(None));
        let ptr = Arc::as_ptr(&signal);
        (Self { signal, data_ptr: data.clone() }, ptr, data)
    }
}

impl Future for SignalDataFuture {
    type Output = Result<Vec<u8>, ()>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.signal.load(Ordering::Acquire) {
            SIGNAL_PENDING => Poll::Pending,
            SIGNAL_OK => {
                let data = self.data_ptr.lock().unwrap().take().unwrap_or_default();
                Poll::Ready(Ok(data))
            }
            _ => Poll::Ready(Err(())),
        }
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
    fn test_signal_future() {
        let scope = AsyncScope::new(Priority::Idle);
        let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
        let signal2 = signal.clone();

        let handle = scope.spawn(SignalFuture::from_signal(signal.clone()));

        // Signal not set yet — spawn a thread to set it.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            signal2.store(SIGNAL_OK, Ordering::Release);
        });

        let result = handle.wait();
        assert!(result.is_ok());
    }

    #[test]
    fn test_signal_future_error() {
        let scope = AsyncScope::new(Priority::Idle);
        let signal = Arc::new(AtomicU32::new(SIGNAL_PENDING));
        let signal2 = signal.clone();

        let handle = scope.spawn(SignalFuture::from_signal(signal.clone()));

        std::thread::spawn(move || {
            signal2.store(SIGNAL_ERROR, Ordering::Release);
        });

        let result = handle.wait();
        assert!(result.is_err());
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
