# Plan d'implémentation : Architecture Actor

Référence design : `06-design-actor-architecture.md`

## Phase 1 : Fondations — Scheduler + traits Actor

### Objectif

Écrire le framework actor minimal : traits, mailbox, scheduler. Pas encore de portage
de code existant — juste les fondations testées unitairement.

### Nouveau fichier : `src/actor/mod.rs`

```rust
pub mod mailbox;
pub mod scheduler;
pub mod reply;

pub use mailbox::{Mailbox, ActorRef};
pub use reply::{Reply, ReplyReceiver};
pub use scheduler::{Scheduler, SchedulerHandle, ActorId, Priority};
```

### 1.1 — Trait Actor + types de base

**Fichier** : `src/actor/mod.rs`

```rust
use std::task::Poll;

pub trait Actor: Send + 'static {
    type Msg: Send + 'static;

    /// Traite un message. Retourne le status souhaité.
    fn handle(&mut self, msg: Self::Msg) -> ActorStatus;

    /// Priorité courante (recalculée après chaque handle).
    fn priority(&self) -> Priority;

    /// Travail interne quand la mailbox est vide.
    /// Ex: MergerActor avance son merge incrémental.
    /// Par défaut : rien à faire.
    fn poll_idle(&mut self) -> Poll<()> {
        Poll::Pending
    }

    /// Appelé une seule fois quand l'acteur est spawné.
    /// Permet d'initialiser avec accès au scheduler.
    fn on_start(&mut self) {}
}

pub enum ActorStatus {
    /// Continuer à traiter les messages
    Continue,
    /// Yield au scheduler — l'acteur a du travail mais cède pour équité
    Yield,
    /// L'acteur a terminé, le retirer du scheduler
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Idle     = 0,
    Low      = 1,
    Medium   = 2,
    High     = 3,
    Critical = 4,
}
```

### 1.2 — Mailbox + ActorRef

**Fichier** : `src/actor/mailbox.rs`

La mailbox est un wrapper autour d'un crossbeam bounded channel. FIFO strict.

```rust
use crossbeam_channel as channel;

pub struct Mailbox<M> {
    receiver: channel::Receiver<M>,
}

impl<M> Mailbox<M> {
    pub fn try_recv(&self) -> Option<M> {
        self.receiver.try_recv().ok()
    }

    pub fn has_pending(&self) -> bool {
        !self.receiver.is_empty()
    }

    /// Nombre de messages en attente (pour monitoring/debug).
    pub fn len(&self) -> usize {
        self.receiver.len()
    }
}

#[derive(Clone)]
pub struct ActorRef<M> {
    sender: channel::Sender<M>,
}

impl<M> ActorRef<M> {
    pub fn send(&self, msg: M) -> Result<(), channel::SendError<M>> {
        self.sender.send(msg)
    }

    /// Tente d'envoyer sans bloquer. Utile pour fire-and-forget
    /// quand le channel est plein (backpressure).
    pub fn try_send(&self, msg: M) -> Result<(), channel::TrySendError<M>> {
        self.sender.try_send(msg)
    }
}

/// Crée une paire (Mailbox, ActorRef) avec un channel bounded.
pub fn mailbox<M>(capacity: usize) -> (Mailbox<M>, ActorRef<M>) {
    let (sender, receiver) = channel::bounded(capacity);
    (Mailbox { receiver }, ActorRef { sender })
}
```

**Capacité par défaut** : on reprend `PIPELINE_MAX_SIZE_IN_DOCS = 10_000` pour
l'IndexerActor. Les autres acteurs auront des capacités plus petites (64 ou 128).

### 1.3 — Reply + ReplyReceiver

**Fichier** : `src/actor/reply.rs`

Remplace `FutureResult` pour le pattern request/reply.

```rust
use crossbeam_channel as channel;

/// Côté acteur : envoie la réponse.
pub struct Reply<T> {
    sender: channel::Sender<T>,
}

/// Côté appelant : attend la réponse.
pub struct ReplyReceiver<T> {
    receiver: channel::Receiver<T>,
}

impl<T> Reply<T> {
    pub fn send(self, value: T) {
        let _ = self.sender.send(value);
    }
}

impl<T> ReplyReceiver<T> {
    /// Attente bloquante (mode multi-thread).
    pub fn wait_blocking(self) -> T {
        self.receiver.recv().expect("actor died without replying")
    }

    /// Attente non-bloquante. Retourne None si pas encore de réponse.
    pub fn try_recv(&self) -> Option<T> {
        self.receiver.try_recv().ok()
    }

    /// Attente coopérative (mode single-thread).
    /// Fait tourner le scheduler entre chaque tentative.
    pub fn wait_cooperative<F>(self, mut run_step: F) -> T
    where
        F: FnMut(),
    {
        loop {
            match self.receiver.try_recv() {
                Ok(value) => return value,
                Err(_) => run_step(),
            }
        }
    }
}

/// Crée une paire (Reply, ReplyReceiver).
/// Utilise un channel bounded(1) — une seule réponse.
pub fn reply<T>() -> (Reply<T>, ReplyReceiver<T>) {
    let (sender, receiver) = channel::bounded(1);
    (Reply { sender }, ReplyReceiver { receiver })
}
```

**Choix** : on utilise `crossbeam_channel::bounded(1)` au lieu de `oneshot` pour
rester cohérent (une seule dépendance channel). Le overhead est négligeable.

### 1.4 — Scheduler

**Fichier** : `src/actor/scheduler.rs`

Le scheduler est le coeur du système. Il gère un pool de threads et une priority
queue d'acteurs prêts.

```rust
pub struct Scheduler {
    num_threads: usize,
    // La ready-queue et les acteurs sont derrière un Mutex partagé
    // entre les threads du pool.
    shared: Arc<SharedState>,
}

struct SharedState {
    /// Priority queue : acteurs triés par priorité décroissante.
    /// Chaque entrée = (Priority, ActorId).
    ready_queue: Mutex<BinaryHeap<ReadyEntry>>,
    /// Stockage des acteurs (type-erased via Box<dyn AnyActor>).
    actors: Mutex<HashMap<ActorId, ActorSlot>>,
    /// Condvar pour réveiller les threads parkés.
    work_available: Condvar,
    /// Flag global pour shutdown propre.
    shutdown: AtomicBool,
}
```

**Type erasure** : Le scheduler doit stocker des acteurs de types différents.
On utilise un trait object `dyn AnyActor` :

```rust
/// Trait object qui wrappe un Actor<Msg=M> + sa Mailbox<M>.
trait AnyActor: Send {
    /// Tente de traiter un message. Retourne false si mailbox vide.
    fn try_handle_one(&mut self) -> Option<ActorStatus>;
    /// Priorité courante.
    fn priority(&self) -> Priority;
    /// A des messages en attente ?
    fn has_pending(&self) -> bool;
    /// Travail interne ?
    fn poll_idle(&mut self) -> Poll<()>;
}

/// Implémentation concrète pour un Actor<Msg=M>.
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
}
```

**API principale du Scheduler** :

```rust
impl Scheduler {
    pub fn new(num_threads: usize) -> Self;

    /// Spawn un acteur. Retourne l'ActorRef pour lui envoyer des messages.
    /// `capacity` = taille du channel bounded de la mailbox.
    pub fn spawn<A: Actor>(
        &self,
        actor: A,
        capacity: usize,
    ) -> ActorRef<A::Msg>;

    /// Lance les threads du pool. Non-bloquant — les threads tournent
    /// en background. Retourne un SchedulerHandle pour shutdown.
    pub fn start(&self) -> SchedulerHandle;

    /// Exécute un seul step (pour Reply::wait_cooperative en single-thread).
    pub fn run_one_step(&self);

    pub fn is_single_threaded(&self) -> bool {
        self.num_threads <= 1
    }
}

/// Handle retourné par start(). Drop = shutdown + join tous les threads.
pub struct SchedulerHandle {
    threads: Vec<JoinHandle<()>>,
    shared: Arc<SharedState>,
}

impl Drop for SchedulerHandle {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.work_available.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
```

**run_loop de chaque thread** :

```rust
fn run_loop(shared: &SharedState) {
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }

        // 1. Pop l'acteur le plus prioritaire qui a du travail
        let actor_id = {
            let mut queue = shared.ready_queue.lock().unwrap();
            loop {
                match queue.pop() {
                    Some(entry) => {
                        // Vérifier que l'acteur a encore du travail
                        let actors = shared.actors.lock().unwrap();
                        if actors[&entry.actor_id].has_pending()
                           || actors[&entry.actor_id].poll_idle().is_ready()
                        {
                            break Some(entry.actor_id);
                        }
                        // Sinon, l'acteur est idle — ne pas le remettre
                    }
                    None => {
                        // Rien dans la queue → park
                        shared.work_available.wait(&mut queue);
                        if shared.shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        break None;
                    }
                }
            }
        };

        let Some(actor_id) = actor_id else { continue };

        // 2. Prendre le lock sur l'acteur et traiter un batch
        let mut actors = shared.actors.lock().unwrap();
        let slot = actors.get_mut(&actor_id).unwrap();

        const BATCH_SIZE: usize = 32;
        let mut stopped = false;

        for _ in 0..BATCH_SIZE {
            match slot.actor.try_handle_one() {
                Some(ActorStatus::Continue) => {}
                Some(ActorStatus::Yield) => break,
                Some(ActorStatus::Stop) => { stopped = true; break; }
                None => {
                    // Mailbox vide — tenter poll_idle
                    match slot.actor.poll_idle() {
                        Poll::Ready(()) => {}
                        Poll::Pending => break,
                    }
                }
            }
        }

        if stopped {
            actors.remove(&actor_id);
        } else if slot.actor.has_pending()
               || slot.actor.poll_idle().is_ready()
        {
            // Encore du travail → remettre dans la ready queue
            let priority = slot.actor.priority();
            drop(actors);
            shared.ready_queue.lock().unwrap().push(ReadyEntry {
                priority,
                actor_id,
            });
            shared.work_available.notify_one();
        }
        // Sinon : acteur idle, il sera réveillé quand un message arrive
    }
}
```

### 1.5 — Réveil sur envoi de message

Quand un `ActorRef::send()` envoie un message, il faut réveiller le scheduler
si l'acteur était idle. On utilise un callback enregistré sur le sender :

```rust
pub fn spawn<A: Actor>(&self, actor: A, capacity: usize) -> ActorRef<A::Msg> {
    let (mailbox, actor_ref) = mailbox(capacity);
    let id = self.next_id();
    let wrapper = ActorWrapper { actor, mailbox };

    self.shared.actors.lock().unwrap().insert(id, ActorSlot {
        actor: Box::new(wrapper),
    });

    // Enregistrer un notifier : quand le premier message arrive
    // dans une mailbox vide, on met l'acteur dans la ready queue.
    let shared = self.shared.clone();
    let notifier_ref = actor_ref.clone();
    // Le notifier est appelé par un thread dédié ou via un wrapper
    // autour du sender... (voir discussion ci-dessous)

    actor_ref
}
```

**Question de design : comment notifier le scheduler sur send ?**

Crossbeam ne supporte pas de callback sur send. Deux options :

**Option A** : Wrapper autour de `ActorRef::send()` qui, après le send, vérifie
si la mailbox était vide (len passé de 0 à 1) et notifie le scheduler.

```rust
impl<M> ActorRef<M> {
    pub fn send(&self, msg: M) -> Result<(), SendError<M>> {
        let was_empty = self.receiver_len.load(Ordering::Relaxed) == 0;
        self.sender.send(msg)?;
        if was_empty {
            self.notify_scheduler();
        }
        Ok(())
    }
}
```

**Option B** : Le scheduler poll périodiquement toutes les mailboxes (simple mais
moins réactif).

**Recommandation** : Option A. Un `AtomicUsize` par mailbox pour tracker la longueur
approximative (pas besoin d'être exact — un faux positif réveille le scheduler
pour rien, un faux négatif est rattrapé au prochain poll).

### 1.6 — Events + Observabilité

**Fichier** : `src/actor/events.rs`

L'observabilité fait partie des fondations, pas une feature ajoutée après coup.
Sans elle, on debug le scheduler à l'aveugle — exactement le piège de la complexité
conceptuelle qu'on veut éviter.

**Principe** : le scheduler émet des events structurés à chaque action significative.
Zero-cost quand personne n'écoute (check atomique).

```rust
use std::time::Duration;
use super::{ActorId, Priority};

/// Events émis par le scheduler.
/// Chaque event porte assez de contexte pour reconstruire l'histoire
/// complète du scheduling sans avoir besoin de logs dans les acteurs.
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    /// Un acteur a traité un message
    MessageHandled {
        actor_id: ActorId,
        actor_name: &'static str,
        duration: Duration,
        mailbox_depth: usize,
        priority: Priority,
    },
    /// Un acteur change de priorité (après handle)
    PriorityChanged {
        actor_id: ActorId,
        actor_name: &'static str,
        from: Priority,
        to: Priority,
    },
    /// Un acteur passe idle (mailbox vide, pas de travail interne)
    ActorIdle {
        actor_id: ActorId,
        actor_name: &'static str,
    },
    /// Un acteur est réveillé (message arrive dans mailbox vide)
    ActorWoken {
        actor_id: ActorId,
        actor_name: &'static str,
        woken_by: WakeReason,
    },
    /// Un thread du pool se park (rien à faire)
    ThreadParked { thread_index: usize },
    /// Un thread du pool est unparked
    ThreadUnparked { thread_index: usize },
    /// Un acteur s'arrête (ActorStatus::Stop)
    ActorStopped {
        actor_id: ActorId,
        actor_name: &'static str,
    },
    /// Un acteur est spawné
    ActorSpawned {
        actor_id: ActorId,
        actor_name: &'static str,
        mailbox_capacity: usize,
    },
}

#[derive(Debug, Clone)]
pub enum WakeReason {
    /// Un message a été envoyé via ActorRef::send
    MessageReceived,
    /// poll_idle a retourné Ready
    IdleWork,
}
```

**EventBus** : channel broadcast avec subscriber count atomique.

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use crossbeam_channel as channel;

pub struct EventBus {
    /// Nombre de subscribers actifs. Si 0, les events ne sont pas émis.
    subscriber_count: AtomicUsize,
    /// Sender partagé — tous les threads du scheduler écrivent dessus.
    sender: channel::Sender<SchedulerEvent>,
    /// Template receiver pour créer des clones (crossbeam MPMC).
    receiver: channel::Receiver<SchedulerEvent>,
}

/// Handle retourné par subscribe(). Reçoit tous les events.
pub struct EventReceiver {
    receiver: channel::Receiver<SchedulerEvent>,
    bus: Arc<EventBus>,
}

impl EventBus {
    pub fn new() -> Self {
        // Channel unbounded — les events ne doivent pas bloquer le scheduler.
        // Si le consumer est lent, les events s'accumulent (backlog).
        // On peut ajouter un bounded + drop policy plus tard si besoin.
        let (sender, receiver) = channel::unbounded();
        EventBus {
            subscriber_count: AtomicUsize::new(0),
            sender,
            receiver,
        }
    }

    /// Zero-cost check : est-ce que quelqu'un écoute ?
    #[inline]
    pub fn has_subscribers(&self) -> bool {
        self.subscriber_count.load(Ordering::Relaxed) > 0
    }

    /// Émet un event. No-op si personne n'écoute.
    #[inline]
    pub fn emit(&self, event: SchedulerEvent) {
        if self.has_subscribers() {
            let _ = self.sender.send(event);
        }
    }

    /// Subscribe aux events.
    pub fn subscribe(&self) -> EventReceiver {
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        EventReceiver {
            receiver: self.receiver.clone(), // crossbeam MPMC clone
            bus: Arc::new(/* ... */),
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.bus.subscriber_count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Iterator for EventReceiver {
    type Item = SchedulerEvent;
    fn next(&mut self) -> Option<SchedulerEvent> {
        self.receiver.recv().ok()
    }
}
```

**Intégration dans le Scheduler** :

```rust
struct SharedState {
    ready_queue: Mutex<BinaryHeap<ReadyEntry>>,
    actors: Mutex<HashMap<ActorId, ActorSlot>>,
    work_available: Condvar,
    shutdown: AtomicBool,
    events: EventBus,  // ← ajouté
}

impl Scheduler {
    /// Subscribe aux events du scheduler.
    pub fn subscribe_events(&self) -> EventReceiver {
        self.shared.events.subscribe()
    }
}
```

**Intégration dans le run_loop** — les events sont émis aux points clés :

```rust
fn run_loop(shared: &SharedState, thread_index: usize) {
    loop {
        // ... pop actor from ready_queue ...

        let Some(actor_id) = actor_id else {
            shared.events.emit(SchedulerEvent::ThreadParked { thread_index });
            // park...
            shared.events.emit(SchedulerEvent::ThreadUnparked { thread_index });
            continue;
        };

        let mut actors = shared.actors.lock().unwrap();
        let slot = actors.get_mut(&actor_id).unwrap();
        let priority_before = slot.actor.priority();

        for _ in 0..BATCH_SIZE {
            let start = Instant::now();

            match slot.actor.try_handle_one() {
                Some(status) => {
                    // Émettre l'event MessageHandled
                    shared.events.emit(SchedulerEvent::MessageHandled {
                        actor_id,
                        actor_name: slot.name,
                        duration: start.elapsed(),
                        mailbox_depth: slot.actor.mailbox_len(),
                        priority: slot.actor.priority(),
                    });

                    match status {
                        ActorStatus::Stop => {
                            shared.events.emit(SchedulerEvent::ActorStopped {
                                actor_id, actor_name: slot.name,
                            });
                            actors.remove(&actor_id);
                            break;
                        }
                        ActorStatus::Yield => break,
                        ActorStatus::Continue => {}
                    }
                }
                None => { /* poll_idle... */ break; }
            }
        }

        // Détecter un changement de priorité
        let priority_after = slot.actor.priority();
        if priority_before != priority_after {
            shared.events.emit(SchedulerEvent::PriorityChanged {
                actor_id,
                actor_name: slot.name,
                from: priority_before,
                to: priority_after,
            });
        }

        // Si l'acteur n'a plus de travail → idle
        if !slot.actor.has_pending() && slot.actor.poll_idle().is_pending() {
            shared.events.emit(SchedulerEvent::ActorIdle {
                actor_id, actor_name: slot.name,
            });
        }

        // ... remettre dans ready_queue si travail restant ...
    }
}
```

**Intégration dans ActorRef::send** — émettre `ActorWoken` :

```rust
impl<M> ActorRef<M> {
    pub fn send(&self, msg: M) -> Result<(), SendError<M>> {
        let was_empty = /* ... */;
        self.sender.send(msg)?;
        if was_empty {
            self.notify_scheduler();
            // Le scheduler émet ActorWoken dans notify_scheduler()
        }
        Ok(())
    }
}
```

**Modifications au trait AnyActor** — ajouter le nom et la taille mailbox :

```rust
trait AnyActor: Send {
    fn try_handle_one(&mut self) -> Option<ActorStatus>;
    fn priority(&self) -> Priority;
    fn has_pending(&self) -> bool;
    fn poll_idle(&mut self) -> Poll<()>;
    fn name(&self) -> &'static str;       // ← ajouté
    fn mailbox_len(&self) -> usize;        // ← ajouté
}
```

Le `name` vient du trait `Actor` :

```rust
pub trait Actor: Send + 'static {
    type Msg: Send + 'static;

    /// Nom lisible pour les events/debug.
    fn name(&self) -> &'static str;

    fn handle(&mut self, msg: Self::Msg) -> ActorStatus;
    fn priority(&self) -> Priority;
    fn poll_idle(&mut self) -> Poll<()> { Poll::Pending }
}
```

**Exemples d'utilisation** :

```rust
// Debug : print tous les events
let events = scheduler.subscribe_events();
std::thread::spawn(move || {
    for event in events {
        eprintln!("[scheduler] {:?}", event);
    }
});

// Test : vérifier qu'un acteur a traité N messages
let events = scheduler.subscribe_events();
// ... do work ...
let handled_count = events
    .into_iter()
    .take_while(|e| !matches!(e, SchedulerEvent::ActorStopped { .. }))
    .filter(|e| matches!(e, SchedulerEvent::MessageHandled { actor_name: "indexer", .. }))
    .count();

// Monitoring : agréger les durées de handle par acteur
let events = scheduler.subscribe_events();
std::thread::spawn(move || {
    let mut stats: HashMap<&str, (u64, Duration)> = HashMap::new();
    for event in events {
        if let SchedulerEvent::MessageHandled { actor_name, duration, .. } = event {
            let entry = stats.entry(actor_name).or_default();
            entry.0 += 1;
            entry.1 += duration;
        }
    }
});
```

### Fichier créé

```
src/actor/events.rs    (~120 lignes)
```

### 1.7 — Tests unitaires Phase 1

```rust
#[cfg(test)]
mod tests {
    /// Test basique : un acteur compteur
    #[test]
    fn test_actor_counter() {
        struct Counter { count: u32 }
        enum CounterMsg { Inc, Get(Reply<u32>) }

        impl Actor for Counter {
            type Msg = CounterMsg;
            fn handle(&mut self, msg: CounterMsg) -> ActorStatus {
                match msg {
                    CounterMsg::Inc => { self.count += 1; ActorStatus::Continue }
                    CounterMsg::Get(reply) => {
                        reply.send(self.count);
                        ActorStatus::Continue
                    }
                }
            }
            fn priority(&self) -> Priority { Priority::Medium }
        }

        let scheduler = Scheduler::new(1);
        let counter_ref = scheduler.spawn(Counter { count: 0 }, 64);
        let _handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Inc).unwrap();
        counter_ref.send(CounterMsg::Inc).unwrap();

        let (reply, receiver) = reply();
        counter_ref.send(CounterMsg::Get(reply)).unwrap();
        assert_eq!(receiver.wait_blocking(), 3);
    }

    /// Test priorité : High passe avant Low
    #[test]
    fn test_priority_ordering();

    /// Test single-thread : Reply::wait_cooperative ne deadlock pas
    #[test]
    fn test_single_thread_reply();

    /// Test multi-thread : N acteurs sur M threads
    #[test]
    fn test_multi_thread_fairness();

    /// Test shutdown propre
    #[test]
    fn test_scheduler_drop_shutdown();

    /// Test events : subscribe + vérifier que les events arrivent
    #[test]
    fn test_events_received() {
        let scheduler = Scheduler::new(1);
        let events = scheduler.subscribe_events();
        let counter_ref = scheduler.spawn(Counter { count: 0 }, 64);
        let _handle = scheduler.start();

        counter_ref.send(CounterMsg::Inc).unwrap();
        let (reply, receiver) = reply();
        counter_ref.send(CounterMsg::Get(reply)).unwrap();
        let _ = receiver.wait_blocking();

        // Collecter les events
        // On doit voir: ActorSpawned, ActorWoken, MessageHandled × 2
        let collected: Vec<_> = events
            .into_iter()
            .take(4)
            .collect();
        assert!(collected.iter().any(|e|
            matches!(e, SchedulerEvent::ActorSpawned { .. })
        ));
        assert_eq!(
            collected.iter()
                .filter(|e| matches!(e, SchedulerEvent::MessageHandled { .. }))
                .count(),
            2
        );
    }

    /// Test zero-cost : pas de subscriber = pas d'overhead
    #[test]
    fn test_events_zero_cost_no_subscriber() {
        let scheduler = Scheduler::new(1);
        // PAS de subscribe_events()
        let counter_ref = scheduler.spawn(Counter { count: 0 }, 64);
        let _handle = scheduler.start();

        // Les events ne sont pas émis, pas d'allocation
        for _ in 0..10_000 {
            counter_ref.send(CounterMsg::Inc).unwrap();
        }
        let (reply, receiver) = reply();
        counter_ref.send(CounterMsg::Get(reply)).unwrap();
        assert_eq!(receiver.wait_blocking(), 10_000);
        // Si on arrive ici sans OOM, les events n'ont pas été buffer
    }
}
```

### Fichiers créés en Phase 1

```
src/actor/
  mod.rs           (~70 lignes)   — trait Actor, ActorStatus, Priority, re-exports
  mailbox.rs       (~70 lignes)   — Mailbox, ActorRef, fn mailbox()
  reply.rs         (~60 lignes)   — Reply, ReplyReceiver, fn reply()
  events.rs        (~120 lignes)  — SchedulerEvent, EventBus, EventReceiver
  scheduler.rs     (~300 lignes)  — Scheduler, SharedState, AnyActor, run_loop
```

**Total estimé** : ~620 lignes de code + ~200 lignes de tests.

L'observabilité ajoute ~180 lignes (events.rs + intégration dans scheduler.rs)
mais c'est un investissement dès le départ qui économise des heures de debug
sur les phases suivantes.

---

## Phase 2 : IndexerActor — portage de worker_loop

### Objectif

Remplacer `worker_loop` + les `WorkerSender`/`FlushSender`/`WorkerMessage` par
un `IndexerActor` qui implémente le trait `Actor`. L'`IndexWriter` possède des
`ActorRef<IndexerMsg>` au lieu de channels bruts.

### 2.1 — Définir IndexerMsg

**Fichier** : `src/indexer/index_writer.rs` (ou nouveau `src/indexer/indexer_actor.rs`)

```rust
use crate::actor::{Reply, ActorRef};
use crate::indexer::AddBatch;
use crate::schema::document::Document;

pub(crate) enum IndexerMsg<D: Document> {
    /// Batch de documents à indexer.
    Docs(AddBatch<D>),
    /// Flush le segment en cours et répondre quand c'est fait.
    Flush(Reply<crate::Result<()>>),
    /// Arrêt propre.
    Shutdown,
}
```

C'est essentiellement le `WorkerMessage` actuel + le `FlushSender` fusionnés en
un seul enum. **Plus besoin de deux channels séparés.**

### 2.2 — Implémenter IndexerActor

```rust
pub(crate) struct IndexerActor<D: Document> {
    segment_updater: SegmentUpdater,  // pour l'instant on garde l'ancien
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    bomb: IndexWriterBomb<D>,
    /// Segment en cours d'écriture (None si idle)
    current: Option<SegmentInProgress>,
}

struct SegmentInProgress {
    segment: Segment,
    writer: SegmentWriter,
}

impl<D: Document> Actor for IndexerActor<D> {
    type Msg = IndexerMsg<D>;

    fn handle(&mut self, msg: IndexerMsg<D>) -> ActorStatus {
        match msg {
            IndexerMsg::Docs(batch) => self.handle_docs(batch),
            IndexerMsg::Flush(reply) => self.handle_flush(reply),
            IndexerMsg::Shutdown => self.handle_shutdown(),
        }
    }

    fn priority(&self) -> Priority {
        if self.current.is_some() {
            Priority::High  // segment ouvert = mémoire allouée
        } else {
            Priority::Low   // idle
        }
    }
}
```

**handle_docs** — quasi copier-coller du code actuel :

```rust
fn handle_docs(&mut self, batch: AddBatch<D>) -> ActorStatus {
    if batch.is_empty() {
        return ActorStatus::Continue;
    }

    // Créer le segment si c'est le premier batch
    let current = match &mut self.current {
        Some(c) => c,
        None => {
            self.delete_cursor.skip_to(batch[0].opstamp);
            let segment = self.index.new_segment();
            let writer = SegmentWriter::for_segment(
                self.mem_budget, segment.clone()
            ).expect("segment writer creation"); // TODO: error handling
            self.current = Some(SegmentInProgress { segment, writer });
            self.current.as_mut().unwrap()
        }
    };

    for doc in batch {
        current.writer.add_document(doc).expect("add_document");
    }

    // Vérifier le budget mémoire
    if current.writer.mem_usage() >= self.mem_budget - MARGIN_IN_BYTES {
        self.finalize_current_segment();
        // current est maintenant None, priorité repasse à Low
    }

    ActorStatus::Continue
}
```

**handle_flush** — plus besoin de drain ! La mailbox est FIFO, tous les Docs
précédents ont déjà été traités :

```rust
fn handle_flush(&mut self, reply: Reply<crate::Result<()>>) -> ActorStatus {
    // Tous les Docs envoyés avant ce Flush ont déjà été traités
    // par des appels handle_docs() précédents (FIFO garanti).
    // Il suffit de finaliser le segment en cours.
    let result = self.finalize_current_segment();
    reply.send(result);
    ActorStatus::Continue
}
```

C'est **la grande simplification** : le bug #2 (select! aléatoire + drain) disparaît
structurellement. Le handle_flush fait 4 lignes au lieu de 30.

**handle_shutdown** :

```rust
fn handle_shutdown(&mut self) -> ActorStatus {
    let _ = self.finalize_current_segment();
    self.bomb.defuse();
    ActorStatus::Stop
}
```

**finalize_current_segment** — helper, identique à `finalize_segment` actuel :

```rust
fn finalize_current_segment(&mut self) -> crate::Result<()> {
    if let Some(current) = self.current.take() {
        if self.segment_updater.is_alive() {
            finalize_segment(
                current.segment,
                current.writer,
                &self.segment_updater,
                &mut self.delete_cursor,
            )?;
        }
    }
    Ok(())
}
```

### 2.3 — Modifier IndexWriter

**Changements dans la struct** :

```rust
pub struct IndexWriter<D: Document = LucivyDocument> {
    // AVANT:
    //   operation_sender: WorkerSender<D>,
    //   worker_flush_senders: Vec<FlushSender>,
    //   workers_join_handle: Vec<JoinHandle<crate::Result<()>>>,
    //
    // APRÈS:
    worker_refs: Vec<ActorRef<IndexerMsg<D>>>,
    scheduler_handle: SchedulerHandle,
    // ... le reste ne change pas
}
```

**prepare_commit** — simplifié, plus de flush_senders séparés :

```rust
pub fn prepare_commit(&mut self) -> crate::Result<PreparedCommit<'_, D>> {
    let mut receivers = Vec::new();
    for worker in &self.worker_refs {
        let (reply, receiver) = reply();
        worker.send(IndexerMsg::Flush(reply))
            .map_err(|_| self.harvest_worker_error())?;
        receivers.push(receiver);
    }
    for receiver in receivers {
        // En single-thread: wait_cooperative fait tourner le scheduler
        // En multi-thread: wait_blocking classique
        let result = if self.scheduler.is_single_threaded() {
            receiver.wait_cooperative(|| self.scheduler.run_one_step())
        } else {
            receiver.wait_blocking()
        };
        result.map_err(|e| LucivyError::ErrorInThread(format!("{e}")))?;
    }
    let commit_opstamp = self.stamper.stamp();
    Ok(PreparedCommit::new(self, commit_opstamp))
}
```

**Drop** :

```rust
impl<D: Document> Drop for IndexWriter<D> {
    fn drop(&mut self) {
        self.segment_updater.kill();
        for worker in &self.worker_refs {
            let _ = worker.send(IndexerMsg::Shutdown);
        }
        // SchedulerHandle::drop() join les threads automatiquement
    }
}
```

### 2.4 — Supprimer l'ancien code

- Supprimer `worker_loop()` (144 lignes)
- Supprimer `WorkerMessage`, `WorkerSender`, `WorkerReceiver` de `mod.rs`
- Supprimer `FlushSender`, `FlushReceiver` de `mod.rs`
- Supprimer `harvest_worker_error()` (le scheduler gère)
- Simplifier `IndexWriterStatus` (plus besoin de stocker le WorkerReceiver)

### 2.5 — Validation

Les **1118 tests existants** doivent passer sans modification. L'API publique
(`add_document`, `commit`, `rollback`) ne change pas. Seule l'implémentation interne
change.

Tests spécifiques à ajouter :
- Test IndexerActor en isolation (sans IndexWriter)
- Test single-thread commit (Scheduler::new(1))
- Proptests en mode single-thread

### Estimation Phase 2

- **Lignes supprimées** : ~250 (worker_loop + types channels + harvest_worker_error)
- **Lignes ajoutées** : ~200 (IndexerActor + modifications IndexWriter)
- **Lignes modifiées** : ~50 (prepare_commit, Drop, rollback, add_indexing_worker)
- **Bilan net** : ~-50 lignes — le code se simplifie

---

## Phase 3 : SegmentUpdaterActor

### Objectif

Porter `InnerSegmentUpdater` vers un acteur. Supprimer le rayon pool single-thread
pour le segment_updater (garder rayon pour les merges en attendant Phase 4).

### 3.1 — SegmentUpdaterMsg

```rust
pub(crate) enum SegmentUpdaterMsg {
    AddSegment(SegmentEntry, Reply<crate::Result<()>>),
    Commit {
        opstamp: Opstamp,
        payload: Option<String>,
        reply: Reply<crate::Result<()>>,
    },
    GarbageCollect(Reply<crate::Result<GarbageCollectionResult>>),
    StartMerge(MergeOperation),
    MergeComplete {
        segment_entries: Vec<SegmentEntry>,
        result: crate::Result<Option<SegmentMeta>>,
        reply: Reply<crate::Result<Option<SegmentMeta>>>,
    },
    Kill,
}
```

### 3.2 — Portage

Le corps de chaque handler est copié depuis les méthodes existantes de
`InnerSegmentUpdater`. La logique métier ne change pas — seule l'enveloppe change
(de `schedule_task(closure)` vers `handle(msg)`).

**Ce qui change** :

| Avant (rayon) | Après (acteur) |
|---------------|----------------|
| `schedule_add_segment()` crée une closure, la spawn sur le pool, retourne FutureResult | `AddSegment(entry, reply)` → handler direct, `reply.send(result)` |
| `schedule_commit()` idem | `Commit { reply, .. }` → handler direct |
| `schedule_garbage_collect()` idem | `GarbageCollect(reply)` → handler direct |
| `start_merge()` spawn sur merge_thread_pool | `StartMerge(op)` → spawn sur rayon merge pool (inchangé pour l'instant) |

**Ce qui ne change PAS** :
- `apply_deletes`, `compute_merge_candidates`, `segment_manager` — logique métier intacte
- Le merge_thread_pool rayon reste (Phase 4 le remplacera)

### 3.3 — Remplacement de FutureResult

Partout où on utilisait `FutureResult<T>` comme valeur de retour, on utilise
`ReplyReceiver<T>`. L'API interne change mais l'API publique reste identique
(les méthodes publiques qui retournaient `FutureResult` retournent `ReplyReceiver`
ou appellent `.wait()` directement).

### Estimation Phase 3

- **Lignes à refactorer** : ~200 dans segment_updater.rs
- **Lignes de FutureResult à supprimer** : 130 (tout le fichier)
- **Complexité** : Moyenne — c'est un portage mécanique, la logique métier ne change pas

---

## Phase 4 : Mode Single-Thread (intégration)

### Objectif

Faire passer toute la suite de tests avec `Scheduler::new(1)`.

### 4.1 — ThreadBudget API

```rust
pub enum ThreadBudget {
    /// Tout sur 1 seul thread (WASM sans SharedArrayBuffer)
    Single,
    /// N threads, le scheduler distribue les acteurs
    Threads(usize),
}

impl Index {
    pub fn writer_with_thread_budget<D: Document>(
        &self,
        budget: ThreadBudget,
        memory_budget: usize,
    ) -> crate::Result<IndexWriter<D>> {
        let num_threads = match budget {
            ThreadBudget::Single => 1,
            ThreadBudget::Threads(n) => n,
        };
        // Calcul du nombre de workers indexer en fonction du budget
        let num_workers = match budget {
            ThreadBudget::Single => 1,
            ThreadBudget::Threads(n) => (n - 1).max(1).min(8),
            // -1 car le SegmentUpdater prend un slot
        };
        // ... créer Scheduler(num_threads), spawner les acteurs ...
    }
}
```

### 4.2 — wait coopératif

Toutes les attentes de `ReplyReceiver` dans `IndexWriter` doivent utiliser
`wait_cooperative` en mode single-thread. Ce sont les points :

1. `prepare_commit` → attend Flush replies des IndexerActors
2. `rollback` → attend Flush replies (sync)
3. `merge` (appel explicite) → attend le résultat du merge

### 4.3 — Tests

- Lancer `cargo test` avec un `writer_for_tests` qui utilise `Scheduler::new(1)`
- Si les proptests passent en single-thread, l'architecture est validée
- Ajouter un feature flag ou une env var pour basculer les tests en mode single-thread

### Estimation Phase 4

- **Lignes** : ~50 de modifications (surtout les appels wait)
- **Complexité** : Faible SI les phases 1-3 sont solides

---

## Résumé des phases suivantes (non détaillées)

### Phase 5 : MergerActor (remplace rayon merge pool)

- Porter les closures merge → MergerActor
- Optionnel : merge incrémental pour le mode single-thread
- Sans merge incrémental : le merge tourne en blocking dans son slot de scheduler
  (acceptable car les merges sont déclenchés entre les commits)

### Phase 6 : CompressorActor + WatcherActor

- Porter les derniers threads éphémères
- Zéro `thread::spawn` dans le hot path

### Phase 7 : Optimisations

- Work-stealing entre threads
- Affinité acteur-thread (éviter les migrations inutiles)
- Monitoring (métriques par acteur : latence, throughput, queue depth)

---

## Dépendances à supprimer/réduire après le refactor complet

| Dépendance | Utilisée par | Supprimable ? |
|------------|-------------|---------------|
| `rayon` | executor.rs (search), segment_updater, merges | Après Phase 5 : oui pour segment_updater et merges. Search parallèle garde rayon (c'est un pattern différent) |
| `crossbeam-channel` | Partout | **Non** — on continue de l'utiliser, c'est la base des Mailbox |
| `oneshot` (futures-channel) | FutureResult | **Oui** après Phase 3 — remplacé par Reply |
