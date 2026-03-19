# Doc 09 — Design : luciole comme framework complet de coordination

Date : 19 mars 2026
Basé sur : docs 05-08, analyse complète de l'usage lucivy ↔ luciole

## Le constat

Aujourd'hui lucivy utilise luciole pour 6 acteurs, mais chaque acteur
réinvente des patterns que luciole pourrait fournir. Si on regarde ce que
lucivy fait *au-dessus* de luciole, on trouve des patterns récurrents
qui ne devraient pas être du code applicatif.

### Patterns réinventés dans lucivy

| Pattern | Où dans lucivy | Ce que lucivy code en plus |
|---------|----------------|---------------------------|
| **Pool de workers** | IndexerActor × N, ReaderActor × N | Round-robin manual, Vec<ActorRef>, next_worker counter |
| **Pipeline** | Reader → Router → Shard | Chaînage manuel des ActorRef, drain séquentiel des 3 stages |
| **Incremental work** | MergeStep self-messages | send(SuMergeStepMsg) à soi-même, check active_merge |
| **Drain/barrier** | drain_all_merges, PipelineDrainMsg | Busy-loop ou wait_cooperative avec compteur |
| **Fan-out/collect** | Search: dispatch N shards → merge résultats | Vec<ReplyReceiver>, collect loop, heap merge |
| **DAG structurel** | commit: merges → barrier → save → GC → reload | Pas structuré, flags et locks |
| **Contexte partagé** | Arc<SegmentUpdaterShared> partout | Passé manuellement dans chaque handler |

**Chaque pattern = du code applicatif fragile au lieu d'une primitive luciole testée.**

## La vision : 5 primitives luciole

Luciole devrait fournir **5 briques** qui couvrent 100% des usages lucivy :

```
1. Actor        — un thread logique avec mailbox (existe déjà)
2. Pool         — N actors identiques avec dispatch strategy
3. Pipeline     — stages chaînés avec backpressure et drain
4. Dag          — graphe de tâches avec parallélisme structural
5. Scope        — contexte partagé + lifecycle (drain, pause, shutdown)
```

Lucivy n'a plus qu'à déclarer ses nœuds/acteurs et les brancher.

## Primitive 1 : Actor (améliorer l'existant)

### Le problème actuel

GenericActor est puissant mais verbeux. Pour un acteur simple
(IndexerActor qui reçoit des batches), il faut :

1. Définir un struct message
2. Implémenter Message (type_tag, encode, decode)
3. Créer un GenericActor
4. Register un TypedHandler avec closure
5. Gérer ActorState manuellement
6. Envoyer via into_envelope / into_envelope_with_local

C'est 50-80 lignes de boilerplate par acteur. Le encode/decode n'est
même jamais utilisé en local (c'est pour le réseau futur).

### Ce que luciole pourrait offrir en plus

**Option A : Actor typé simple (sans sérialisation)**

Pour les cas locaux (99% de lucivy), un acteur typé directement :

```rust
// Lucivy écrit juste ça :
struct IndexerActor {
    segment: Option<SegmentWriter>,
    mem_budget: usize,
}

impl Actor for IndexerActor {
    type Msg = IndexerMsg;
    fn name(&self) -> &'static str { "indexer" }
    fn handle(&mut self, msg: IndexerMsg) -> ActorStatus { ... }
    fn priority(&self) -> Priority { ... }
}

enum IndexerMsg {
    Docs(Vec<Document>),
    Flush(Reply<()>),
    Shutdown,
}
```

Pas de `Envelope`, pas de `Message` trait, pas de `encode/decode`.
Le `mailbox` est typé directement `Mailbox<IndexerMsg>`.

C'est **le trait Actor qui existe déjà dans lib.rs** ! Sauf que personne
ne l'utilise directement — tout passe par GenericActor + Envelope. Pourquoi ?

Parce que le scheduler fait `spawn<A: Actor>` qui exige un seul `type Msg`.
GenericActor résout ça avec `Msg = Envelope` (union universelle).

**Proposition** : garder les deux modes, mais faciliter le mode typé.

Le mode Envelope/GenericActor reste pour :
- Acteurs multi-rôles (ajout/retrait de handlers dynamiques)
- Réseau futur (sérialisation)
- Acteurs qui doivent recevoir des types variés

Le mode typé (Actor<Msg=MyEnum>) pour :
- 90% des cas (un acteur = un enum de messages)
- Zero overhead (pas de Box<dyn Any>, pas de HashMap dispatch)
- Pattern match sur l'enum, exhaustivité vérifiée par le compilateur

**Ce qui manque pour le mode typé** : un helper pour le request/response.

```rust
enum IndexerMsg {
    Docs(Vec<Document>),
    Flush(Reply<Result<(), String>>),  // Reply intégré dans l'enum
    Shutdown,
}

// Côté envoi :
let (reply, rx) = reply::<Result<(), String>>();
actor_ref.send(IndexerMsg::Flush(reply))?;
let result = rx.wait_cooperative_named("flush", || sched.run_one_step());
```

Ça existe déjà ! `Reply` + `ReplyReceiver` fonctionnent avec n'importe quel type.
Le seul manque : pas de helper ergonomique côté luciole pour combiner
send + wait en une seule opération.

**Helper proposé** :

```rust
// Dans luciole :
impl<M: Send + 'static> ActorRef<M> {
    /// Send a message that contains a Reply, wait for the response.
    pub fn request<R, F>(
        &self,
        make_msg: F,
        label: &str,
    ) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(Reply<R>) -> M,
    {
        let (reply, rx) = reply::<R>();
        self.send(make_msg(reply)).map_err(|_| "actor disconnected")?;
        Ok(rx.wait_cooperative_named(label, || global_scheduler().run_one_step()))
    }
}

// Usage lucivy :
let result = actor_ref.request(
    |r| IndexerMsg::Flush(r),
    "flush_indexer",
)?;
```

**Une seule ligne pour request/response.**

## Primitive 2 : Pool

### Le problème

Lucivy crée des pools manuellement dans 3 endroits :

```rust
// index_writer.rs — IndexerActor pool
let mut workers = Vec::new();
for i in 0..num_workers {
    let (mb, mut ar) = mailbox(cap);
    scheduler.spawn(actor, mb, &mut ar, cap);
    workers.push(ar);
}
let next = AtomicUsize::new(0);

// Pour envoyer :
let idx = next.fetch_add(1, Ordering::Relaxed) % workers.len();
workers[idx].send(msg)?;
```

```rust
// sharded_handle.rs — ReaderActor pool (même pattern)
// sharded_handle.rs — ShardActor pool (même pattern, mais par shard_id pas round-robin)
```

### Ce que luciole devrait offrir

```rust
pub struct Pool<M: Send + 'static> {
    workers: Vec<ActorRef<M>>,
    strategy: DispatchStrategy,
    next: AtomicUsize,
}

pub enum DispatchStrategy {
    RoundRobin,
    LeastLoaded,    // mailbox.len() le plus bas
    KeyRouted,      // dispatch par clé (shard_id)
}

impl<M: Send + 'static> Pool<M> {
    pub fn spawn<A: Actor<Msg = M>>(
        scheduler: &Scheduler,
        count: usize,
        make_actor: impl Fn(usize) -> A,
        capacity: usize,
    ) -> Self;

    /// Send to next worker (round-robin or least-loaded)
    pub fn send(&self, msg: M) -> Result<(), String>;

    /// Send to specific worker by key
    pub fn send_to(&self, key: usize, msg: M) -> Result<(), String>;

    /// Broadcast to all workers
    pub fn broadcast(&self, make_msg: impl Fn() -> M) -> Result<(), String>;

    /// Request to one worker, wait for reply
    pub fn request<R, F>(&self, make_msg: F, label: &str) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(Reply<R>) -> M;

    /// Scatter request to all workers, collect replies
    pub fn scatter<R, F>(&self, make_msg: F, label: &str) -> Vec<R>
    where
        R: Send + 'static,
        F: Fn(Reply<R>) -> M;

    /// Drain: wait until all workers' mailboxes are empty
    pub fn drain(&self, label: &str);

    pub fn len(&self) -> usize;
}
```

**Lucivy écrirait juste :**

```rust
// Indexer pool
let indexers = Pool::spawn(&scheduler, num_workers, |i| {
    IndexerActor::new(i, mem_budget)
}, 1024);

// Insert doc
indexers.send(IndexerMsg::Docs(batch))?;

// Flush all
indexers.broadcast(|| IndexerMsg::Flush)?;

// Search scatter-gather
let results = shard_pool.scatter(
    |r| ShardMsg::Search(query.clone(), r),
    "search_shards",
);
```

### scatter — le pattern search

Le scatter-gather est le pattern le plus critique pour la search :
dispatch la query à N shards, collecter N résultats, merger.

```rust
impl<M: Send + 'static> Pool<M> {
    pub fn scatter<R, F>(&self, make_msg: F, label: &str) -> Vec<R>
    where
        R: Send + 'static,
        F: Fn(Reply<R>) -> M,
    {
        let scheduler = global_scheduler();
        let mut receivers = Vec::with_capacity(self.workers.len());
        for worker in &self.workers {
            let (reply, rx) = reply::<R>();
            worker.send(make_msg(reply)).ok();
            receivers.push(rx);
        }
        receivers.into_iter()
            .map(|rx| rx.wait_cooperative_named(label, || scheduler.run_one_step()))
            .collect()
    }
}
```

**Aujourd'hui c'est 15 lignes de code lucivy à chaque search. Avec ça : 1 ligne.**

## Primitive 3 : Pipeline

### Le problème

Le pipeline d'ingestion shardé (Reader → Router → Shard) est câblé
manuellement dans sharded_handle.rs (~200 lignes) :

1. Créer les actors
2. Stocker les refs de chaque stage
3. Chaîner : Reader envoie à Router, Router envoie à Shard
4. Drain : PipelineDrainMsg propagé stage par stage
5. Gestion des erreurs à chaque transition

### Ce que luciole devrait offrir

```rust
pub struct Pipeline {
    stages: Vec<PipelineStage>,
}

pub struct PipelineStage {
    name: &'static str,
    actors: Vec<ActorRef<Envelope>>,  // 1 ou N actors
    strategy: DispatchStrategy,
}

impl Pipeline {
    pub fn builder() -> PipelineBuilder;
    pub fn send(&self, msg: impl Into<Envelope>) -> Result<(), String>;
    pub fn drain(&self, label: &str);
    pub fn shutdown(&self);
}

pub struct PipelineBuilder {
    stages: Vec<PipelineStageConfig>,
}

impl PipelineBuilder {
    pub fn stage<A: Actor<Msg = Envelope>>(
        self,
        name: &'static str,
        count: usize,
        make_actor: impl Fn(usize) -> A,
    ) -> Self;

    pub fn build(self, scheduler: &Scheduler) -> Pipeline;
}
```

Hmm, en fait c'est plus subtil. Chaque stage peut avoir un type de
message différent. Le Router reçoit des RouteMsg, les Shards reçoivent
des ShardMsg. C'est pas un pipeline homogène.

**Repensons.** Le pipeline est plutôt un pattern d'usage de Pool :

```
Reader pool ──send──▸ Router pool ──send──▸ Shard pool
     N:1                  1:1               1:N (key-routed)
```

Le vrai besoin c'est pas un type Pipeline, c'est :
1. **Pool** (déjà proposé) avec les 3 dispatch strategies
2. **drain()** qui propage : drain readers, puis drain router, puis drain shards
3. Les acteurs eux-mêmes savent à quel pool envoyer (via ActorRef dans leur state)

Le drain cascadé pourrait être un helper :

```rust
/// Drain a sequence of pools in order.
/// Each pool is fully drained before the next starts.
pub fn drain_pipeline(pools: &[&dyn Drainable], label: &str) {
    for (i, pool) in pools.iter().enumerate() {
        pool.drain(&format!("{}_stage_{}", label, i));
    }
}
```

Ou mieux, un `PipelineScope` qui possède les pools et gère le lifecycle :

```rust
pub struct PipelineScope {
    stages: Vec<Box<dyn Drainable>>,
}

impl PipelineScope {
    pub fn new() -> Self;
    pub fn add_stage(&mut self, stage: impl Drainable + 'static);
    pub fn drain_all(&self, label: &str);
    pub fn shutdown_all(&self);
}
```

En fait le type important c'est le **trait Drainable** :

```rust
pub trait Drainable: Send + Sync {
    /// Wait until all pending work is done.
    fn drain(&self, label: &str);
    /// Signal shutdown (stop after current work).
    fn shutdown(&self);
}

impl<M: Send + 'static> Drainable for Pool<M> { ... }
impl<M: Send + 'static> Drainable for ActorRef<M> { ... }
```

## Primitive 4 : DAG (déjà implémenté, à affiner)

Le DAG qu'on a codé (phase 1-2) est bon pour l'exécution structurelle.
Ce qui manque :

### 4.1 Contexte partagé (Services)

Les nœuds du merge DAG ont besoin d'accéder au SegmentManager, au
Directory, etc. Deux approches :

**Option A : capture à la construction** (simple, recommandé)

```rust
struct MergeNode {
    shared: Arc<SegmentUpdaterShared>,
    merge_op: MergeOperation,
}

impl Node for MergeNode {
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        // self.shared a tout ce qu'il faut
        let mut state = MergeState::new(&self.merge_op, &self.shared)?;
        while state.step() == Continue {}
        ctx.set_output("result", PortValue::new(state.into_result()));
        Ok(())
    }
}
```

**Les nœuds capturent leur contexte.** Pas besoin de ServiceRegistry.
C'est plus simple que rag3weaver et suffisant pour lucivy.

**Option B : services dans le DAG** (plus générique)

```rust
impl Dag {
    pub fn set_service<T: Send + Sync + 'static>(&mut self, value: T);
}

impl NodeContext {
    pub fn service<T: 'static>(&self) -> Option<&T>;
}
```

On peut faire les deux : l'option A par défaut, l'option B si on veut
un DAG qui partage des ressources entre nœuds sans les passer à la
construction. Mais l'option B ajoute de la complexité.

**Recommandation : option A d'abord, option B si le besoin se présente.**

### 4.2 DAG + Scheduler intégration

Actuellement le DAG utilise `std::thread::scope` séparément du scheduler.
C'est simple et ça marche, mais les threads du DAG ne sont pas les threads
du scheduler. Si tous les threads du scheduler sont occupés par des acteurs,
le DAG spawne quand même ses propres threads → surcharge CPU.

**Option : dispatch vers le pool du scheduler**

```rust
impl Scheduler {
    /// Execute a task on the scheduler's thread pool.
    /// Returns a ReplyReceiver for the result.
    pub fn spawn_task<F, R>(&self, f: F) -> ReplyReceiver<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;
}
```

Le runtime DAG utiliserait `spawn_task` au lieu de `thread::scope` :

```rust
// runtime.rs
for node in level_nodes {
    let rx = scheduler.spawn_task(move || node.execute(&mut ctx));
    receivers.push(rx);
}
for rx in receivers {
    let result = rx.wait_cooperative_named(...);
}
```

**Avantage** : réutilise les threads existants, pas de surcharge.
**Inconvénient** : les tâches DAG et les acteurs se partagent les threads,
potentiellement plus lent si les acteurs sont très actifs.

**En pratique** : pendant le commit DAG, les acteurs d'indexation sont
drainés (pas actifs). Donc les threads sont libres. Le partage est gratuit.

### 4.3 DAG builder ergonomique

Pour le cas courant (chaîne linéaire avec fan-out de merges) :

```rust
let mut dag = Dag::new();

// Le PlanNode produit N merge ops dynamiquement
dag.add_node("plan", PlanMergesNode::new(candidates));

// Fan-out : N MergeNodes en parallèle
for (i, op) in merge_ops.iter().enumerate() {
    let name = format!("merge_{i}");
    dag.add_node(&name, MergeNode::new(op.clone(), shared.clone()));
    dag.connect("plan", &format!("op_{i}"), &name, "input")?;
}

// Fan-in → chain
dag.add_node("end", EndMergeNode::new(shared.clone()));
for i in 0..merge_ops.len() {
    dag.connect(&format!("merge_{i}"), "result", "end", &format!("in_{i}"))?;
}
dag.chain(&["end", "save", "gc", "reload"])?;  // ← helper
```

Le helper `chain` pour les séquences linéaires trigger→trigger :

```rust
impl Dag {
    /// Connect nodes linearly via trigger ports (done → go).
    pub fn chain(&mut self, names: &[&str]) -> Result<(), String> {
        for pair in names.windows(2) {
            self.connect(pair[0], "done", pair[1], "trigger")?;
        }
        Ok(())
    }
}
```

### 4.4 Nœuds dynamiques (fan-out runtime)

Le PlanMergesNode ne sait pas à la construction combien de merges il va
produire. Il découvre les candidates à l'exécution.

**Solution : les nœuds peuvent déclarer des outputs dynamiques.**

En fait, plus simple : le PlanNode produit un `Vec<MergeOp>` sur un
seul port. Le runtime ne fait pas le fan-out — c'est le code appelant
qui construit le DAG avec le bon nombre de nœuds.

```rust
// Avant de construire le DAG :
let candidates = collect_merge_candidates(&segment_manager);
let merge_ops = plan_merges(candidates);

// Maintenant on sait combien de merges → on construit le DAG
let mut dag = Dag::new();
for (i, op) in merge_ops.iter().enumerate() {
    dag.add_node(&format!("merge_{i}"), MergeNode::new(op, shared));
}
// ...
```

**Le DAG est construit dynamiquement, pas statiquement.** C'est le bon
pattern. On n'a pas besoin de nœuds qui modifient la structure du DAG
à l'exécution.

## Primitive 5 : Scope (lifecycle management)

### Le problème

La gestion du lifecycle dans lucivy est manuelle :

```rust
// commit() dans sharded_handle.rs :
drain_pipeline(&reader_pool, &router, &shard_actors)?;  // ~30 lignes
for shard in &shard_actors {
    shard.send(ShardCommitMsg { ... })?;  // ~15 lignes
}
collect_replies()?;  // ~20 lignes
```

```rust
// close() dans sharded_handle.rs :
// shutdown readers, router, shards, wait for all
// ~40 lignes de code séquentiel
```

### Ce que luciole devrait offrir

Un **Scope** qui possède des acteurs/pools et gère leur lifecycle :

```rust
pub struct Scope {
    name: String,
    drainables: Vec<(String, Box<dyn Drainable>)>,
}

impl Scope {
    pub fn new(name: &str) -> Self;

    /// Register an actor or pool in this scope.
    pub fn register(&mut self, name: &str, d: impl Drainable + 'static);

    /// Drain all registered actors/pools in registration order.
    pub fn drain(&self);

    /// Drain in reverse order then shutdown.
    pub fn shutdown(&self);

    /// Execute a DAG after draining all actors.
    /// Drains → executes DAG → resumes.
    pub fn execute_dag(&self, dag: &mut Dag) -> Result<DagResult, String> {
        self.drain();
        let result = execute_dag(dag, None)?;
        // actors resume naturally when new messages arrive
        Ok(result)
    }
}
```

**Le Scope encode le pattern "drain → DAG → resume" en une seule méthode.**

Pour lucivy :
```rust
// Construction
let mut scope = Scope::new("index_writer");
scope.register("readers", reader_pool);
scope.register("router", router_pool);
scope.register("shards", shard_pool);

// Commit
scope.execute_dag(&mut commit_dag)?;

// Close
scope.shutdown();
```

## Vue d'ensemble : lucivy simplifié

### Avant (aujourd'hui) — ~600 lignes de threading dans lucivy

```
lucivy code:
  - 6 GenericActor definitions (~80 lignes chacun)
  - Manual Vec<ActorRef> pools (~30 lignes × 3)
  - Manual drain logic (~50 lignes × 2)
  - Manual scatter-gather (~20 lignes × search)
  - drain_all_merges (~100 lignes)
  - gc_protected_segments logic (~40 lignes)
  - rebuild_deferred_sfx (~80 lignes)
```

### Après (avec luciole complet) — ~150 lignes de threading dans lucivy

```
lucivy code:
  - 6 Actor impls (enum messages, 30-40 lignes chacun, exhaustif par pattern match)
  - Pool::spawn() × 3 (une ligne chacun)
  - scope.execute_dag() pour le commit (une ligne)
  - shard_pool.scatter() pour la search (une ligne)
  - scope.drain() / scope.shutdown() (une ligne chacun)
```

Le code lucivy ne fait plus que de la **logique métier** (merge, index,
search). Toute la **coordination** est dans luciole.

## Plan d'implémentation révisé

### Phase 1 : Typed Actor ergonomics (dans luciole, ~100 lignes)

- `ActorRef::request()` helper (send + wait_cooperative en 1 appel)
- Documenter le pattern Actor<Msg=MyEnum> comme recommandé
- Tests : acteur typé avec enum, request/response, self-messages

### Phase 2 : Pool (dans luciole, ~200 lignes)

- `Pool<M>` struct avec RoundRobin et KeyRouted
- `send()`, `send_to()`, `broadcast()`, `scatter()`
- `Drainable` trait + `Pool::drain()`
- Tests : round-robin, scatter-gather, drain

### Phase 3 : Scope (dans luciole, ~100 lignes)

- `Scope` struct avec register + drain + shutdown
- `Scope::execute_dag()` (drain → DAG → done)
- Tests : drain cascadé, shutdown order

### Phase 4 : DAG refinements (dans luciole, ~50 lignes)

- `Dag::chain()` helper pour séquences trigger
- Event callback → intégration EventBus existant
- Tests : chain, events intégrés

### Phase 5 : Lucivy migration (dans lucivy, ~300 lignes changed)

- Migrer IndexerActor vers Actor<Msg=IndexerMsg>
- Migrer ShardActor vers Actor<Msg=ShardMsg>
- Remplacer Vec<ActorRef> par Pool
- Remplacer drain manuels par Scope
- Remplacer drain_all_merges par commit DAG
- Supprimer : gc_protected_segments, rebuild_deferred_sfx, track/untrack

### Phase 6 : Bench validation

- 20K : SFX ground truth match
- 90K : pas de panic, tous les queries OK
- Métriques DAG post-mortem
- Comparaison perf avant/après

## Ce qui ne change PAS dans luciole

- `GenericActor` : reste pour les cas multi-rôles et réseau futur
- `Envelope` / `Message` : reste pour la sérialisation réseau
- `TypedHandler` : reste, utilisé par GenericActor
- `EventBus` : reste, étendu avec DagEvent
- `Scheduler` core : inchangé (threads, BinaryHeap, take pattern)
- `Reply` / `ReplyReceiver` : inchangé
- WASM compatibility : inchangée

## Estimation de taille

```
luciole ajouts :
  pool.rs         ~200 lignes (Pool, DispatchStrategy, Drainable)
  scope.rs        ~100 lignes (Scope, lifecycle)
  dag.rs +50      ~50 lignes  (chain helper, builder ergonomics)
  lib.rs +20      ~20 lignes  (request helper sur ActorRef, exports)
                  ──────
                  ~370 lignes

lucivy changements :
  indexer_actor.rs     -80 +40  (GenericActor → Actor<IndexerMsg>)
  sharded_handle.rs    -200 +60 (manual pools/drain → Pool/Scope)
  segment_updater_actor.rs -150 +30 (drain/rebuild → DAG)
  segment_updater.rs   -40 +10  (gc_protected removed)
                       ──────
                       net -330 lignes supprimées

TOTAL : +370 luciole, -330 lucivy = net +40 lignes
        mais 370 lignes sont testées et réutilisables
```

## Addendum : threads persistants unifiés

### Le constat

Le scheduler a déjà N threads persistants dans `run_loop`. Chaque thread
fait `loop { pop actor → handle batch → put back }`. Les threads ne
sont jamais créés ou détruits après l'init.

Mais on a **3 mécanismes de threading séparés** :
1. Scheduler threads (pool) → acteurs
2. `spawn_pinned` → thread dédié par acteur (IndexerWorker)
3. `std::thread::scope` dans le DAG runtime → threads temporaires

Ce qui serait mieux : **un seul pool de threads persistants** qui exécute
tout — acteurs, nœuds DAG, tâches scatter, etc.

### Le concept unifiant : la Tâche

Un acteur qui traite un message = une tâche.
Un nœud DAG qui s'exécute = une tâche.
Un scatter shard = N tâches.
Un finalize segment = une tâche.

```
WorkItem =
  | ActorBatch(actor_id)     — traiter la mailbox de cet acteur
  | Task(Box<dyn FnOnce>)    — exécuter cette closure
```

Le pool de threads ne sait pas ce qu'il exécute. Il pop un `WorkItem`
de la ready queue et l'exécute. Les acteurs sont des WorkItems récurrents
(re-enqueue quand mailbox non-vide). Les tâches DAG sont des WorkItems
one-shot.

### Architecture proposée

```
SharedState {
    ready_queue: BinaryHeap<WorkItem>,  // acteurs ET tâches
    actors: HashMap<ActorId, ActorSlot>,
    work_available: Condvar,
}

enum WorkItem {
    Actor {
        priority: Priority,
        actor_id: ActorId,
    },
    Task {
        priority: Priority,
        task: Box<dyn FnOnce() + Send>,
        done: Reply<()>,  // signal de completion
    },
}

// Thread loop (unique, pour tout) :
fn run_loop(shared: &SharedState) {
    loop {
        let item = pop_work(shared);
        match item {
            WorkItem::Actor { actor_id, .. } => {
                // Exactement comme aujourd'hui
                handle_actor_batch(shared, actor_id);
            }
            WorkItem::Task { task, done, .. } => {
                task();
                done.send(());
            }
        }
    }
}
```

### Ce que ça change pour le DAG runtime

Le runtime ne crée plus de threads. Il soumet des tâches au pool :

```rust
// runtime.rs — exécution d'un niveau parallèle
fn execute_level(nodes: Vec<NodeWork>, scheduler: &Scheduler) -> Vec<NodeResult> {
    let mut receivers = Vec::new();
    for node_work in nodes {
        let rx = scheduler.submit_task(Priority::High, move || {
            node_work.execute()
        });
        receivers.push(rx);
    }
    // Attendre tous les résultats
    receivers.into_iter()
        .map(|rx| rx.wait_cooperative_named("dag_node", || scheduler.run_one_step()))
        .collect()
}
```

### Ce que ça change pour spawn_pinned

`spawn_pinned` disparaît. Un "pinned" actor c'est juste un acteur
normal avec Priority::Critical. Les threads du pool le traitent
en priorité.

Si on veut vraiment de l'affinité (hot cache), on pourrait ajouter
un hint `preferred_thread: Option<usize>` dans le WorkItem. Mais
c'est une optimisation future, pas nécessaire maintenant.

### Ce que ça change pour scatter-gather

```rust
impl<M> Pool<M> {
    pub fn scatter<R, F>(&self, make_msg: F) -> Vec<R>
    where
        R: Send + 'static,
        F: Fn(Reply<R>) -> M,
    {
        // Envoie un message à chaque worker
        // Les workers traitent via le MÊME pool de threads
        // Les résultats reviennent via Reply
    }
}
```

Pas de threads supplémentaires. Le scatter envoie N messages à N acteurs.
Les N acteurs sont dans la ready queue. Les N threads du pool les traitent.
Parallélisme naturel.

### Impact sur le code existant

Le changement principal est dans `scheduler.rs` :
- `ReadyEntry` → `WorkItem` (enum avec Actor et Task)
- `run_loop` → match sur WorkItem
- Ajout de `Scheduler::submit_task(priority, closure) -> ReplyReceiver`
- `spawn_pinned` → marqué deprecated, remplacé par Priority::High

Les acteurs ne changent pas. Leur API est la même.
Le DAG runtime remplace `thread::scope` par `submit_task`.
C'est un refactoring interne du scheduler, transparent pour lucivy.

### Résumé

Le pool de threads persistants est le **seul moteur d'exécution**.
Tout passe par la ready queue :

```
Thread pool [N threads]
    ↑
    ready_queue (priority heap)
    ↑                    ↑                  ↑
  Acteurs             DAG nodes          Tasks (scatter, finalize, ...)
  (récurrents)        (one-shot)         (one-shot)
```

Un seul pool. Un seul mécanisme. Pas de spawn/destroy.
Les threads persistent du boot au shutdown.

## Résumé final

Luciole passe de "scheduler d'acteurs + DAG séparé" à "pool de threads
persistants unifié" avec 5 primitives :

1. **Actor** — travail récurrent sur le pool (messages typés, request/response)
2. **Pool** — N actors identiques, round-robin/key-routed, scatter-gather
3. **Scope** — lifecycle management, drain cascadé, execute_dag intégré
4. **DAG** — graphe de tâches soumises au pool, parallélisme structural
5. **Task** — travail one-shot soumis au pool (closure + Reply)

Un seul pool de threads. Pas de spawn/destroy. Les threads persistent.
Lucivy ne code que sa logique métier.
