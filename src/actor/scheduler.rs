use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Instant;

use super::events::{EventBus, EventReceiver, SchedulerEvent, WakeReason};
use super::mailbox::{attach_wake_handle, ActorRef, Mailbox, WakeHandle};
use super::{Actor, ActorStatus, Priority};

// ---------------------------------------------------------------------------
// Global scheduler — un seul pool de threads pour tout le process.
// Comme rayon::ThreadPool, initialisé lazy au premier usage.
// ---------------------------------------------------------------------------

/// Optional log hook — called for every scheduler debug event.
/// Set by the emscripten binding to route events into the SAB ring buffer.
static LOG_HOOK: Mutex<Option<Box<dyn Fn(&str) + Send>>> = Mutex::new(None);

/// Register a log hook that receives formatted scheduler event strings.
/// Called from the emscripten binding to route [sched] events into the ring buffer.
pub fn set_scheduler_log_hook<F: Fn(&str) + Send + 'static>(f: F) {
    *LOG_HOOK.lock().unwrap() = Some(Box::new(f));
}

struct GlobalSchedulerState {
    scheduler: Arc<Scheduler>,
    _handle: SchedulerHandle, // garde les threads en vie
}

static GLOBAL_SCHEDULER: OnceLock<GlobalSchedulerState> = OnceLock::new();

/// Retourne le scheduler global partagé par tout le process.
/// Initialisé lazy avec un nombre de threads = nombre de cores.
///
/// Si la variable d'environnement `LUCIVY_SCHEDULER_DEBUG=1` est définie,
/// un thread logger affiche tous les events du scheduler sur stderr.
pub(crate) fn global_scheduler() -> &'static Arc<Scheduler> {
    &GLOBAL_SCHEDULER
        .get_or_init(|| {
            let num_threads = std::env::var("LUCIVY_SCHEDULER_THREADS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(2)
                });
            eprintln!("[scheduler] starting with {num_threads} threads");
            let scheduler = Arc::new(Scheduler::new(num_threads));
            let handle = scheduler.start();

            // Debug logger — activé par env var.
            // LUCIVY_SCHEDULER_DEBUG=1 → stderr
            // LUCIVY_SCHEDULER_DEBUG=/path/to/file → fichier
            if let Ok(debug_val) = std::env::var("LUCIVY_SCHEDULER_DEBUG") {
                let events = scheduler.subscribe_events();
                std::thread::Builder::new()
                    .name("scheduler-debug".into())
                    .spawn(move || {
                        use std::io::Write;
                        let mut out: Box<dyn Write + Send> = if debug_val == "1" {
                            Box::new(std::io::stderr())
                        } else {
                            Box::new(
                                std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&debug_val)
                                    .expect("cannot open scheduler debug log"),
                            )
                        };
                        while let Some(event) = events.recv() {
                            let msg = format!("[sched] {event:?}");
                            let _ = writeln!(out, "{msg}");
                            let _ = out.flush();
                            // Also route to the log hook (ring buffer in WASM)
                            if let Ok(guard) = LOG_HOOK.lock() {
                                if let Some(ref hook) = *guard {
                                    hook(&msg);
                                }
                            }
                        }
                    })
                    .expect("failed to spawn scheduler debug thread");
            }

            GlobalSchedulerState {
                scheduler,
                _handle: handle,
            }
        })
        .scheduler
}

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
    events: Arc<EventBus<SchedulerEvent>>,
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
    pub fn actor_id(&self) -> ActorId {
        self.actor_id
    }

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
            events: Arc::clone(&self.shared.events),
            actor_name: name,
        });
        attach_wake_handle(actor_ref, Arc::clone(&wake_handle));

        actor.on_start(actor_ref.clone());

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
        self.shared.work_available.notify_one();

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

    /// Exécute un step de travail. Retourne `true` si du travail a été fait.
    pub fn run_one_step(&self) -> bool {
        run_one_step_impl(&self.shared)
    }

    pub fn is_single_threaded(&self) -> bool {
        self.num_threads <= 1
    }

    pub fn subscribe_events(&self) -> EventReceiver<SchedulerEvent> {
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
                let needs_rewake;
                {
                    let mut actors = shared.actors.lock().unwrap();
                    if let Some(slot) = actors.get_mut(&actor_id) {
                        slot.actor = Some(actor_box);
                        // Remettre le flag idle → le prochain send() réveillera.
                        slot.wake_handle.is_idle.store(true, Ordering::Release);
                        // RACE FIX: un send() a pu arriver entre has_pending()
                        // (dans handle_batch) et le store(true) ci-dessus.
                        // Ce send() a vu is_idle=false → pas de wake.
                        // On re-vérifie la mailbox pour rattraper ce cas.
                        needs_rewake = slot.actor.as_ref()
                            .map(|a| a.has_pending())
                            .unwrap_or(false);
                    } else {
                        needs_rewake = false;
                    }
                }
                if needs_rewake {
                    // Un message a glissé entre le check et le idle.
                    // Re-enqueue l'acteur (doublon possible, géré par take → None).
                    let priority = {
                        let actors = shared.actors.lock().unwrap();
                        actors.get(&actor_id)
                            .and_then(|s| s.actor.as_ref())
                            .map(|a| a.priority())
                            .unwrap_or(Priority::Medium)
                    };
                    {
                        let mut queue = shared.ready_queue.lock().unwrap();
                        queue.push(ReadyEntry { priority, actor_id });
                    }
                    shared.work_available.notify_one();
                } else {
                    shared.events.emit(SchedulerEvent::ActorIdle {
                        actor_id,
                        actor_name: name,
                    });
                }
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

    shared.events.emit(SchedulerEvent::BatchStarted {
        actor_id,
        actor_name,
        mailbox_depth: actor.mailbox_len(),
    });

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
                // Vérifier si un message est arrivé pendant le handler
                // (ex: self-message MergeStep). Si oui, break → HasMore.
                // On ne continue PAS la boucle pour ne pas monopoliser le thread.
                if actor.has_pending() {
                    break;
                }
                match actor.poll_idle() {
                    Poll::Ready(()) => break,
                    Poll::Pending => {
                        emit_priority_change(shared, actor_id, actor_name, priority_before, actor);
                        return BatchResult::Idle;
                    }
                }
            }
        }
    }

    emit_priority_change(shared, actor_id, actor_name, priority_before, actor);

    // On ne rappelle PAS poll_idle() ici — il a des side effects (merge step).
    // On retourne toujours HasMore : soit il reste des messages (batch épuisé),
    // soit on a break sur un poll_idle Ready (travail idle restant).
    // Le prochain tour de run_loop vérifiera les autres acteurs d'abord.
    BatchResult::HasMore
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

fn run_one_step_impl(shared: &SharedState) -> bool {
    let entry = {
        let mut queue = shared.ready_queue.lock().unwrap();
        queue.pop()
    };

    let Some(entry) = entry else {
        return false;
    };

    let actor_id = entry.actor_id;

    let (mut actor_box, name) = {
        let mut actors = shared.actors.lock().unwrap();
        let slot = match actors.get_mut(&actor_id) {
            Some(s) => s,
            None => return false,
        };
        match slot.actor.take() {
            Some(actor) => (actor, slot.name),
            None => return false,
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
        let needs_rewake;
        {
            let mut actors = shared.actors.lock().unwrap();
            if let Some(slot) = actors.get_mut(&actor_id) {
                slot.actor = Some(actor_box);
                if idle {
                    slot.wake_handle.is_idle.store(true, Ordering::Release);
                    // Même race fix que dans run_loop.
                    needs_rewake = slot.actor.as_ref()
                        .map(|a| a.has_pending())
                        .unwrap_or(false);
                } else {
                    needs_rewake = false;
                }
            } else {
                needs_rewake = false;
            }
        }

        if !idle || needs_rewake {
            let mut queue = shared.ready_queue.lock().unwrap();
            queue.push(ReadyEntry {
                priority: entry.priority,
                actor_id,
            });
        }
    }
    true
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
        assert_eq!(reply_rx.wait_cooperative(|| scheduler.run_one_step()), 3);
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
        assert_eq!(reply_rx.wait_cooperative(|| scheduler.run_one_step()), 1000);
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
        assert_eq!(rx_a.wait_cooperative(|| scheduler.run_one_step()), 500);

        let (tx_b, rx_b) = reply();
        ref_b.send(CounterMsg::Get(tx_b)).unwrap();
        assert_eq!(rx_b.wait_cooperative(|| scheduler.run_one_step()), 500);
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
        let _ = reply_rx.wait_cooperative(|| scheduler.run_one_step());

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
        assert_eq!(reply_rx.wait_cooperative(|| scheduler.run_one_step()), 10_000);
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

    // -----------------------------------------------------------------------
    // Stress tests — exercice des race conditions connues
    // -----------------------------------------------------------------------

    /// Stress test : N senders externes envoient en boucle vers un acteur
    /// pendant que le scheduler le traite. Cible la race TOCTOU
    /// has_pending() ↔ is_idle=true.
    #[test]
    fn stress_concurrent_sends_no_deadlock() {
        let scheduler = Scheduler::new(4);
        let (counter_ref, _id) = spawn_counter(&scheduler, 10_000);
        let _handle = scheduler.start();

        let num_senders = 8;
        let msgs_per_sender = 5_000;
        let total = num_senders * msgs_per_sender;

        let handles: Vec<_> = (0..num_senders)
            .map(|_| {
                let r = counter_ref.clone();
                std::thread::spawn(move || {
                    for _ in 0..msgs_per_sender {
                        r.send(CounterMsg::Inc).unwrap();
                        // Pas de sleep — on veut maximiser la contention.
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Attendre que tous les messages soient traités.
        let (reply_tx, reply_rx) = reply();
        counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
        let count = reply_rx.wait_cooperative(|| scheduler.run_one_step());
        assert_eq!(count, total as u32);
    }

    /// Stress test : spawn + send en boucle rapide.
    /// Cible le notify_one manquant au spawn.
    #[test]
    fn stress_rapid_spawn_and_send() {
        let scheduler = Scheduler::new(4);
        let _handle = scheduler.start();

        let total = Arc::new(AtomicU32::new(0));

        for _ in 0..50 {
            let (counter_ref, _id) = spawn_counter(&scheduler, 256);
            let t = Arc::clone(&total);

            // Envoie immédiatement après le spawn.
            for _ in 0..100 {
                counter_ref.send(CounterMsg::Inc).unwrap();
            }

            let (reply_tx, reply_rx) = reply();
            counter_ref.send(CounterMsg::Get(reply_tx)).unwrap();
            let count = reply_rx.wait_cooperative(|| scheduler.run_one_step());
            assert_eq!(count, 100);
            t.fetch_add(count, Ordering::Relaxed);

            counter_ref.send(CounterMsg::Stop).unwrap();
        }

        assert_eq!(total.load(Ordering::Relaxed), 50 * 100);
    }

    /// Stress test : self-messages (acteur s'envoie N messages à lui-même).
    /// Cible la race is_idle + self-message pendant handle().
    #[test]
    fn stress_self_messages() {
        struct SelfSender {
            remaining: u32,
            done: Arc<AtomicU32>,
            self_ref: Option<ActorRef<SelfMsg>>,
        }

        enum SelfMsg {
            Start(u32),
            Step,
            GetDone(Reply<u32>),
        }

        impl Actor for SelfSender {
            type Msg = SelfMsg;

            fn name(&self) -> &'static str {
                "self-sender"
            }

            fn handle(&mut self, msg: SelfMsg) -> ActorStatus {
                match msg {
                    SelfMsg::Start(n) => {
                        self.remaining = n;
                        if let Some(ref sr) = self.self_ref {
                            let _ = sr.send(SelfMsg::Step);
                        }
                    }
                    SelfMsg::Step => {
                        if self.remaining > 0 {
                            self.remaining -= 1;
                            self.done.fetch_add(1, Ordering::Relaxed);
                            if let Some(ref sr) = self.self_ref {
                                let _ = sr.send(SelfMsg::Step);
                            }
                        }
                    }
                    SelfMsg::GetDone(reply) => {
                        reply.send(self.done.load(Ordering::Relaxed));
                    }
                }
                ActorStatus::Continue
            }

            fn priority(&self) -> Priority {
                Priority::Medium
            }

            fn on_start(&mut self, self_ref: ActorRef<SelfMsg>) {
                self.self_ref = Some(self_ref);
            }
        }

        let scheduler = Scheduler::new(4);
        let _handle = scheduler.start();
        let done = Arc::new(AtomicU32::new(0));

        let (mbox, mut aref) = mailbox::<SelfMsg>(1_000);
        scheduler.spawn(
            SelfSender {
                remaining: 0,
                done: Arc::clone(&done),
                self_ref: None,
            },
            mbox,
            &mut aref,
            1_000,
        );

        let count = 10_000u32;
        aref.send(SelfMsg::Start(count)).unwrap();

        let (reply_tx, reply_rx) = reply();
        // Les self-messages doivent tous être traités avant le GetDone
        // (FIFO : Start, Step, Step, ..., GetDone).
        // On envoie GetDone après un court délai pour laisser les Steps s'empiler.
        std::thread::sleep(std::time::Duration::from_millis(50));
        aref.send(SelfMsg::GetDone(reply_tx)).unwrap();
        let result = reply_rx.wait_cooperative(|| scheduler.run_one_step());
        assert_eq!(result, count);
    }

    /// Stress test : N acteurs communiquent entre eux en ping-pong.
    /// Exerce le wake inter-acteur sous contention.
    #[test]
    fn stress_ping_pong_actors() {
        struct PingPong {
            partner: Option<ActorRef<PPMsg>>,
            count: u32,
        }

        enum PPMsg {
            SetPartner(ActorRef<PPMsg>),
            Ping(u32),
            GetCount(Reply<u32>),
        }

        impl Actor for PingPong {
            type Msg = PPMsg;

            fn name(&self) -> &'static str {
                "ping-pong"
            }

            fn handle(&mut self, msg: PPMsg) -> ActorStatus {
                match msg {
                    PPMsg::SetPartner(p) => {
                        self.partner = Some(p);
                    }
                    PPMsg::Ping(remaining) => {
                        self.count += 1;
                        if remaining > 0 {
                            if let Some(ref p) = self.partner {
                                let _ = p.send(PPMsg::Ping(remaining - 1));
                            }
                        }
                    }
                    PPMsg::GetCount(reply) => {
                        reply.send(self.count);
                    }
                }
                ActorStatus::Continue
            }

            fn priority(&self) -> Priority {
                Priority::Medium
            }
        }

        let scheduler = Scheduler::new(4);
        let _handle = scheduler.start();

        let (mbox_a, mut ref_a) = mailbox::<PPMsg>(1_000);
        scheduler.spawn(
            PingPong { partner: None, count: 0 },
            mbox_a,
            &mut ref_a,
            1_000,
        );

        let (mbox_b, mut ref_b) = mailbox::<PPMsg>(1_000);
        scheduler.spawn(
            PingPong { partner: None, count: 0 },
            mbox_b,
            &mut ref_b,
            1_000,
        );

        // Connecter les partenaires
        ref_a.send(PPMsg::SetPartner(ref_b.clone())).unwrap();
        ref_b.send(PPMsg::SetPartner(ref_a.clone())).unwrap();

        // Lancer le ping-pong
        let rounds = 5_000u32;
        ref_a.send(PPMsg::Ping(rounds)).unwrap();

        // Attendre la fin — total des pings = rounds + 1
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (tx_a, rx_a) = reply();
        ref_a.send(PPMsg::GetCount(tx_a)).unwrap();
        let count_a = rx_a.wait_cooperative(|| scheduler.run_one_step());

        let (tx_b, rx_b) = reply();
        ref_b.send(PPMsg::GetCount(tx_b)).unwrap();
        let count_b = rx_b.wait_cooperative(|| scheduler.run_one_step());

        // a commence avec Ping(5000), envoie à b Ping(4999),
        // b envoie à a Ping(4998), etc.
        // Total pings traités = rounds + 1
        assert_eq!(count_a + count_b, rounds + 1);
    }
}
