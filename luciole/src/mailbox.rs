use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use flume as channel;

use super::events::{EventBus, SchedulerEvent};
use super::scheduler::SchedulerNotifier;

/// Completion shared by all references, independent of the shutdown protocol.
/// A reply is delivered only after the receiver and owning actor are dropped.
// One linearization point for admission versus close. Low bits count sends;
// the high bit permanently closes admission. No separate check/increment race.
const CLOSING: usize = 1usize << (usize::BITS - 1);
const ACTIVE_SENDS: usize = CLOSING - 1;

struct Completion {
    sends: AtomicUsize,
    send_wait: Mutex<()>,
    sends_changed: Condvar,
    waiters: Mutex<Option<Vec<crate::Reply<()>>>>,
}

impl Completion {
    fn new() -> Self {
        Self {
            sends: AtomicUsize::new(0),
            send_wait: Mutex::new(()),
            sends_changed: Condvar::new(),
            waiters: Mutex::new(Some(Vec::new())),
        }
    }
    fn begin_send(&self) -> Option<SendGuard<'_>> {
        let mut state = self.sends.load(Ordering::Acquire);
        loop {
            if state & CLOSING != 0 || state & ACTIVE_SENDS == ACTIVE_SENDS { return None; }
            match self.sends.compare_exchange_weak(state, state + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(SendGuard(self)),
                Err(current) => state = current,
            }
        }
    }
    fn subscribe(&self) -> crate::ReplyReceiver<()> {
        let (tx, rx) = crate::reply();
        let mut state = self.waiters.lock().unwrap();
        if let Some(waiters) = state.as_mut() {
            waiters.push(tx);
        } else {
            drop(state);
            tx.send(());
        }
        rx
    }
    fn finish(&self) {
        let waiters = self.waiters.lock().unwrap().take().unwrap_or_default();
        // Never run reply callbacks under the lifecycle mutex.
        for waiter in waiters { waiter.send(()); }
    }
}

// Borrowed from ActorRef for the duration of send/try_send: no Arc clone.
struct SendGuard<'a>(&'a Completion);
impl Drop for SendGuard<'_> {
    fn drop(&mut self) {
        let previous = self.0.sends.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous & ACTIVE_SENDS > 0);
        if previous & CLOSING != 0 {
            // Pair with the closer's check/wait mutex. Even intermediate
            // completions wake it: a full bounded queue may need draining
            // before the LAST admitted sender can finish.
            let _wait = self.0.send_wait.lock().unwrap();
            self.0.sends_changed.notify_all();
        }
    }
}

struct CompletionGuard(Arc<Completion>);
impl Drop for CompletionGuard {
    fn drop(&mut self) { self.0.finish(); }
}

/// Côté réception d'un acteur. FIFO strict.
pub struct Mailbox<M> {
    pub(super) receiver: channel::Receiver<M>,
    // Last: pending messages/receiver must be destroyed before completion.
    _completion: CompletionGuard,
}

impl<M> Drop for Mailbox<M> {
    fn drop(&mut self) {
        let completion = &self._completion.0;
        let already_unwinding = std::thread::panicking();
        let mut first_panic = None;
        let mut discard = |messages: Vec<M>| {
            for message in messages {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(message)));
                if let Err(payload) = result {
                    if first_panic.is_none() { first_panic = Some(payload); }
                }
            }
        };
        completion.sends.fetch_or(CLOSING, Ordering::AcqRel);
        loop {
            // Free bounded-channel slots so sends admitted before closing can
            // finish. Drop messages outside our locks: their destructors may
            // send to other actors (or attempt a now-rejected send to this one).
            let pending: Vec<_> = self.receiver.drain().collect();
            discard(pending);
            let mut state = completion.send_wait.lock().unwrap();
            if completion.sends.load(Ordering::Acquire) & ACTIVE_SENDS == 0 {
                drop(state);
                // No admitted sender remains and no new sender can enter.
                let remaining: Vec<_> = self.receiver.drain().collect();
                discard(remaining);
                break;
            }
            if !self.receiver.is_empty() { continue; }
            // A sender will either fill the empty queue and finish, or finish
            // without sending. Its guard wakes us; no polling or sleep.
            state = completion.sends_changed.wait(state).unwrap();
            drop(state);
        }
        drop(discard);
        // CompletionGuard is dropped during this unwind only AFTER active=0
        // and an empty queue. Preserve an existing unwind rather than aborting
        // with a second panic from cleanup.
        if !already_unwinding {
            if let Some(payload) = first_panic { std::panic::resume_unwind(payload); }
        }
    }
}

impl<M> Mailbox<M> {
    pub fn try_recv(&self) -> Option<M> {
        self.receiver.try_recv().ok()
    }

    /// Returns true if all senders have been dropped and the mailbox is empty.
    /// The scheduler uses this to garbage-collect zombie actors.
    pub fn is_disconnected(&self) -> bool {
        self.receiver.is_disconnected() && self.receiver.is_empty()
    }

    pub fn has_pending(&self) -> bool {
        !self.receiver.is_empty()
    }

    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

/// Handle pour envoyer des messages à un acteur. Clonable.
pub struct ActorRef<M> {
    sender: channel::Sender<M>,
    completion: Arc<Completion>,
    notifier: Option<Arc<WakeHandle>>,
}

/// Partagé entre l'ActorRef et le scheduler.
/// Le scheduler remet `is_idle = true` quand l'acteur passe idle.
/// L'ActorRef le passe à `false` (swap) pour savoir s'il doit wake.
pub struct WakeHandle {
    pub(crate) notifier: SchedulerNotifier,
    pub(crate) is_idle: AtomicBool,
    pub(crate) events: Arc<EventBus<SchedulerEvent>>,
    /// Nom de l'acteur — copié au spawn, zero-lock dans send().
    pub(crate) actor_name: &'static str,
}

// Manual Clone impl — don't require M: Clone (crossbeam Sender is Clone for any M).
impl<M> Clone for ActorRef<M> {
    fn clone(&self) -> Self {
        ActorRef {
            sender: self.sender.clone(),
            completion: self.completion.clone(),
            notifier: self.notifier.clone(),
        }
    }
}

impl<M> ActorRef<M> {
    /// Wait for destruction, not merely a shutdown acknowledgement.
    /// The scheduler parameter also supports local cooperative schedulers.
    /// Waiting from an actor handler is forbidden (including self-join).
    pub fn wait_stopped(&self, scheduler: &crate::Scheduler, label: &str) -> Result<(), String> {
        if crate::scheduler::in_actor_handler() {
            return Err("cannot wait for actor destruction inside a handler".into());
        }
        scheduler.try_wait(self.completion.subscribe(), label)
            .map_err(|_| format!("actor completion lost ({label})"))
    }

    pub fn send(&self, msg: M) -> Result<(), channel::SendError<M>> {
        let _send = match self.completion.begin_send() {
            Some(guard) => guard,
            None => return Err(channel::SendError(msg)),
        };
        self.sender.send(msg)?;
        if let Some(wh) = &self.notifier {
            if wh.is_idle.swap(false, Ordering::AcqRel) {
                wh.events.emit(SchedulerEvent::MessageSentWithWake {
                    actor_id: wh.notifier.actor_id(),
                    actor_name: wh.actor_name,
                });
                wh.notifier.wake();
            } else {
                wh.events.emit(SchedulerEvent::MessageSentNoWake {
                    actor_id: wh.notifier.actor_id(),
                    actor_name: wh.actor_name,
                    mailbox_depth: self.sender.len(),
                });
            }
        } else {
            eprintln!("[diag] ActorRef::send: NO notifier! mailbox_depth={}",
                self.sender.len());
        }
        Ok(())
    }

    /// Send a message containing a `Reply<R>`, wait cooperatively for the response.
    ///
    /// `make_msg` receives a `Reply<R>` and must return the message to send.
    /// The caller blocks (cooperatively) until the actor replies.
    ///
    /// ```ignore
    /// let count = actor_ref.request(|r| CounterMsg::Get(r), "get_count")?;
    /// ```
    pub fn request<R, F>(&self, make_msg: F, label: &str) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(crate::reply::Reply<R>) -> M,
    {
        let (tx, rx) = crate::reply::reply::<R>();
        self.send(make_msg(tx)).map_err(|_| "actor disconnected".to_string())?;
        let scheduler = crate::scheduler::global_scheduler();
        // An actor that stops (or panics) with this request in its mailbox
        // drops the Reply: that is an error for the caller, not a panic —
        // a panic here inside a DAG node used to corrupt the heap.
        scheduler
            .try_wait(rx, label)
            .map_err(|_| format!("actor died without replying ({label})"))
    }

    /// Access the wake handle (for async executor waker integration).
    pub(crate) fn wake_handle(&self) -> Option<&Arc<WakeHandle>> {
        self.notifier.as_ref()
    }

    /// Number of messages currently in the mailbox (for diagnostics).
    pub fn mailbox_depth(&self) -> usize {
        self.sender.len()
    }

    /// Send a request to this actor and pipe the result to a target actor.
    ///
    /// `msg_fn` receives a `Reply<T>` and must return the message to send.
    /// When this actor replies, `map` transforms the result into a message
    /// for `target`, which receives it in its mailbox.
    ///
    /// **Atomic**: callback is registered BEFORE the message is sent.
    /// No race condition, no thread blocked, no Suspend needed.
    ///
    /// ```ignore
    /// // "Ask indexer to flush, send result back to me as FlushDone"
    /// self.indexer.pipe_to(
    ///     |reply| IndexerMsg::Flush(reply),
    ///     &self.self_ref, "flush",
    ///     |result| ShardMsg::FlushDone { result },
    /// );
    /// ```
    pub fn pipe_to<T, TargetMsg, F, G>(
        &self,
        msg_fn: F,
        target: &ActorRef<TargetMsg>,
        label: &str,
        map: G,
    ) where
        T: Send + 'static,
        TargetMsg: Send + 'static,
        F: FnOnce(crate::reply::Reply<T>) -> M,
        G: FnOnce(T) -> TargetMsg + Send + 'static,
    {
        let (tx, rx) = crate::reply::reply::<T>();
        let target = target.clone();
        let edge_id = crate::wait_graph::register(
            crate::wait_graph::current_waiter(),
            label.to_string(),
        );
        // 1. Callback BEFORE send — no race possible.
        rx.set_pipe_edge(edge_id);
        rx.set_pipe(move |value| {
            let _ = target.send(map(value));
        });
        // 2. Send AFTER callback is in place.
        let _ = self.send(msg_fn(tx));
    }

    pub fn try_send(&self, msg: M) -> Result<(), channel::TrySendError<M>> {
        let _send = match self.completion.begin_send() {
            Some(guard) => guard,
            None => return Err(channel::TrySendError::Disconnected(msg)),
        };
        self.sender.try_send(msg)?;
        if let Some(wh) = &self.notifier {
            if wh.is_idle.swap(false, Ordering::AcqRel) {
                wh.notifier.wake();
            }
        }
        Ok(())
    }
}

impl<M: Send + 'static> crate::scope::Drainable for ActorRef<M>
where M: From<crate::pool::DrainMsg>
{
    fn drain(&self, label: &str) {
        let _ = self.request(
            |r| M::from(crate::pool::DrainMsg(r)),
            label,
        );
    }
}

/// Crée une paire (Mailbox, ActorRef).
/// `capacity = 0` → unbounded, `capacity > 0` → bounded.
/// Le WakeHandle sera attaché par le scheduler au spawn.
pub fn mailbox<M>(capacity: usize) -> (Mailbox<M>, ActorRef<M>) {
    let (sender, receiver) = if capacity == 0 {
        channel::unbounded()
    } else {
        channel::bounded(capacity)
    };
    let completion = Arc::new(Completion::new());
    (
        Mailbox { receiver, _completion: CompletionGuard(completion.clone()) },
        ActorRef {
            sender,
            completion,
            notifier: None,
        },
    )
}

pub(super) fn attach_wake_handle<M>(actor_ref: &mut ActorRef<M>, handle: Arc<WakeHandle>) {
    actor_ref.notifier = Some(handle);
}

#[cfg(test)]
mod destruction_tests {
    use super::*;

    #[test]
    fn atomic_admission_keeps_count_when_closing() {
        let completion = Completion::new();
        let first = completion.begin_send().unwrap();
        let second = completion.begin_send().unwrap();
        assert_eq!(completion.sends.fetch_or(CLOSING, Ordering::AcqRel), 2);
        assert!(completion.begin_send().is_none());
        drop(first);
        assert_eq!(completion.sends.load(Ordering::Acquire), CLOSING | 1);
        drop(second);
        assert_eq!(completion.sends.load(Ordering::Acquire), CLOSING);
        assert!(completion.begin_send().is_none());
    }

    #[test]
    fn completion_drains_admitted_send_and_rejects_new_sends() {
        let (mailbox, actor_ref) = mailbox::<Arc<()>>(1);
        let resource = Arc::new(());
        actor_ref.send(resource.clone()).unwrap();
        // Freeze an admitted send before its potentially blocking flume send.
        // This is the same guard held by ActorRef::send across flume.send.
        let admitted = actor_ref.completion.begin_send().unwrap();
        let sender = actor_ref.sender.clone();
        let payload = resource.clone();
        std::thread::scope(|scope| {
            let sending = scope.spawn(move || {
                let result = sender.send(payload);
                drop(admitted);
                result
            });
            drop(mailbox);
            sending.join().unwrap().unwrap();
        });
        actor_ref.wait_stopped(&crate::Scheduler::new(1), "mailbox_destroyed").unwrap();
        assert_eq!(Arc::strong_count(&resource), 1);
        assert!(actor_ref.send(resource.clone()).is_err());
        assert!(actor_ref.try_send(resource.clone()).is_err());
        assert_eq!(Arc::strong_count(&resource), 1);
    }

    #[test]
    fn closing_drains_more_admitted_senders_than_channel_capacity() {
        let (mailbox, actor_ref) = mailbox::<Arc<()>>(1);
        let resource = Arc::new(());
        actor_ref.send(resource.clone()).unwrap(); // Fill before any receiver runs.
        std::thread::scope(|scope| {
            let mut sending = Vec::new();
            for _ in 0..3 {
                let admitted = actor_ref.completion.begin_send().unwrap();
                let sender = actor_ref.sender.clone();
                let payload = resource.clone();
                sending.push(scope.spawn(move || {
                    let result = sender.send(payload);
                    drop(admitted);
                    result
                }));
            }
            drop(mailbox);
            for thread in sending { thread.join().unwrap().unwrap(); }
        });
        actor_ref.wait_stopped(&crate::Scheduler::new(1), "all_admitted_destroyed").unwrap();
        assert_eq!(Arc::strong_count(&resource), 1);
    }

    #[test]
    fn panicking_message_drop_waits_for_admitted_send() {
        use std::sync::mpsc;
        use std::time::Duration;
        enum Message { Bomb(mpsc::Sender<()>), Payload(Arc<()>) }
        impl Drop for Message {
            fn drop(&mut self) {
                if let Self::Bomb(entered) = self {
                    let _ = entered.send(());
                    panic!("expected queued destructor failure");
                }
                if let Self::Payload(resource) = self {
                    assert!(Arc::strong_count(resource) >= 2);
                }
            }
        }
        let (mailbox, actor_ref) = mailbox(1);
        let resource = Arc::new(());
        let (entered_tx, entered_rx) = mpsc::channel();
        assert!(actor_ref.send(Message::Bomb(entered_tx)).is_ok());
        let admitted = actor_ref.completion.begin_send().unwrap();
        let payload = Message::Payload(resource.clone());
        let sender = actor_ref.sender.clone();
        let (finished_tx, finished_rx) = mpsc::channel();
        let dropping = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(mailbox)));
            finished_tx.send(result.is_err()).unwrap();
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        let premature = finished_rx.recv_timeout(Duration::from_secs(1));
        let completed_early = actor_ref.completion.waiters.lock().unwrap().is_none();
        // Release even when the old implementation completed prematurely.
        let sent = sender.send(payload);
        drop(admitted);
        dropping.join().unwrap();
        assert!(entered.is_ok());
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert!(!completed_early, "completion preceded admitted send cleanup");
        assert!(sent.is_ok());
        assert!(finished_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        actor_ref.wait_stopped(&crate::Scheduler::new(1), "panic_cleaned").unwrap();
        assert_eq!(Arc::strong_count(&resource), 1);
    }

    #[test]
    fn pending_destructor_can_reenter_closed_mailbox() {
        struct Pending(ActorRef<Option<Box<Pending>>>);
        impl Drop for Pending {
            fn drop(&mut self) { assert!(self.0.try_send(None).is_err()); }
        }
        let (mailbox, actor_ref) = mailbox(1);
        assert!(actor_ref.send(Some(Box::new(Pending(actor_ref.clone())))).is_ok());
        drop(mailbox);
        actor_ref.wait_stopped(&crate::Scheduler::new(1), "reentrant_drop").unwrap();
    }
}
