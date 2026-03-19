# Doc 07 — Vision : luciole comme lib de threading DAG

Date : 19 mars 2026

## L'idée

Plutôt que d'ajouter un mini-DAG spécifique au merge dans lucivy, on
étend luciole pour devenir une **lib générique de threads persistants
orchestrés par DAG**.

Luciole aujourd'hui : scheduler d'acteurs avec mailboxes et priorités.
Luciole demain : **threads persistants + DAG d'orchestration + observabilité**.

Les acteurs deviennent des workers qui participent à des DAGs. Un worker
peut participer à plusieurs DAGs simultanément. Le DAG définit l'ordre
d'exécution et le flux de données entre workers.

## Ce que ça unifie

### Aujourd'hui : 3 patterns distincts

1. **Acteurs background** (luciole) : messages async, pas de garantie d'ordre
2. **Drain synchrone** (drain_all_merges) : busy-loop dans le scheduler thread
3. **DAG rag3weaver** (dataflow runtime) : exécution séquentielle par topo sort

### Demain : 1 seul pattern

Des workers persistants qui exécutent des tâches définies par des DAGs.
Le DAG encode le parallélisme ET le séquencement. Les workers sont
réutilisés entre les DAGs (pas de spawn/destroy à chaque opération).

## Architecture proposée

```
luciole/
  src/
    worker.rs        — Worker trait (remplace Actor), persistent thread
    pool.rs          — WorkerPool, persistent threads réutilisables
    dag.rs           — DAG structure, nodes, edges, topo sort
    node.rs          — Node trait, NodeContext, métriques
    runtime.rs       — Exécution par niveaux + dispatch vers workers
    port.rs          — PortValue générique (Any + type tag)
    observe.rs       — Events, taps, métriques (zero-cost)
    scheduler.rs     — Compat backward (actors = workers + mailbox)
    reply.rs         — Reply/ReplyReceiver (inchangé)
    mailbox.rs       — Mailbox (inchangé, utilisé pour le mode actor)
```

## Les deux modes d'utilisation

### Mode Actor (backward compat, pour le background)

Exactement comme aujourd'hui. Un Worker avec une Mailbox traite des
messages un par un. Utile pour l'indexation continue :

```rust
let indexer = pool.spawn_actor("indexer", IndexerActor::new());
indexer.send(AddDocMsg { doc });
// Pas de DAG, pas d'ordre garanti, fire-and-forget
```

### Mode DAG (pour les opérations orchestrées)

Un DAG définit l'orchestration. Les nœuds sont des closures ou des
types qui implémentent Node. L'exécution utilise les workers du pool :

```rust
let mut dag = Dag::new();
dag.add_node("plan", PlanMergesNode::new(candidates));
dag.add_node("merge_0", MergeNode::new(plan_0));
dag.add_node("merge_1", MergeNode::new(plan_1));
dag.add_node("barrier", BarrierNode);
dag.add_node("save", SaveMetasNode::new(opstamp));
dag.add_node("gc", GCNode);
dag.add_node("reload", ReloadNode);

dag.connect("plan", "ops_0", "merge_0", "input");
dag.connect("plan", "ops_1", "merge_1", "input");
dag.connect("merge_0", "result", "barrier", "in_0");
dag.connect("merge_1", "result", "barrier", "in_1");
dag.connect("barrier", "done", "save", "trigger");
dag.connect("save", "done", "gc", "trigger");
dag.connect("gc", "done", "reload", "trigger");

let result = pool.execute_dag(&dag)?;
// Bloquant. Merges parallèles, GC après, reload à la fin.
// Métriques par nœud disponibles dans result.
```

## Worker : le concept central

Un Worker est un **thread persistant** qui peut exécuter des tâches.
Les tâches viennent soit d'une Mailbox (mode actor) soit d'un DAG
(mode orchestré).

```rust
pub trait Worker: Send + 'static {
    /// Nom pour l'observabilité
    fn name(&self) -> &str;
}

pub trait ActorWorker: Worker {
    type Msg: Send;
    fn handle(&mut self, msg: Self::Msg) -> WorkerStatus;
    fn poll_idle(&mut self) -> Poll<()> { Poll::Pending }
}

pub trait DagWorker: Worker {
    /// Exécute un nœud du DAG. Appelé par le runtime.
    fn execute_node(&mut self, node: &mut dyn Node, ctx: &mut NodeContext)
        -> Result<(), String>;
}
```

En pratique, le pool a des threads génériques qui peuvent exécuter
n'importe quel nœud. Les workers spécialisés (comme l'IndexerActor)
restent en mode actor pour le background.

## Exécution par niveaux

Le runtime DAG utilise le pool de threads pour le parallélisme :

```
Niveau 0: [PlanMerges]
  → Exécuté par 1 thread du pool
  → Output: 4 merge plans

Niveau 1: [Merge_0, Merge_1, Merge_2, Merge_3]
  → Exécutés par 4 threads du pool en parallèle
  → Chaque thread fait le merge complet (init → postings → sfx → close)
  → Barrier automatique : le runtime attend que les 4 finissent

Niveau 2: [EndMerge]
  → 1 thread, agrège les 4 résultats

Niveau 3: [SaveMetas] → Niveau 4: [GC] → Niveau 5: [Reload]
  → Séquentiels, 1 thread chaque
```

Le pool réutilise les mêmes threads que le scheduler d'acteurs.
Pas de spawn supplémentaire. Les threads idle du pool sont disponibles
pour les nœuds DAG.

## Observabilité structurelle

### Events typés (remplace lucivy_trace! et les eprintln)

```rust
pub enum DagEvent {
    /// Un nœud commence son exécution
    NodeStarted {
        dag_id: DagId,
        node: String,
        node_type: &'static str,
        thread_id: usize,
    },
    /// Un nœud termine avec succès
    NodeCompleted {
        dag_id: DagId,
        node: String,
        duration_ms: u64,
        metrics: Vec<(String, f64)>,
    },
    /// Un nœud échoue
    NodeFailed {
        dag_id: DagId,
        node: String,
        error: String,
        duration_ms: u64,
    },
    /// Un niveau du DAG commence (N nœuds en parallèle)
    LevelStarted {
        dag_id: DagId,
        level: usize,
        nodes: Vec<String>,
    },
    /// Un niveau termine
    LevelCompleted {
        dag_id: DagId,
        level: usize,
        duration_ms: u64,
    },
}
```

### Métriques par nœud

```rust
impl NodeContext {
    pub fn metric(&mut self, key: &str, value: f64);
    pub fn info(&mut self, msg: &str);
    pub fn warn(&mut self, msg: &str);
}
```

Chaque nœud émet des métriques structurées collectées par le runtime.
Accessibles dans le DagResult après exécution :

```rust
let result = pool.execute_dag(&dag)?;
for (node_name, metrics) in result.node_metrics() {
    eprintln!("{}: {:?}", node_name, metrics);
}
// merge_0: {"docs": 1250, "sfx_terms": 8422, "postings_ms": 450, "sfx_ms": 1200}
// merge_1: {"docs": 1000, "sfx_terms": 6100, "postings_ms": 380, "sfx_ms": 980}
// save: {"segments": 8, "metas_bytes": 2048}
// gc: {"deleted": 12, "freed_mb": 45.2}
```

### Edge taps (zero-cost quand inactif)

```rust
let tap = dag.tap("merge_0", "result");  // Capture les données entre 2 nœuds
let result = pool.execute_dag(&dag)?;
if let Some(data) = tap.try_recv() {
    // Inspecter le MergeResult sans modifier le code du nœud
}
```

### Worker activity (déjà implémenté)

Le `ActorActivity` qu'on a ajouté cette session s'étend naturellement :
le runtime set l'activity à `"dag:merge_0"` pendant l'exécution d'un nœud.
`dump_state()` montre quel worker exécute quel nœud de quel DAG.

## Comparaison avec rag3weaver

| Aspect | rag3weaver dataflow | luciole DAG |
|--------|-------------------|-------------|
| Threading | async (tokio), séquentiel par itération | sync, parallèle par niveau (pool) |
| Workers | Pas de workers, chaque execute() est standalone | Threads persistants réutilisés |
| Acteurs | Pas d'acteurs, que des nodes | Workers = acteurs OR nœuds DAG |
| WASM | async_broadcast | Coopératif (wait_cooperative existant) |
| PortValue | 15+ variants RAG-spécifiques | Any + TypeId (générique) |
| Checkpoint | Cypher DB, async | Filesystem, sync (futur) |
| Events | async_broadcast, peut overflow | Channel sync borné, backpressure |
| Taps | TapRegistry, zero-cost | Même pattern, adaptable |
| DI/Services | String-keyed registry | Pas nécessaire (context direct) |

## Ce que rag3weaver pourrait utiliser de luciole

Si luciole devient une lib de DAG threading, rag3weaver pourrait
l'utiliser au lieu de son propre dataflow runtime :

- Les nodes rag3weaver implémenteraient le trait Node de luciole
- L'exécution par niveaux donnerait du parallélisme gratuit
  (les nœuds de search indépendants tourneraient en parallèle)
- Les checkpoints seraient gérés par luciole (filesystem, pas Cypher)
- L'observabilité serait unifiée entre lucivy et rag3weaver

C'est pas obligatoire pour la première version, mais c'est le chemin
naturel si luciole est bien conçue.

## Plan d'implémentation

### Phase 1 : Core DAG dans luciole (~400 lignes)
- `dag.rs` : Dag struct, add_node, connect, topo sort par niveaux
- `node.rs` : trait Node sync, NodeContext, PortDef
- `port.rs` : PortValue (Any + TypeId), merge strategies
- Tests unitaires

### Phase 2 : Runtime dans luciole (~300 lignes)
- `runtime.rs` : exécution par niveaux, dispatch vers le pool existant
- DagEvent enum + channel
- DagResult avec métriques par nœud
- Integration avec le pool de threads existant

### Phase 3 : Nœuds de merge dans lucivy (~200 lignes)
- PlanMergesNode, MergeNode, EndMergeNode, SaveMetasNode, GCNode, ReloadNode
- Intégration dans handle_commit(rebuild_sfx=true)
- Tests E2E avec le bench Linux kernel

### Phase 4 : Observabilité avancée (~150 lignes)
- Edge taps zero-cost
- Intégration diagnostics.rs avec DagResult
- Bench post-mortem automatique depuis les métriques DAG

### Phase 5 : Backward compat scheduler (~100 lignes)
- Le scheduler existant utilise le pool de threads
- Les acteurs sont des workers en mode actor
- Pas de breaking change pour le code existant

## Estimation de taille

```
luciole/src/dag.rs       ~150 lignes (structure, topo sort)
luciole/src/node.rs      ~100 lignes (trait, context, portdef)
luciole/src/runtime.rs   ~200 lignes (execution, events, dispatch)
luciole/src/port.rs      ~80 lignes (PortValue Any, merge)
luciole/src/observe.rs   ~80 lignes (taps, events)
                         ─────────
                         ~610 lignes dans luciole

lucivy nodes             ~200 lignes (6 nœuds de merge)
integration              ~100 lignes (handle_commit)
                         ─────────
                         ~300 lignes dans lucivy

TOTAL                    ~910 lignes
```

Pour comparaison : le code actuel de drain_all_merges + rebuild_deferred_sfx
+ gc_protected_segments + track_segments fait ~300 lignes et ne marche pas
de manière fiable. Le DAG fait 3× plus de code mais résout le problème
par construction et apporte parallélisme + observabilité.

## Ce qu'on garde de luciole actuel

- `mailbox.rs` : inchangé (mode actor)
- `reply.rs` : inchangé (wait_cooperative)
- `scheduler.rs` : adapté pour utiliser le pool partagé
- `events.rs` : étendu avec DagEvent
- `generic_actor.rs` : inchangé (backward compat)
- `handler.rs` : inchangé
- `envelope.rs` : inchangé
- `actor_state.rs` : inchangé

## Résumé

Luciole passe de "scheduler d'acteurs" à "framework de coordination
multi-threadé" avec deux modes :
1. **Acteurs** : messages async, background, fire-and-forget
2. **DAG** : orchestration structurelle, parallélisme par niveaux, séquencement garanti

Le tout sur un **pool de threads persistants partagé**. Pas de spawn/destroy.
Les mêmes threads font le background et l'orchestration. L'observabilité
est intégrée à chaque couche.
