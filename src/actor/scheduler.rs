use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Instant;

use super::events::{EventBus, EventReceiver, SchedulerEvent, WakeReason};
use super::mailbox::{attach_wake_handle, ActorRef, Mailbox, WakeHandle};
use super::{Actor, ActorStatus, Priority};

/// Identifiant unique d'un acteur dans le scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ActorId(u64);

/// Nombre max de messages traités par batch avant de yield au scheduler.
const BATCH_SIZE: usize = 32;

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

pub(crate) struct Scheduler {
    num_threads: usize,
    shared: Arc<SharedState>,
    next_actor_id: AtomicU64,
}

struct SharedState {
    ready_queue: Mutex<BinaryHeap<ReadyEntry>>,
    /// Un acteur est `take()` pendant qu'il est traité par un thread,
    /// ce qui évite les deadlocks lors de réentrance (doc 08, point 3).
    actors: Mutex<HashMap<ActorId, ActorSlot>>,
    work_available: Condvar,
    shutdown: AtomicBool,
    events: Arc<EventBus>,
}

struct ActorSlot {
    actor: Option<Box<dyn AnyActor>>,
    name: &'static str,
    /// Partagé avec les ActorRef — le scheduler remet is_idle=true
    /// quand l'acteur passe idle, l'ActorRef le swap à false pour wake.
    wake_handle: Arc<WakeHandle>,
}

#[derive(Debug, Clone, Copy)]
struct ReadyEntry {
    priority: Priority,
    actor_id: ActorId,
}

impl Eq for ReadyEntry {}

impl PartialEq for ReadyEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.actor_id == other.actor_id
    }
}

impl Ord for ReadyEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl PartialOrd for ReadyEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// AnyActor — type erasure
// ---------------------------------------------------------------------------

trait AnyActor: Send {
    fn try_handle_one(&mut self) -> Option<ActorStatus>;
    fn priority(&self) -> Priority;
    fn has_pending(&self) -> bool;
    fn poll_idle(&mut self) -> Poll<()>;
    fn name(&self) -> &'static str;
    fn mailbox_len(&self) -> usize;
}

struct ActorWrapper<A: Actor> {
    actor: A,
    mailbox: Mailbox<A::Msg>,
}

impl<A: Actor> AnyActor for ActorWrapper<A> {
    fn try_handle_one(&mut self) -> Option<ActorStatus> {
        let msg = self.mailbox.try_recv()?;
        Some(self.actor.handle(msg))
    }

    fn priority(&self) -> Priority {
        self.actor.priority()
    }

    fn has_pending(&self) -> bool {
        self.mailbox.has_pending()
    }

    fn poll_idle(&mut self) -> Poll<()> {
        self.actor.poll_idle()
    }

    fn name(&self) -> &'static str {
        self.actor.name()
    }

    fn mailbox_len(&self) -> usize {
        self.mailbox.len()
    }
}

// ---------------------------------------------------------------------------
// SchedulerNotifier — utilisé par ActorRef pour réveiller le scheduler
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(super) struct SchedulerNotifier {
    actor_id: ActorId,
    shared: Arc<SharedState>,
}

impl SchedulerNotifier {
    pub fn wake(&self) {
        // Récupérer la priorité et le nom en un seul lock.
        let (priority, name) = {
            let actors = self.shared.actors.lock().unwrap();
            match actors.get(&self.actor_id) {
                Some(slot) => {
                    let prio = match &slot.actor {
                        Some(actor) => actor.priority(),
                        None => Priority::Medium, // En cours de traitement, on push quand même
                    };
                    (prio, slot.name)
                }
                None => return, // L'acteur n'existe plus
            }
        };

        {
            let mut queue = self.shared.ready_queue.lock().unwrap();
            queue.push(ReadyEntry {
                priority,
                actor_id: self.actor_id,
            });
        }

        self.shared.events.emit(SchedulerEvent::ActorWoken {
            actor_id: self.actor_id,
            actor_name: name,
            woken_by: WakeReason::MessageReceived,
        });

        self.shared.work_available.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Scheduler API
// ---------------------------------------------------------------------------

impl Scheduler {
    pub fn new(num_threads: usize) -> Self {
        assert!(num_threads >= 1, "scheduler needs at least 1 thread");
        Scheduler {
            num_threads,
            shared: Arc::new(SharedState {
                ready_queue: Mutex::new(BinaryHeap::new()),
                actors: Mutex::new(HashMap::new()),
                work_available: Condvar::new(),
                shutdown: AtomicBool::new(false),
                events: Arc::new(EventBus::new()),
            }),
            next_actor_id: AtomicU64::new(0),
        }
    }

    pub fn spawn<A: Actor>(
        &self,
        mut actor: A,
        mailbox: Mailbox<A::Msg>,
        actor_ref: &mut ActorRef<A::Msg>,
        capacity: usize,
    ) -> ActorId {
        let id = ActorId(self.next_actor_id.fetch_add(1, Ordering::Relaxed));
        let name = actor.name();

        let wake_handle = Arc::new(WakeHandle {
            notifier: SchedulerNotifier {
                actor_id: id,
                shared: Arc::clone(&self.shared),
            },
            // Commence à false : l'acteur est dans la ready queue au spawn,
            // donc pas idle. Le scheduler le mettra à true quand il passera idle.
            is_idle: AtomicBool::new(false),
        });
        attach_wake_handle(actor_ref, Arc::clone(&wake_handle));

        actor.on_start();

        let wrapper = ActorWrapper { actor, mailbox };
        let priority = wrapper.priority();

        {
            let mut actors = self.shared.actors.lock().unwrap();
            actors.insert(
                id,
                ActorSlot {
                    actor: Some(Box::new(wrapper)),
                    name,
                    wake_handle,
                },
            );
        }

        {
            let mut queue = self.shared.ready_queue.lock().unwrap();
            queue.push(ReadyEntry {
                priority,
                actor_id: id,
            });
        }

        self.shared.events.emit(SchedulerEvent::ActorSpawned {
            actor_id: id,
            actor_name: name,
            mailbox_capacity: capacity,
        });

        id
    }

    pub fn start(&self) -> SchedulerHandle {
        let mut threads = Vec::with_capacity(self.num_threads);
        for thread_index in 0..self.num_threads {
            let shared = Arc::clone(&self.shared);
            let handle = std::thread::Builder::new()
                .name(format!("scheduler-{thread_index}"))
                .spawn(move || {
                    run_loop(&shared, thread_index);
                })
                .expect("failed to spawn scheduler thread");
            threads.push(handle);
        }
        SchedulerHandle {
            threads,
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn run_one_step(&self) {
        run_one_step_impl(&self.shared);
    }

    pub fn is_single_threaded(&self) -> bool {
        self.num_threads <= 1
    }

    pub fn subscribe_events(&self) -> EventReceiver {
        self.shared.events.subscribe()
    }
}

// ---------------------------------------------------------------------------
// SchedulerHandle
// ---------------------------------------------------------------------------

pub(crate) struct SchedulerHandle {
    threads: Vec<JoinHandle<()>>,
    shared: Arc<SharedState>,
}

impl SchedulerHandle {
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.work_available.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.do_shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// run_loop
// ---------------------------------------------------------------------------

fn run_loop(shared: &SharedState, thread_index: usize) {
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }

        let actor_id = pop_ready_actor(shared, thread_index);
        let Some(actor_id) = actor_id else {
            continue;
        };

        // Prendre l'acteur OUT du HashMap.
        let (mut actor_box, name) = {
            let mut actors = shared.actors.lock().unwrap();
            let slot = match actors.get_mut(&actor_id) {
                Some(s) => s,
                None => continue,
            };
            match slot.actor.take() {
                Some(actor) => (actor, slot.name),
                None => continue, // Déjà pris (doublon dans la ready queue)
            }
        };

        let result = handle_batch(shared, actor_id, name, &mut actor_box);

        match result {
            BatchResult::Stopped => {
                let mut actors = shared.actors.lock().unwrap();
                actors.remove(&actor_id);
                shared.events.emit(SchedulerEvent::ActorStopped {
                    actor_id,
                    actor_name: name,
                });
            }
            BatchResult::HasMore => {
                let priority = actor_box.priority();
                {
                    let mut actors = shared.actors.lock().unwrap();
                    if let Some(slot) = actors.get_mut(&actor_id) {
                        slot.actor = Some(actor_box);
                        // Pas idle — on ne touche pas le flag.
                    }
                }
                {
                    let mut queue = shared.ready_queue.lock().unwrap();
                    queue.push(ReadyEntry {
                        priority,
                        actor_id,
                    });
                }
                shared.work_available.notify_one();
            }
            BatchResult::Idle => {
                {
                    let mut actors = shared.actors.lock().unwrap();
                    if let Some(slot) = actors.get_mut(&actor_id) {
                        slot.actor = Some(actor_box);
                        // Remettre le flag idle → le prochain send() réveillera.
                        slot.wake_handle.is_idle.store(true, Ordering::Release);
                    }
                }
                shared.events.emit(SchedulerEvent::ActorIdle {
                    actor_id,
                    actor_name: name,
                });
            }
        }
    }
}

fn pop_ready_actor(shared: &SharedState, thread_index: usize) -> Option<ActorId> {
    let mut queue = shared.ready_queue.lock().unwrap();
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return None;
        }
        match queue.pop() {
            Some(entry) => return Some(entry.actor_id),
            None => {
                shared
                    .events
                    .emit(SchedulerEvent::ThreadParked { thread_index });
                queue = shared.work_available.wait(queue).unwrap();
                shared
                    .events
                    .emit(SchedulerEvent::ThreadUnparked { thread_index });
            }
        }
    }
}

enum BatchResult {
    Stopped,
    HasMore,
    Idle,
}

fn handle_batch(
    shared: &SharedState,
    actor_id: ActorId,
    actor_name: &'static str,
    actor: &mut Box<dyn AnyActor>,
) -> BatchResult {
    let priority_before = actor.priority();

    for _ in 0..BATCH_SIZE {
        let start = Instant::now();

        match actor.try_handle_one() {
            Some(status) => {
                shared.events.emit(SchedulerEvent::MessageHandled {
                    actor_id,
                    actor_name,
                    duration: start.elapsed(),
                    mailbox_depth: actor.mailbox_len(),
                    priority: actor.priority(),
                });

                match status {
                    ActorStatus::Stop => return BatchResult::Stopped,
                    ActorStatus::Yield => break,
                    ActorStatus::Continue => {}
                }
            }
            None => {
                match actor.poll_idle() {
                    Poll::Ready(()) => {} // Encore du travail interne
                    Poll::Pending => {
                        emit_priority_change(shared, actor_id, actor_name, priority_before, actor);
                        return BatchResult::Idle;
                    }
                }
            }
        }
    }

    emit_priority_change(shared, actor_id, actor_name, priority_before, actor);

    if actor.has_pending() || actor.poll_idle().is_ready() {
        BatchResult::HasMore
    } else {
        BatchResult::Idle
    }
}

fn emit_priority_change(
    shared: &SharedState,
    actor_id: ActorId,
    actor_name: &'static str,
    priority_before: Priority,
    actor: &Box<dyn AnyActor>,
) {
    let priority_after = actor.priority();
    if priority_before != priority_after {
        shared.events.emit(SchedulerEvent::PriorityChanged {
            actor_id,
            actor_name,
            from: priority_before,
            to: priority_after,
        });
    }
}

// ---------------------------------------------------------------------------
// run_one_step — pour Reply::wait_cooperative en mode single-thread
// ---------------------------------------------------------------------------

fn run_one_step_impl(shared: &SharedState) {
    let entry = {
        let mut queue = shared.ready_queue.lock().unwrap();
        queue.pop()
    };

    let Some(entry) = entry else {
        std::thread::yield_now();
        return;
    };

    let actor_id = entry.actor_id;

    let (mut actor_box, name) = {
        let mut actors = shared.actors.lock().unwrap();
        let slot = match actors.get_mut(&actor_id) {
            Some(s) => s,
            None => return,
        };
        match slot.actor.take() {
            Some(actor) => (actor, slot.name),
            None => return,
        }
    };

    // Traiter UN SEUL message (rendre la main vite en mode coopératif).
    let start = Instant::now();
    let (stopped, idle) = match actor_box.try_handle_one() {
        Some(status) => {
            shared.events.emit(SchedulerEvent::MessageHandled {
                actor_id,
                actor_name: name,
                duration: start.elapsed(),
                mailbox_depth: actor_box.mailbox_len(),
                priority: actor_box.priority(),
            });
            match status {
                ActorStatus::Stop => (true, false),
                ActorStatus::Yield | ActorStatus::Continue => (false, false),
            }
        }
        None => match actor_box.poll_idle() {
            Poll::Ready(()) => (false, false),
            Poll::Pending => (false, true),
        },
    };

    if stopped {
        let mut actors = shared.actors.lock().unwrap();
        actors.remove(&actor_id);
        shared.events.emit(SchedulerEvent::ActorStopped {
            actor_id,
            actor_name: name,
        });
    } else {
        let mut actors = shared.actors.lock().unwrap();
        if let Some(slot) = actors.get_mut(&actor_id) {
            slot.actor = Some(actor_box);
            if idle {
                slot.wake_handle.is_idle.store(true, Ordering::Release);
            }
        }
        drop(actors);

        if !idle {
            let mut queue = shared.ready_queue.lock().unwrap();
            queue.push(ReadyEntry {
                priority: entry.priority,
                actor_id,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use std::sync::atomic::AtomicU32;

    struct Counter {
        count: u32,
    }

    enum CounterMsg {
        Inc,
        Get(Reply<u32>),
        Stop,
    }

    impl Actor for Counter {
        type Msg = CounterMsg;

        fn name(&self) -> &'static str {
            "counter"
        }

        fn handle(&mut self, msg: CounterMsg) -> ActorStatus {
            match msg {
                CounterMsg::Inc => {
                    self.count += 1;
                    ActorStatus::Continue
                }
                CounterMsg::Get(reply) => {
                    reply.send(self.count);
                    ActorStatus::Continue
                }
                CounterMsg::Stop => ActorStatus::Stop,
            }
        }

        fn priority(&self) -> Priority {
            Priority::Medium
        }
    }

    fn spawn_counter(scheduler: &Scheduler, capacity: usize) -> (ActorRef<CounterMsg>, ActorId) {
        let (mailbox, mut actor_ref) = mailbox(capacity);
        let id = scheduler.spawn(Counter { count: 0 }, mailbox, &mut actor_ref, capacity);
        (actor_ref, id)
    }

    #[test]
    fn test_actor_counter() {
        let scheduler = Scheduler::new(1);
        let (counter_ref, _id) = spawn_counter(&scheduler, 64);
        let _handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Inc).unwrap();

        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
        assert_eq!(reply_rx.wait_blocking(), 3);
    }

    #[test]
    fn test_actor_stop() {
        let scheduler = Scheduler::new(1);
        let (counter_ref, _id) = spawn_counter(&scheduler, 64);
        let _handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Stop).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    #[test]
    fn test_multi_thread() {
        let scheduler = Scheduler::new(4);
        let (counter_ref, _id) = spawn_counter(&scheduler, 10_000);
        let _handle = scheduler.start();

        for _ in 0..1000 {
            counter_ref.send(CounterMsg::Inc).unwrap();
        }

        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
        assert_eq!(reply_rx.wait_blocking(), 1000);
    }

    #[test]
    fn test_single_thread_cooperative_reply() {
        let scheduler = Scheduler::new(1);

        let (counter_ref, _id) = spawn_counter(&scheduler, 64);

        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Inc).unwrap();

        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();

        let val = reply_rx.wait_cooperative(|| scheduler.run_one_step());
        assert_eq!(val, 2);
    }

    #[test]
    fn test_multiple_actors() {
        let scheduler = Scheduler::new(2);
        let (ref_a, _) = spawn_counter(&scheduler, 64);
        let (ref_b, _) = spawn_counter(&scheduler, 64);
        let _handle = scheduler.start();

        for _ in 0..500 {
            ref_a.send(CounterMsg::Inc).unwrap();
            ref_b.send(CounterMsg::Inc).unwrap();
        }

        let (tx_a, rx_a) = reply();
        ref_a.send(CounterMsg::Get(tx_a)).unwrap();
        assert_eq!(rx_a.wait_blocking(), 500);

        let (tx_b, rx_b) = reply();
        ref_b.send(CounterMsg::Get(tx_b)).unwrap();
        assert_eq!(rx_b.wait_blocking(), 500);
    }

    #[test]
    fn test_priority_ordering() {
        struct PrioActor {
            prio: Priority,
            actor_name: &'static str,
            log: Arc<Mutex<Vec<&'static str>>>,
        }

        enum PrioMsg {
            Ping,
        }

        impl Actor for PrioActor {
            type Msg = PrioMsg;
            fn name(&self) -> &'static str {
                self.actor_name
            }
            fn handle(&mut self, _msg: PrioMsg) -> ActorStatus {
                self.log.lock().unwrap().push(self.actor_name);
                ActorStatus::Continue
            }
            fn priority(&self) -> Priority {
                self.prio
            }
        }

        let scheduler = Scheduler::new(1);
        let log = Arc::new(Mutex::new(Vec::new()));

        let (mbox_low, mut ref_low) = mailbox(64);
        scheduler.spawn(
            PrioActor {
                prio: Priority::Low,
                actor_name: "low",
                log: Arc::clone(&log),
            },
            mbox_low,
            &mut ref_low,
            64,
        );

        let (mbox_high, mut ref_high) = mailbox(64);
        scheduler.spawn(
            PrioActor {
                prio: Priority::High,
                actor_name: "high",
                log: Arc::clone(&log),
            },
            mbox_high,
            &mut ref_high,
            64,
        );

        ref_low.send(PrioMsg::Ping).unwrap();
        ref_high.send(PrioMsg::Ping).unwrap();

        for _ in 0..10 {
            scheduler.run_one_step();
        }

        let log = log.lock().unwrap();
        if let (Some(pos_high), Some(pos_low)) = (
            log.iter().position(|&n| n == "high"),
            log.iter().position(|&n| n == "low"),
        ) {
            assert!(
                pos_high < pos_low,
                "high priority should be handled first, got: {:?}",
                *log
            );
        }
    }

    #[test]
    fn test_events_received() {
        let scheduler = Scheduler::new(1);
        let events = scheduler.subscribe_events();
        let (counter_ref, _) = spawn_counter(&scheduler, 64);
        let _handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();
        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
        let _ = reply_rx.wait_blocking();

        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut collected = Vec::new();
        while let Some(e) = events.try_recv() {
            collected.push(e);
        }

        assert!(
            collected
                .iter()
                .any(|e| matches!(e, SchedulerEvent::ActorSpawned { .. })),
            "should have ActorSpawned event"
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, SchedulerEvent::MessageHandled { .. })),
            "should have MessageHandled events"
        );
    }

    #[test]
    fn test_zero_cost_no_subscriber() {
        let scheduler = Scheduler::new(1);
        let (counter_ref, _) = spawn_counter(&scheduler, 64);
        let _handle = scheduler.start();

        for _ in 0..10_000 {
            counter_ref.send(CounterMsg::Inc).unwrap();
        }
        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
        assert_eq!(reply_rx.wait_blocking(), 10_000);
    }

    #[test]
    fn test_scheduler_drop_shutdown() {
        let scheduler = Scheduler::new(2);
        let (counter_ref, _) = spawn_counter(&scheduler, 64);
        let handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();

        drop(handle);
    }

    #[test]
    fn test_poll_idle_actor() {
        struct IdleWorker {
            remaining: u32,
            total_done: Arc<AtomicU32>,
        }

        impl Actor for IdleWorker {
            type Msg = ();

            fn name(&self) -> &'static str {
                "idle-worker"
            }

            fn handle(&mut self, _msg: ()) -> ActorStatus {
                ActorStatus::Continue
            }

            fn priority(&self) -> Priority {
                if self.remaining > 0 {
                    Priority::Low
                } else {
                    Priority::Idle
                }
            }

            fn poll_idle(&mut self) -> Poll<()> {
                if self.remaining > 0 {
                    self.remaining -= 1;
                    self.total_done.fetch_add(1, Ordering::Relaxed);
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }

        let scheduler = Scheduler::new(1);
        let done = Arc::new(AtomicU32::new(0));

        let (mbox, mut aref) = mailbox::<()>(64);
        scheduler.spawn(
            IdleWorker {
                remaining: 5,
                total_done: Arc::clone(&done),
            },
            mbox,
            &mut aref,
            64,
        );

        for _ in 0..20 {
            scheduler.run_one_step();
        }

        assert_eq!(done.load(Ordering::Relaxed), 5);
    }
}
