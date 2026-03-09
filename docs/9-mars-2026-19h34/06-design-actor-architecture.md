# Design : Architecture Actor + Scheduling Coopératif

## Motivation

Aujourd'hui ld-lucivy a 6 types de threads avec 4 patterns de communication différents :
- crossbeam MPMC + select! (indexer workers)
- rayon ThreadPool + FutureResult/oneshot (segment_updater, merges)
- std::sync::mpsc::sync_channel (docstore compressor)
- Atomic state polling (file watcher)

On veut :
1. **Unifier** autour d'un modèle acteur avec mailbox typée
2. **Rendre le nombre de threads configurable** — de 1 (WASM single-thread) à N (natif)
3. **Scheduling coopératif** — les acteurs cèdent le contrôle entre messages, permettant
   de multiplexer plusieurs acteurs sur peu de threads

## Vision : Acteurs sur un Scheduler

```
┌─────────────────────────────────────────────────────┐
│                   Scheduler(N)                       │
│  N = nombre de OS threads alloués                    │
│                                                      │
│  Thread 0          Thread 1          Thread 2        │
│  ┌──────────┐     ┌──────────┐     ┌──────────┐     │
│  │ run_loop │     │ run_loop │     │ run_loop │     │
│  │  ┌─────┐ │     │  ┌─────┐ │     │  ┌─────┐ │     │
│  │  │Act A│ │     │  │Act C│ │     │  │Act E│ │     │
│  │  │Act B│ │     │  │Act D│ │     │  │Act F│ │     │
│  │  └─────┘ │     │  └─────┘ │     │  └─────┘ │     │
│  └──────────┘     └──────────┘     └──────────┘     │
└─────────────────────────────────────────────────────┘
```

Chaque **run_loop** :
1. Cherche un acteur qui a des messages en attente
2. Appelle `actor.handle_one()` — traite UN message
3. L'acteur peut yield (coopératif) ou continuer
4. Retour au scheduler pour le prochain acteur prêt

## Les Acteurs de ld-lucivy

### 1. IndexerActor (× num_workers)

Remplace le `worker_loop` actuel.

```rust
struct IndexerActor<D: Document> {
    mailbox: Mailbox<IndexerMsg<D>>,
    segment_updater: ActorRef<SegmentUpdaterMsg>,
    index: Index,
    mem_budget: usize,
    delete_cursor: DeleteCursor,
    // État du segment en cours (None si idle)
    current_segment: Option<SegmentInProgress>,
}

enum IndexerMsg<D: Document> {
    Docs(AddBatch<D>),
    Flush(Reply<Result<()>>),
    Shutdown,
}

struct SegmentInProgress {
    segment: Segment,
    writer: SegmentWriter,
}
```

**Comportement** :
- `Docs(batch)` → Si pas de segment en cours, en créer un + `skip_to`.
  Ajouter les docs au segment_writer. Si budget mémoire atteint, finaliser
  et envoyer `AddSegment` au SegmentUpdaterActor.
- `Flush(reply)` → Drainer la mailbox de tous les `Docs` restants (important !),
  finaliser le segment en cours, répondre `Ok(())`.
- `Shutdown` → Finaliser si segment en cours, puis arrêter.

**Priorité de scheduling** : Haute quand segment en cours (mémoire utilisée),
basse quand idle.

### 2. SegmentUpdaterActor (× 1)

Remplace le rayon single-thread pool du `SegmentUpdater`.

```rust
struct SegmentUpdaterActor {
    mailbox: Mailbox<SegmentUpdaterMsg>,
    index: Index,
    segment_manager: SegmentManager,
    merge_policy: Arc<dyn MergePolicy>,
    stamper: Stamper,
    merge_operations: MergeOperationInventory,
}

enum SegmentUpdaterMsg {
    AddSegment(SegmentEntry, Reply<Result<()>>),
    Commit(Opstamp, Option<String>, Reply<Result<()>>),
    GarbageCollect(Reply<Result<GarbageCollectionResult>>),
    StartMerge(MergeOperation, Reply<Result<Option<SegmentMeta>>>),
    MergeComplete(MergeResult),
    Kill,
}
```

**Comportement** :
- `AddSegment` → Enregistrer le segment, répondre. Déclencher merge si nécessaire
  (envoi `StartMerge` à un MergerActor via le scheduler).
- `Commit` → Appliquer les deletes pendantes, mettre à jour le meta, répondre.
- `StartMerge` → Demander au scheduler de créer/réutiliser un MergerActor.

**Priorité** : Moyenne (pas bloquant pour l'indexation sauf au commit).

### 3. MergerActor (× num_merge_threads, poolé)

Remplace les closures sur `merge_thread_pool`.

```rust
struct MergerActor {
    mailbox: Mailbox<MergerMsg>,
    index: Index,
}

enum MergerMsg {
    Merge(MergeOperation, Reply<Result<Option<SegmentMeta>>>),
}
```

**Comportement** :
- `Merge(op, reply)` → Exécuter le merge (CPU-intensif). Répondre avec le résultat.
  **Yield périodique** : le merge peut yield au scheduler toutes les N itérations
  pour ne pas monopoliser un thread.

**Priorité** : Basse (background task). En mode 1-thread, les merges sont différés
jusqu'à ce que l'indexation soit idle.

### 4. CompressorActor (× 1, optionnel)

Remplace le `DedicatedThreadBlockCompressor` éphémère.

```rust
struct CompressorActor {
    mailbox: Mailbox<CompressorMsg>,
}

enum CompressorMsg {
    CompressBlock(BlockData, Reply<Result<CompressedBlock>>),
    Shutdown,
}
```

**Priorité** : Basse (peut être différé si 1 thread).

### 5. WatcherActor (× 1, optionnel)

Remplace le file watcher polling thread.

```rust
struct WatcherActor {
    mailbox: Mailbox<WatcherMsg>,
    watch_callbacks: Vec<WatchCallback>,
}

enum WatcherMsg {
    Tick,  // schedulé périodiquement par le scheduler
    Register(WatchCallback),
}
```

**Priorité** : Très basse. En mode 1-thread, tick seulement quand idle.

## Deux Niveaux de Scheduling

Il y a deux problèmes de priorité distincts qu'il ne faut pas confondre :

1. **Dans quelle ordre traiter les messages d'un acteur ?** → politique de la Mailbox
2. **Quel acteur faire tourner en premier ?** → politique du Scheduler (priority queue)

### Mailbox : FIFO (pas de priority queue)

Les messages dans une mailbox sont traités en **FIFO** — l'ordre d'envoi est respecté.
C'est crucial pour la correction :

```
Timeline d'envoi vers IndexerActor:
  t=1  Docs(batch_1)     ← envoyé par add_document()
  t=2  Docs(batch_2)     ← envoyé par add_document()
  t=3  Docs(batch_3)     ← envoyé par add_document()
  t=4  Flush(reply)      ← envoyé par prepare_commit()

Ordre de traitement (FIFO):
  → Docs(batch_1)  → Docs(batch_2)  → Docs(batch_3)  → Flush(reply)
```

Le Flush arrive **naturellement après** tous les Docs parce qu'il a été envoyé après.
C'est la causalité de l'envoi qui garantit l'ordre — pas besoin de priority queue,
pas besoin du hack `try_recv` drain qu'on a dû écrire dans l'implémentation actuelle
avec `crossbeam::select!`.

**Pourquoi notre bug #2 disparaît** : Aujourd'hui le worker écoute sur DEUX channels
(doc + flush) via `select!` qui choisit aléatoirement. Avec un acteur à mailbox unique
FIFO, il n'y a qu'un seul channel. Le Flush est derrière les Docs dans la queue.
Le problème n'existe plus structurellement.

**Exception** : `Shutdown` devrait court-circuiter la FIFO (pas besoin de traiter
les 10 000 docs restants si on veut kill). Option : flag externe `AtomicBool` que
l'acteur check en début de `handle()`, ou un drain explicite dans le handler de Shutdown.

```rust
struct Mailbox<M> {
    /// FIFO channel — l'ordre d'envoi est l'ordre de traitement
    receiver: crossbeam_channel::Receiver<M>,
}
```

### Scheduler : Priority Queue d'acteurs

Le scheduler maintient une **priority queue** qui décide **quel acteur** exécuter
quand plusieurs ont des messages en attente. C'est ici que la priorité intervient.

```
Priority queue du Scheduler (BinaryHeap):
  ┌───────────────────────────────────────────────────────┐
  │  priority=Critical │ SegmentUpdaterActor (commit wait)│  ← en premier
  │  priority=High     │ IndexerActor #0 (segment ouvert) │
  │  priority=Medium   │ SegmentUpdaterActor (idle)        │
  │  priority=Low      │ MergerActor #0 (merge en cours)   │
  │  priority=Idle     │ WatcherActor                      │
  └───────────────────────────────────────────────────────┘
```

La priorité est **dynamique** — elle change selon l'état de l'acteur :

```rust
impl Actor for IndexerActor {
    fn priority(&self) -> Priority {
        if self.current_segment.is_some() {
            // Segment ouvert = mémoire allouée → à traiter vite
            Priority::High
        } else {
            Priority::Low
        }
    }
}

impl Actor for SegmentUpdaterActor {
    fn priority(&self) -> Priority {
        if self.has_pending_commit_reply() {
            // Un commit() attend → critique
            Priority::Critical
        } else {
            Priority::Medium
        }
    }
}

impl Actor for MergerActor {
    fn priority(&self) -> Priority {
        // Toujours basse — les merges sont background
        Priority::Low
    }
}
```

### Résumé : qui décide quoi

```
                    ┌──────────────┐
                    │  Scheduler   │
                    │ (prio queue) │  ← "quel acteur tourne maintenant?"
                    └──────┬───────┘
                           │ pop l'acteur de plus haute priorité
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │ Acteur A │ │ Acteur B │ │ Acteur C │
        │ [FIFO  ] │ │ [FIFO  ] │ │ [FIFO  ] │  ← "dans quel ordre
        │ msg1     │ │ msg1     │ │ msg1     │     traiter MES messages?"
        │ msg2     │ │ msg2     │ │          │
        │ msg3     │ │          │ │          │
        └──────────┘ └──────────┘ └──────────┘
```

- La **priority queue du scheduler** trie les acteurs : l'IndexerActor avec un segment
  ouvert passe avant le MergerActor.
- La **FIFO de chaque mailbox** trie les messages : les Docs passent avant le Flush
  parce qu'ils ont été envoyés avant.

Ce sont deux mécanismes orthogonaux qui se combinent.

## Le Scheduler

### Trait Actor

```rust
trait Actor: Send + 'static {
    type Msg: Send + 'static;

    /// Traite un message. Retourne si l'acteur veut continuer.
    fn handle(&mut self, msg: Self::Msg) -> ActorStatus;

    /// Priorité courante (peut changer dynamiquement).
    /// Appelé par le scheduler après chaque handle() pour repositionner
    /// l'acteur dans la priority queue.
    fn priority(&self) -> Priority;

    /// Appelé quand la mailbox est vide et l'acteur est idle.
    /// Retourne Poll::Ready si l'acteur a du travail interne
    /// (ex: MergerActor avec un merge incrémental en cours).
    fn poll_idle(&mut self) -> Poll<()> { Poll::Pending }
}

enum ActorStatus {
    Continue,
    /// L'acteur veut yield — il a du travail mais cède pour équité
    Yield,
    /// L'acteur a terminé, le scheduler peut le retirer
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    /// WatcherActor en attente de tick
    Idle = 0,
    /// Merges, compression — différable
    Low = 1,
    /// Segment updater idle — pas urgent
    Medium = 2,
    /// Indexer workers avec segment ouvert — mémoire allouée
    High = 3,
    /// Un Reply est en attente — l'appelant bloque dessus
    Critical = 4,
}
```

### Mailbox et ActorRef

```rust
struct Mailbox<M> {
    /// FIFO channel — causalité respectée
    receiver: crossbeam_channel::Receiver<M>,
}

impl<M> Mailbox<M> {
    fn try_recv(&self) -> Option<M> {
        self.receiver.try_recv().ok()
    }

    fn has_pending(&self) -> bool {
        !self.receiver.is_empty()
    }
}

/// Handle pour envoyer des messages à un acteur
#[derive(Clone)]
struct ActorRef<M> {
    sender: crossbeam_channel::Sender<M>,
}

impl<M> ActorRef<M> {
    /// Fire-and-forget
    fn send(&self, msg: M) -> Result<(), SendError<M>> {
        self.sender.send(msg)
    }

    /// Request-reply : envoie un message qui contient un Reply,
    /// attend la réponse. Le constructeur du message est passé en closure.
    fn ask<T, F>(&self, scheduler: &Scheduler, make_msg: F) -> T
    where
        F: FnOnce(Reply<T>) -> M,
    {
        let (reply, receiver) = Reply::new();
        self.sender.send(make_msg(reply)).expect("actor dead");
        receiver.wait(scheduler)
    }
}

/// Handle oneshot pour les réponses request/reply
struct Reply<T> {
    sender: oneshot::Sender<T>,
}

struct ReplyReceiver<T> {
    receiver: oneshot::Receiver<T>,
}

impl<T> Reply<T> {
    fn new() -> (Reply<T>, ReplyReceiver<T>) {
        let (tx, rx) = oneshot::channel();
        (Reply { sender: tx }, ReplyReceiver { receiver: rx })
    }

    fn send(self, value: T) {
        let _ = self.sender.send(value);
    }
}

impl<T> ReplyReceiver<T> {
    /// Bloque en attendant la réponse.
    /// En mode multi-thread : simple recv().
    /// En mode single-thread : fait tourner le scheduler pour éviter deadlock.
    fn wait(self, scheduler: &Scheduler) -> T {
        if scheduler.is_single_threaded() {
            loop {
                match self.receiver.try_recv() {
                    Ok(value) => return value,
                    Err(_) => scheduler.run_one_step(),
                }
            }
        } else {
            self.receiver.recv().expect("actor died without replying")
        }
    }
}
```

### Scheduler Core

```rust
struct Scheduler {
    /// Priority queue d'acteurs prêts (triés par priority() décroissante)
    ready_queue: Mutex<BinaryHeap<ActorEntry>>,
    /// Tous les acteurs enregistrés (pour park/unpark)
    actors: Vec<AnyActor>,
    /// Nombre de OS threads du pool
    num_threads: usize,
    /// Condition variable pour réveiller les threads en park
    work_available: Condvar,
}

/// Entrée dans la priority queue
struct ActorEntry {
    priority: Priority,
    actor_id: ActorId,
}

impl Ord for ActorEntry {
    // Plus haute priorité = sort en premier du BinaryHeap
}

impl Scheduler {
    fn new(num_threads: usize) -> Self;

    /// Enregistre un acteur, retourne son ActorRef
    fn spawn<A: Actor>(&mut self, actor: A) -> ActorRef<A::Msg>;

    /// Lance les run_loops sur N threads. Bloquant.
    fn run(&mut self);

    fn is_single_threaded(&self) -> bool { self.num_threads == 1 }

    /// Exécute un step (mode single-thread, appelé par Reply::wait)
    fn run_one_step(&self);
}
```

**Algorithme du run_loop** (par thread) :

```
loop {
    // 1. Pop l'acteur de plus haute priorité avec messages
    actor = ready_queue.lock().pop();

    if actor.is_none() {
        // Pas de travail → park le thread
        work_available.wait();
        continue;
    }

    // 2. Traiter un batch de messages (max BATCH_SIZE pour équité)
    for _ in 0..BATCH_SIZE {
        match actor.mailbox.try_recv() {
            Some(msg) => {
                match actor.handle(msg) {
                    Continue => {}
                    Yield => break,
                    Stop => { remove_actor(); break; }
                }
            }
            None => {
                // Mailbox vide — vérifier si l'acteur a du travail interne
                match actor.poll_idle() {
                    Poll::Ready(()) => {}    // encore du travail
                    Poll::Pending => break,  // vraiment idle
                }
            }
        }
    }

    // 3. Recalculer la priorité et remettre dans la queue
    let new_priority = actor.priority();
    ready_queue.lock().push(ActorEntry {
        priority: new_priority,
        actor_id: actor.id,
    });

    // 4. Réveiller un thread si d'autres acteurs attendent
    if ready_queue.lock().len() > 0 {
        work_available.notify_one();
    }
}
```

**Quand un message arrive dans une mailbox** (via `ActorRef::send`) :
- Le scheduler vérifie si l'acteur est idle (pas dans la ready_queue)
- Si oui → recalcule sa priorité et l'insère dans la ready_queue
- Notify un thread parké via `work_available.notify_one()`

Cela garantit que :
- Un acteur idle est réveillé dès qu'il reçoit un message
- La priority queue est toujours triée par priorité courante
- Les threads ne spin-loopent pas (park/unpark via Condvar)

## Scénarios de Thread Budget

### N = 8 (natif, serveur)

```
Thread 0: IndexerActor #0
Thread 1: IndexerActor #1
Thread 2: SegmentUpdaterActor
Thread 3: MergerActor #0
Thread 4: MergerActor #1
Thread 5: MergerActor #2
Thread 6: MergerActor #3
Thread 7: CompressorActor + WatcherActor (multiplexés)
```

Comportement quasi-identique à aujourd'hui — chaque acteur a quasiment son thread dédié.
Le scheduling est round-robin au sein d'un thread, mais comme chaque thread a peu
d'acteurs, c'est essentiellement 1:1.

### N = 2 (WASM avec SharedArrayBuffer)

```
Thread 0: IndexerActor #0 + SegmentUpdaterActor
Thread 1: MergerActor #0 + CompressorActor
```

L'indexation et les segment updates partagent un thread. Les merges et la compression
partagent l'autre. Le scheduler donne priorité à l'indexation (High) sur les updates
(Medium). Le merge (Low) cède quand le compressor a du travail.

### N = 1 (WASM sans SharedArrayBuffer, ou budget minimal)

```
Thread 0: TOUT
  IndexerActor → SegmentUpdaterActor → MergerActor → CompressorActor
```

Tout est coopératif sur un seul thread. Le scheduling par priorité garantit :
1. Les docs sont indexés en premier (High)
2. Les segments sont enregistrés (Medium)
3. Les merges tournent en background (Low) — yield toutes les N itérations
4. La compression est différée (Low)

**Point clé** : Le `commit()` reste bloquant du point de vue de l'API. Quand
`prepare_commit` envoie un `Flush`, le scheduler sait qu'il doit traiter les messages
de l'IndexerActor (Flush + drain) PUIS du SegmentUpdaterActor (AddSegment + Commit)
avant de rendre la main. Sur 1 thread, c'est une boucle synchrone.

## Pattern Reply et Commit sur 1 Thread

Le problème classique des acteurs single-thread : le deadlock.

```
IndexWriter::commit()
  → envoie Flush à IndexerActor
  → attend Reply
  → MAIS IndexerActor est sur le même thread
  → DEADLOCK
```

**Solution : le scheduler exécute des messages pendant l'attente d'une Reply.**

```rust
impl<T> Reply<T> {
    /// Bloque en attendant la réponse, mais laisse le scheduler
    /// tourner d'autres acteurs pendant ce temps.
    fn wait(self, scheduler: &Scheduler) -> T {
        loop {
            match self.receiver.try_recv() {
                Ok(value) => return value,
                Err(_) => {
                    // Pas encore de réponse → faire tourner le scheduler
                    scheduler.run_one_step();
                }
            }
        }
    }
}
```

En mode multi-thread, `wait()` peut simplement bloquer (`recv()`).
En mode single-thread, `wait()` fait tourner le scheduler pour éviter le deadlock.

## Séquence d'un Commit (mode 1 thread)

```
1. IndexWriter::commit()
2.   → prepare_commit() envoie Flush(reply_tx) à IndexerActor
3.   → reply.wait(scheduler):
4.     scheduler.run_one_step()
5.       → IndexerActor.handle(Flush):
6.         drain mailbox for Docs
7.         finalize_segment → envoie AddSegment(reply2_tx) à SegmentUpdaterActor
8.         reply2.wait(scheduler):
9.           scheduler.run_one_step()
10.            → SegmentUpdaterActor.handle(AddSegment):
11.              enregistre segment, reply2_tx.send(Ok(()))
12.        ← reply2 reçu
13.        reply_tx.send(Ok(()))
14.  ← reply reçu
15.  schedule_commit → envoie Commit(reply3_tx)
16.  reply3.wait(scheduler):
17.    scheduler.run_one_step()
18.      → SegmentUpdaterActor.handle(Commit): apply deletes, save meta
19.      reply3_tx.send(Ok(()))
20.  ← reply3 reçu
21. commit terminé ✓
```

Tout est synchrone et déterministe en mode 1 thread. Pas de deadlock car `wait()`
fait tourner le scheduler.

## Gestion du Merge Coopératif

Le merge est l'opération la plus longue (lecture + réécriture de segments). Pour
qu'il ne bloque pas l'indexation en mode 1-thread, le merge doit être découpable :

```rust
impl Actor for MergerActor {
    fn handle(&mut self, msg: MergerMsg) -> ActorStatus {
        match msg {
            MergerMsg::Merge(op, reply) => {
                // Sauvegarder l'état du merge en cours
                self.current_merge = Some(MergeInProgress {
                    operation: op,
                    reply,
                    state: MergeState::Starting,
                });
                // Premier pas, le scheduler rappellera via poll_idle
                ActorStatus::Yield
            }
        }
    }

    fn poll_idle(&mut self) -> Poll<()> {
        if let Some(merge) = &mut self.current_merge {
            // Avancer le merge d'un "chunk"
            match merge.state.step() {
                MergeStep::Continue => Poll::Ready(()),  // encore du travail
                MergeStep::Done(result) => {
                    merge.reply.send(result);
                    self.current_merge = None;
                    Poll::Pending  // plus rien
                }
            }
        } else {
            Poll::Pending
        }
    }
}
```

Ça nécessite de refactorer `merge()` pour être incrémental, mais c'est faisable :
le merge itère sur des segments et écrit des blocs. On peut yield entre chaque bloc.

## Phases d'Implémentation

### Phase 1 : Fondations (scheduler + traits)

- `Actor` trait, `Mailbox<M>`, `ActorRef<M>`, `Reply<T>`
- `Scheduler` basique : round-robin, multi-thread
- Pas encore de priorités, pas de work-stealing
- Tests unitaires du scheduler

### Phase 2 : IndexerActor

- Porter `worker_loop` vers `impl Actor for IndexerActor`
- Le `IndexWriter` possède les `ActorRef<IndexerMsg>` au lieu de `WorkerSender`/`FlushSender`
- `prepare_commit` envoie `Flush(reply)` et attend
- Tests : tous les proptests doivent passer

### Phase 3 : SegmentUpdaterActor

- Porter `InnerSegmentUpdater` + son rayon pool vers un acteur
- `FutureResult` → `Reply<T>` unifié
- Supprimer la dépendance rayon pour le segment_updater (garder pour merge)

### Phase 4 : MergerActor

- Porter les closures de merge vers des acteurs
- Merge incrémental (step-by-step) pour le scheduling coopératif
- En mode N-threads, un MergerActor par thread merge
- En mode 1-thread, un seul MergerActor qui yield entre chaque bloc

### Phase 5 : Mode single-thread

- `Reply::wait()` avec scheduler intégré
- Tests en mode `Scheduler::new(1)` — tout passe sur 1 thread
- Benchmark : overhead du scheduling coopératif vs threads dédiés

### Phase 6 : CompressorActor + WatcherActor

- Porter les derniers threads éphémères
- Le WatcherActor utilise `Tick` périodique du scheduler
- Zéro `thread::spawn` dans le hot path

### Phase 7 : Priorités + Work-Stealing

- Scheduling par priorité dynamique
- Work-stealing entre threads (acteurs migrables)
- Tuning pour les cas WASM (1-2 threads) vs natif (4-8 threads)

## API Publique

L'API utilisateur ne change pas :

```rust
let index = Index::create_in_ram(schema);
let mut writer = index.writer_with_num_threads(2, 50_000_000)?;
// OU
let mut writer = index.writer_with_thread_budget(ThreadBudget::Single)?;
// OU
let mut writer = index.writer_with_thread_budget(ThreadBudget::Threads(4))?;

writer.add_document(doc)?;
writer.commit()?;
```

Sous le capot, `writer_with_thread_budget(Single)` crée un `Scheduler(1)` et tous les
acteurs tournent dessus. `writer_with_thread_budget(Threads(4))` crée un `Scheduler(4)`
et distribue les acteurs.

## Risques et Questions Ouvertes

1. **Overhead du scheduling coopératif** — Le context switch entre acteurs a un coût.
   À mesurer vs le coût des threads OS. Sur 1 thread, c'est juste des appels de fonction,
   donc quasi-gratuit.

2. **Merge incrémental** — Rendre le merge "steppable" est le plus gros refactoring.
   Le merge actuel est une grosse fonction synchrone. Il faut le découper en étapes
   avec un état intermédiaire sauvegardé entre les steps.

3. **Starvation** — En mode 1-thread, un acteur gourmand (merge) pourrait starver
   les autres. Les priorités + yield obligatoire après N steps sont la solution.

4. **Compatibilité async** — Le trait `Actor` est sync (pas de `async fn handle`).
   C'est voulu : on évite le runtime async (tokio/async-std) qui serait trop lourd
   pour WASM. Le scheduling coopératif est fait manuellement via `Yield` + `poll_idle`.

5. **Taille des messages** — `AddBatch<D>` peut être gros (SmallVec de documents).
   On pourrait utiliser `Box<AddBatch<D>>` pour éviter les copies dans les channels.
   Ou un pool de buffers réutilisables.

6. **Backpressure** — Mailbox bounded vs unbounded ? Les channels bounded créent
   de la backpressure naturelle (le producer bloque). Avec des acteurs, on peut
   aussi compter les messages en attente et ralentir le producer.
