# Doc 08 — Plan d'implémentation : luciole DAG threading lib

Date : 19 mars 2026
Prérequis : docs 01-07 de cette session

## État actuel du code

### Branche : `experiment/decouple-sfx`
Commits de la session :
- `5fc5871` — fix contains fuzzy distance default 0
- `12a12e9` — revert deferred sfx (baseline)
- `545315b` — WIP: commit_fast + observabilité + diagnostics
- `51bd1b4` — fix GC protection segments in merge
- `fb3e8d7` — docs bug segments without sfx
- `cd6b25c` — session recap
- `a554efa` — design merge DAG (doc 06)
- `54b7af8` — vision luciole DAG lib (doc 07)

### Bugs connus non résolus
1. **Segments sans sfx** (doc 03) : certains petits segments (47-140 docs)
   n'ont jamais de .sfx/.sfxpost. Cause : timing entre le FinalizerActor
   qui écrit les fichiers et le merge qui commence avant l'écriture.
   → Le DAG résout ça : les merges ne démarrent qu'après que tous les
   segments soient finalisés (dépendance structurelle).

2. **GC race** : malgré `gc_protected_segments` et le check
   `segments_in_merge.is_empty()`, le GC peut supprimer des fichiers
   pendant un merge concurrent.
   → Le DAG résout ça : le GC est un nœud qui dépend de tous les merges.

3. **Mmap cache stale** : `atomic_write` remplace un fichier mais le mmap
   cache retourne l'ancien contenu.
   → Le DAG résout ça : un seul reload après tous les writes.

### Code existant réutilisable

#### Dans luciole (à étendre)
| Fichier | Lignes | Réutilisable | Changement |
|---------|--------|-------------|------------|
| `scheduler.rs` | ~300 | Pool de threads, dispatch | Ajouter dispatch DAG |
| `reply.rs` | ~170 | wait_cooperative_named | Inchangé |
| `mailbox.rs` | ~200 | Mailbox, ActorRef | Inchangé |
| `events.rs` | ~100 | EventBus broadcast | Étendre avec DagEvent |
| `generic_actor.rs` | ~200 | GenericActor | Inchangé |
| `handler.rs` | ~150 | TypedHandler | Inchangé |
| `envelope.rs` | ~200 | Message trait, Envelope | Inchangé |
| `actor_state.rs` | ~50 | ActorState container | Inchangé |

#### Dans lucivy (à remplacer par des nœuds DAG)
| Fichier | Code à remplacer | Remplacé par |
|---------|-----------------|--------------|
| `segment_updater_actor.rs:436-541` | drain_all_merges() | DAG execution |
| `segment_updater_actor.rs:920-1032` | rebuild_deferred_sfx() | Plus nécessaire |
| `segment_updater_actor.rs:547-570` | track_segments/untrack | Plus nécessaire |
| `segment_updater.rs:58-67` | gc_protected_segments | Plus nécessaire |
| `segment_updater.rs:127-182` | list_files() protection | Simplifié |
| `merge_state.rs` | use_deferred_sfx flag | Toujours full |

#### Dans lucivy (à garder tel quel)
| Fichier | Pourquoi |
|---------|----------|
| `merge_state.rs` (phases) | Les 6 phases de merge restent, utilisées par MergeNode |
| `merger.rs` (merge_sfx) | Le merge_sfx complet reste, appelé par MergeState |
| `segment_manager.rs` | Gestion des registres committed/uncommitted |
| `index_writer.rs` | commit/commit_fast API |
| `prepared_commit.rs` | commit/commit_fast |
| `diagnostics.rs` | Outils de diagnostic (étendus avec DagResult) |

### Observabilité existante à intégrer
- `lucivy_trace!()` macro (src/lib.rs) → remplacé par NodeContext.info/warn
- `ActorActivity` (scheduler.rs) → étendu : "dag:node_name" pendant exécution
- `wait_cooperative_named` (reply.rs) → inchangé pour le mode actor
- `dump_state()` (scheduler.rs) → étendu avec l'état des DAGs en cours
- `diagnostics.rs` → consomme DagResult pour le post-mortem

### Bench et dataset
- Dataset : `/home/luciedefraiteur/linux_bench` (kernel Linux, ~91K fichiers)
- Bench : `lucivy_core/benches/bench_sharding.rs`
  - `BENCH_DATASET` env var pour choisir le dataset
  - `LUCIVY_VERIFY=1` pour ground truth
  - `LUCIVY_DEBUG=1` pour les traces
  - `MAX_DOCS=N` pour limiter
  - `BENCH_MODE=RR|TA|SINGLE` pour choisir le mode
  - Post-mortem : compare_postings_vs_sfxpost, inspect_sfx, dump_segment_keys
- Ground truth : `/tmp/verify_ground_truth.rs` (script standalone)
- Résultats validés sur 20K : SFX mutex=1375 = ground truth exact
- Résultats 90K : panic gapmap (bug segments sans sfx, résolu par le DAG)

## Phase 1 : Core DAG dans luciole (~400 lignes)

### 1.1 dag.rs (~150 lignes)

```rust
pub struct Dag {
    nodes: Vec<DagNodeEntry>,
    edges: Vec<DagEdge>,
}

struct DagNodeEntry {
    name: String,
    node: Box<dyn Node>,
}

struct DagEdge {
    from_node: String,
    from_port: String,
    to_node: String,
    to_port: String,
}

impl Dag {
    pub fn new() -> Self;
    pub fn add_node(&mut self, name: &str, node: impl Node + 'static);
    pub fn connect(&mut self, from_node: &str, from_port: &str,
                   to_node: &str, to_port: &str) -> Result<(), String>;

    /// Tri topologique par niveaux.
    /// Retourne Vec<Vec<usize>> : chaque Vec interne = nœuds parallélisables.
    pub fn topological_levels(&self) -> Result<Vec<Vec<usize>>, String>;

    /// Validation : tous les ports required connectés, pas de cycles.
    pub fn validate(&self) -> Result<(), String>;
}
```

**Inspiré de** : rag3weaver `graph.rs` (463 lignes)
- Kahn's algorithm adapté pour retourner des niveaux au lieu d'une liste plate
- Validation des types de ports au connect (PortType::compatible_with)
- Détection de cycles et deadlocks

**Différence clé** : le topo sort retourne des NIVEAUX, pas une séquence.
Les nœuds d'un même niveau sont indépendants et parallélisables.

### 1.2 node.rs (~100 lignes)

```rust
pub trait Node: Send {
    fn name(&self) -> &str;
    fn node_type(&self) -> &'static str;
    fn inputs(&self) -> &[PortDef];
    fn outputs(&self) -> &[PortDef];
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String>;
}

pub struct PortDef {
    pub name: &'static str,
    pub port_type: PortType,
    pub required: bool,
}

pub struct NodeContext {
    inputs: HashMap<String, PortValue>,
    outputs: HashMap<String, PortValue>,
    metrics: Vec<(String, f64)>,
    logs: Vec<(LogLevel, String)>,
}

impl NodeContext {
    pub fn input(&self, port: &str) -> Option<&PortValue>;
    pub fn take_input(&mut self, port: &str) -> Option<PortValue>;
    pub fn set_output(&mut self, port: &str, value: PortValue);
    pub fn metric(&mut self, key: &str, value: f64);
    pub fn info(&mut self, msg: &str);
    pub fn warn(&mut self, msg: &str);
}
```

**Inspiré de** : rag3weaver `node.rs` (255 lignes)
- Trait sync (pas async) — compatible WASM coopératif
- NodeContext sandbox : inputs/outputs/metrics/logs
- `take_input` consomme la valeur (move semantics pour les gros payloads)

**Différence** : pas de undo/checkpoint dans la phase 1, ajouté en phase 5.

### 1.3 port.rs (~80 lignes)

```rust
pub enum PortType {
    /// Typed data identified by TypeId
    Typed(std::any::TypeId),
    /// Trigger signal (no data)
    Trigger,
    /// Compatible with anything
    Any,
}

pub enum PortValue {
    /// Type-erased data
    Data(Box<dyn std::any::Any + Send>),
    /// Trigger signal
    Trigger,
}

impl PortValue {
    pub fn new<T: Send + 'static>(data: T) -> Self;
    pub fn downcast<T: 'static>(&self) -> Option<&T>;
    pub fn take<T: 'static>(self) -> Option<T>;
}
```

**Inspiré de** : rag3weaver `port.rs` (413 lignes)
- Simplifié : pas de 15 variants spécifiques, juste Any + TypeId
- Le type checking se fait au connect via PortType::Typed(TypeId::of::<T>())
- merge_port_values pas nécessaire en phase 1 (fan-in = collect en Vec)

**Pour lucivy** : les types concrets seront `Vec<SegmentId>`, `MergeResultData`,
`Vec<MergeResultData>`, etc. Le PortValue les wrap en Any.

### 1.4 Tests phase 1

- DAG linéaire : A → B → C
- DAG avec parallélisme : A → [B, C] → D
- DAG diamond : A → [B, C] → D (fan-out + fan-in)
- Cycle detection
- Missing required input
- Type mismatch au connect
- NodeContext metrics + logs

## Phase 2 : Runtime dans luciole (~300 lignes)

### 2.1 runtime.rs

```rust
pub struct DagRuntime<'a> {
    pool: &'a WorkerPool,  // ou &'a Scheduler pour backward compat
}

pub struct DagResult {
    pub duration_ms: u64,
    pub node_results: HashMap<String, NodeResult>,
}

pub struct NodeResult {
    pub duration_ms: u64,
    pub metrics: Vec<(String, f64)>,
    pub logs: Vec<(LogLevel, String)>,
    pub outputs: HashMap<String, PortValue>,
}

impl DagRuntime {
    /// Exécute le DAG. Bloquant. Parallèle par niveau.
    pub fn execute(&self, dag: &mut Dag) -> Result<DagResult, String>;
}
```

**Exécution par niveaux** :
```
pour chaque niveau dans topological_levels() :
    si 1 nœud → exécuter séquentiellement sur le thread courant
    si N nœuds → dispatch vers N threads du pool (rayon ou scheduler)
    collecter les outputs
    propager les outputs vers les inputs du niveau suivant via les edges
```

**Dispatch vers le pool** :
Option A — rayon : `level_nodes.par_iter().map(|node| node.execute(ctx))`
Option B — scheduler : envoyer des messages aux workers et wait_cooperative
Option C — crossbeam scoped threads : `scope(|s| { for node in level { s.spawn(|| ...); } })`

L'option C est la plus simple et la plus compatible WASM (pas de rayon).
Mais en WASM single-thread, tout est séquentiel de toute façon.

**Events** :
```rust
pub enum DagEvent {
    NodeStarted { dag_id: u64, node: String, node_type: &'static str, level: usize },
    NodeCompleted { dag_id: u64, node: String, duration_ms: u64, metrics: Vec<(String, f64)> },
    NodeFailed { dag_id: u64, node: String, error: String },
    LevelStarted { dag_id: u64, level: usize, node_count: usize },
    LevelCompleted { dag_id: u64, level: usize, duration_ms: u64 },
    DagCompleted { dag_id: u64, total_ms: u64, node_count: usize },
    DagFailed { dag_id: u64, error: String },
}
```

Broadcast via le EventBus existant de luciole.

### 2.2 observe.rs (~80 lignes)

```rust
pub struct TapRegistry {
    taps: Vec<TapSpec>,
    all: bool,
    tx: flume::Sender<TapEvent>,
}

impl TapRegistry {
    pub fn is_active(&self) -> bool;
    pub fn check_and_emit(&self, edge: &DagEdge, value: &PortValue);
}
```

**Inspiré de** : rag3weaver `observe.rs` (220 lignes)
- Zero-cost quand inactif (check is_active() first)
- Clone le PortValue seulement si un tap matche
- Utile pour le bench post-mortem sans modifier le code des nœuds

### 2.3 Intégration scheduler

Le pool de threads du scheduler existant est réutilisé :
```rust
impl Scheduler {
    /// Exécute un DAG en utilisant les threads du pool.
    /// Les acteurs background continuent de tourner sur les autres threads.
    pub fn execute_dag(&self, dag: &mut Dag) -> Result<DagResult, String> {
        let runtime = DagRuntime { pool: self };
        runtime.execute(dag)
    }
}
```

Les threads du pool qui sont idle (pas en train de traiter un acteur)
sont utilisés pour les nœuds DAG. Si tous les threads sont occupés par
des acteurs, les nœuds DAG attendent (comme wait_cooperative).

## Phase 3 : Nœuds de merge dans lucivy (~200 lignes)

### 3.1 Les 6 nœuds

**PlanMergesNode** :
- Inputs : [] (lit les candidates depuis le SegmentManager)
- Outputs : `Vec<MergeOp>` par shard
- Action : collect_merge_candidates() + organiser par shard

**MergeNode** (un par merge op) :
- Inputs : `MergeOp` (segment IDs + target opstamp)
- Outputs : `MergeResultData`
- Action : `MergeState::new()` → step loop complet → résultat
- Métriques : docs_merged, sfx_terms, postings_ms, sfx_ms, total_ms

**EndMergeNode** :
- Inputs : `Vec<MergeResultData>` (fan-in de tous les MergeNodes)
- Outputs : trigger
- Action : segment_manager.end_merge() pour chaque résultat

**SaveMetasNode** :
- Inputs : trigger
- Outputs : trigger
- Action : purge_deletes + segment_manager.commit + save_metas

**GCNode** :
- Inputs : trigger
- Outputs : trigger
- Action : garbage_collect_files()
- Métriques : deleted_count, freed_bytes

**ReloadNode** :
- Inputs : trigger
- Outputs : []
- Action : reader.reload() pour chaque shard

### 3.2 Construction du DAG dans handle_commit

```rust
fn handle_commit(&mut self, opstamp, payload, rebuild_sfx) {
    if rebuild_sfx {
        // Construire le DAG
        let candidates = self.collect_merge_candidates();
        // + drain active_merge + explicit_merge en amont

        let mut dag = Dag::new();
        dag.add_node("plan", PlanMergesNode::new(candidates));
        for (i, op) in merge_ops.iter().enumerate() {
            dag.add_node(&format!("merge_{i}"), MergeNode::new(op.clone()));
            dag.connect("plan", &format!("op_{i}"), &format!("merge_{i}"), "input")?;
        }
        dag.add_node("end_merge", EndMergeNode::new(segment_manager));
        for i in 0..merge_ops.len() {
            dag.connect(&format!("merge_{i}"), "result", "end_merge", &format!("in_{i}"))?;
        }
        dag.add_node("save", SaveMetasNode::new(opstamp, payload));
        dag.add_node("gc", GCNode);
        dag.add_node("reload", ReloadNode);
        dag.connect("end_merge", "done", "save", "trigger")?;
        dag.connect("save", "done", "gc", "trigger")?;
        dag.connect("gc", "done", "reload", "trigger")?;

        let result = scheduler.execute_dag(&mut dag)?;
        // Log les métriques
    } else {
        // commit_fast : pas de DAG, juste purge + save
        let entries = self.shared.purge_deletes(opstamp)?;
        self.shared.segment_manager.commit(entries);
        self.shared.save_metas(opstamp, payload)?;
    }
}
```

### 3.3 Ce qui est supprimé

- `drain_all_merges()` entier (436-541)
- `rebuild_deferred_sfx()` entier
- `track_segments()` / `untrack_segments()`
- `gc_protected_segments` dans SegmentUpdaterShared
- `use_deferred_sfx` flag dans MergeState
- `merge_sfx_deferred()` dans merger.rs
- Le code de skip/rebuild dans load_sfx_files
- Les `eprintln!("[merge_sfx]...")` debug → remplacés par NodeContext.metric()

## Phase 4 : Observabilité avancée (~150 lignes)

### 4.1 Bench post-mortem via DagResult

```rust
// Dans le bench, après le commit :
if let Some(result) = last_dag_result {
    eprintln!("\n=== DAG post-mortem ===");
    for (node, nr) in result.node_results.iter().sorted_by_key(|(_, r)| r.duration_ms) {
        eprintln!("  {:20} {:6}ms  {:?}", node, nr.duration_ms,
            nr.metrics.iter().map(|(k,v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(" "));
    }
}
```

### 4.2 Edge taps pour diagnostique

```rust
// Dans le bench, avant le commit :
let tap = dag.tap("merge_0", "result");
pool.execute_dag(&mut dag)?;
if let Some(TapEvent { value, .. }) = tap.try_recv() {
    let result = value.downcast::<MergeResultData>().unwrap();
    eprintln!("Merge 0: {} docs, {} sfx terms", result.docs_merged, result.sfx_terms);
}
```

### 4.3 Integration diagnostics.rs

Les fonctions `compare_postings_vs_sfxpost`, `inspect_sfx`, etc. restent
mais consomment aussi les métriques du DAG pour un rapport unifié.

## Phase 5 : Futur (non prioritaire)

### 5.1 Checkpoint filesystem
- Persister l'état du DAG entre les nœuds
- Reprendre après un crash (merge 0 ok, merge 1 ok, merge 2 crash → reprendre)
- Inspiré de rag3weaver `checkpoint_store.rs`

### 5.2 Undo/rollback
- Si un nœud fail, cleanup les segments partiels
- Inspiré de rag3weaver Node::undo()

### 5.3 rag3weaver migration
- Les nodes rag3weaver implémenteraient le trait Node de luciole
- Parallélisme gratuit pour les search pipelines
- Observabilité unifiée

## Résumé des tailles estimées

| Phase | Fichiers | Lignes | Quoi |
|-------|---------|--------|------|
| 1 | dag.rs, node.rs, port.rs | ~330 | Core DAG dans luciole |
| 2 | runtime.rs, observe.rs | ~380 | Exécution + events + taps |
| 3 | 6 nœuds + intégration | ~200 | Nœuds lucivy + handle_commit |
| 4 | bench + diagnostics | ~150 | Observabilité avancée |
| **Total** | | **~1060** | |

Code supprimé du merge actuel : ~500 lignes (drain, rebuild, gc_protected, deferred)
Code net ajouté : ~560 lignes

## Ordre de validation

1. Phase 1 : `cargo test` sur les tests DAG unitaires
2. Phase 2 : test d'intégration DAG simple avec le scheduler
3. Phase 3 : `test_single_handle_highlights` + `test_cold_scheduler` passent
4. Phase 3 : bench 20K → SFX mutex = ground truth, 0 mismatch
5. Phase 3 : bench 90K → pas de panic, tous les queries 20 hits
6. Phase 4 : bench 90K avec post-mortem DAG + edge taps

## Dépendances

- **rayon** : optionnel, pour le parallélisme des merges (alternative : crossbeam scoped)
- **flume** : déjà utilisé dans luciole pour les mailboxes
- Pas de tokio, pas de async_broadcast, pas de serde obligatoire
