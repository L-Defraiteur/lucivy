# Design : execute_dag async — DAG piloté par pipe_to

## Contexte

`execute_dag` est synchrone : il bloque le thread appelant pendant toute
l'exécution du DAG. Quand il tourne sur un scheduler thread (via
`task_pipe_to`), les nodes parallèles sont forcées inline (séquentiel)
pour éviter les cooperative waits récursives.

C'est safe mais :
1. **Pas de parallélisme** quand on est sur un scheduler thread
2. **Thread capturé** pendant toute la durée du DAG
3. **Incompatible** avec le pattern pipe_to (résultat devrait arriver
   comme un message, pas bloquer un thread)

## Objectif

`execute_dag_async` : le DAG s'exécute **entièrement via le scheduler**,
niveau par niveau, sans capturer de thread. Le résultat arrive via pipe_to.

```rust
// Usage depuis un acteur :
execute_dag_async(dag, &self.self_ref, "commit_dag", |result| {
    ShardMsg::DagDone { result }
});
return ActorStatus::Continue;

// Usage depuis un thread externe (backward compat) :
let result = execute_dag(dag, None)?;  // inchangé, synchrone
```

## Analyse de l'existant

### execute_dag actuel (runtime.rs)

```
for (level_idx, level) in topological_levels:
    if inline:
        for node in level: execute_single_node(node)
    else:
        execute_level_parallel(level)
            → submit_task per node
            → scheduler.wait() per task   ← BLOQUANT
```

### Ce que gère execute_dag

- **Topological levels** : nodes ordonnées par dépendances
- **Port data** : HashMap<(node, port), PortValue> — passe entre levels
- **Consumer counts** : fan-out tracking (clone quand >1 consumer)
- **Trigger ports** : skip nodes dont le trigger n'est pas satisfait (BranchNode)
- **Undo stack** : rollback en ordre inverse si un node fail
- **Checkpoints** : persistence du progrès (execute_dag_with_checkpoint)
- **Services** : Arc<ServiceRegistry> injecté dans NodeContext
- **Events** : DagEvent émis à chaque étape
- **Taps** : TapRegistry pour observer les valeurs inter-nodes

### Ownership des nodes

Le `Dag` possède les nodes (`Vec<DagNodeEntry>` avec `Box<dyn Node>`).
L'exécution parallèle actuelle utilise `unsafe ptr::read/ptr::write`
pour extraire les nodes, les envoyer sur un task thread, puis les
remettre. C'est safe car :
- Un seul thread accède à un node à la fois
- Les nodes sont `Send` (trait bound)
- Le node est remis AVANT que le prochain level ne commence

## Design : DagExecutor actor

Au lieu de faire execute_dag dans une boucle synchrone, on crée un
**DagExecutor** : un acteur éphémère qui pilote le DAG niveau par niveau.

### Message enum

```rust
enum DagExecMsg {
    /// Start execution — sent once.
    Start,
    /// Level completed — sent by collect_to when all nodes of a level finish.
    LevelDone {
        level_idx: usize,
        results: Vec<NodeTaskResult>,
    },
    /// External: cancel execution.
    Cancel,
}
```

### DagExecutor state

```rust
struct DagExecutor<R: Send + 'static> {
    dag: Dag,
    levels: Vec<Vec<usize>>,
    current_level: usize,
    port_data: HashMap<(String, String), PortValue>,
    consumer_counts: HashMap<(String, String), usize>,
    node_results: Vec<(String, NodeResult)>,
    undo_stack: Vec<(usize, Box<dyn Any + Send>)>,
    /// Where to send the final DagResult.
    target: ActorRef<R>,
    map: Option<Box<dyn FnOnce(Result<DagResult, String>) -> R + Send>>,
    self_ref: Option<ActorRef<DagExecMsg>>,
    dag_start: Instant,
}
```

### Flow

```
Start
  → compute topological levels
  → schedule level 0

schedule_level(level_idx):
  → for each node in level:
      collect inputs from port_data
      take node out of dag (unsafe ptr::read)
      submit_task(|| node.execute(inputs))
  → collect_replies_to(task_rxs, &self_ref, "level_N",
      |results| DagExecMsg::LevelDone { level_idx, results })

LevelDone { level_idx, results }:
  → for each result:
      put node back (unsafe ptr::write)
      store outputs in port_data
      push undo context if can_undo
      emit DagEvent
  → if error: rollback undo_stack, send Err to target
  → if more levels: schedule_level(level_idx + 1)
  → if done: build DagResult, send Ok to target, Stop
```

### Parallélisme naturel

Les nodes d'un même level sont soumises comme N tasks indépendantes.
`collect_replies_to` collecte les N résultats sans bloquer.
L'acteur reçoit `LevelDone` quand TOUTES les nodes du level sont finies.
Alors il avance au level suivant.

**Aucun thread ne wait.** Les scheduler threads dispatchent les tasks
et les messages. Le parallélisme est réel (pas inline comme aujourd'hui
sur scheduler thread).

### Undo / Rollback

Si un node échoue :
1. Le LevelDone handler voit l'erreur
2. Il parcourt l'undo_stack en reverse
3. Chaque node.undo() est appelé inline (fast, synchrone)
4. Le résultat Err est envoyé au target

### BranchNode / Trigger ports

Même logique qu'aujourd'hui : dans schedule_level, vérifier les triggers.
Les nodes dont le trigger n'est pas satisfait sont skippées (pas de task
soumise). Leurs résultats sont ajoutés avec `skipped: true`.

### Taps / Events

Émis dans le handler LevelDone, exactement comme dans execute_dag actuel.
Le DagExecutor a accès au DagEventBus et au TapRegistry via le Dag.

### Checkpoints

Le handler LevelDone peut persister un checkpoint après chaque level,
exactement comme execute_dag_with_checkpoint le fait aujourd'hui.

## API publique

```rust
/// Execute a DAG asynchronously. Result delivered as a message.
///
/// Spawns a temporary DagExecutor actor that processes the DAG
/// level by level via pipe_to. No thread is ever blocked.
///
/// When the DAG completes (success or error), `map(result)` constructs
/// a message sent to `target`.
pub fn execute_dag_async<R: Send + 'static>(
    dag: Dag,                    // owned, pas &mut
    target: &ActorRef<R>,
    label: &str,
    map: impl FnOnce(Result<DagResult, String>) -> R + Send + 'static,
) {
    // Spawn DagExecutor actor, send Start message.
}

/// Execute synchronously (backward compat, unchanged).
pub fn execute_dag(
    dag: &mut Dag,
    on_event: Option<&dyn Fn(DagEvent)>,
) -> Result<DagResult, String> {
    // ... existing code, no changes ...
}
```

**Note** : `execute_dag_async` prend `Dag` by value (owned), pas `&mut Dag`.
L'acteur DagExecutor possède le Dag pendant l'exécution. Cela simplifie
le lifetime management — pas de borrow checker issues avec les closures.

Les callers qui utilisent `&mut Dag` aujourd'hui devront adapter :
```rust
// Avant :
let mut dag = build_commit_dag(...)?;
let result = execute_dag(&mut dag, None)?;

// Après (async) :
let dag = build_commit_dag(...)?;
execute_dag_async(dag, &self.self_ref, "commit_dag", |result| {
    ShardMsg::DagDone { result }
});
```

## Ownership : Dag by value vs &mut

`execute_dag` prend `&mut Dag` — le caller garde le Dag et peut le
réutiliser après (ex: pour inspect les nodes).

`execute_dag_async` prend `Dag` by value — le DagExecutor actor possède
le Dag. Quand il est fini, il peut renvoyer le Dag dans le message résultat
si le caller en a besoin :

```rust
pub fn execute_dag_async<R: Send + 'static>(
    dag: Dag,
    target: &ActorRef<R>,
    label: &str,
    map: impl FnOnce(Result<DagResult, String>, Dag) -> R + Send + 'static,
    //                                          ^^^ optionnel : retourne le Dag
)
```

Ou simplement ne pas le retourner — les callers actuels ne réutilisent
pas le Dag après execution.

## Interaction avec les primitives existantes

| Primitive | Rôle dans execute_dag_async |
|-----------|---------------------------|
| `collect_replies_to` | Collecter les N résultats d'un level |
| `submit_task` | Exécuter chaque node sur un thread pool |
| `WaitGraph` | Auto-track par collect_replies_to |
| `DagEvent` | Émis dans LevelDone handler |
| `ActorStatus::Stop` | DagExecutor se suicide quand le DAG est fini |

## Ce qui change vs l'existant

| Aspect | execute_dag (sync) | execute_dag_async |
|--------|-------------------|-------------------|
| Thread bloqué | Oui (toute la durée) | Non |
| Parallélisme sur sched thread | Inline (forcé) | Réel (submit_task) |
| Ownership Dag | &mut Dag | Dag (by value) |
| Résultat | Return value | Message via pipe_to |
| Undo | Inline dans la boucle | Inline dans LevelDone |
| Checkpoints | Après chaque node | Après chaque level |
| Callers actors | Via task_pipe_to | Direct |
| Callers threads | Direct | Via scheduler.wait wrapper |

## Backward compat

- `execute_dag` (sync) reste inchangé — thread externe et tests l'utilisent
- `execute_dag_async` est additif
- Migration incrémentale : les callers depuis des actors switchent un par un

## Plan d'implémentation

### Étape 1 : NodeTaskResult
Type pour les résultats des tasks de nodes (node_idx, name, NodeResult,
outputs, node_box). Déjà un tuple inline — en faire un struct propre.

### Étape 2 : DagExecutor actor
- DagExecMsg enum
- DagExecutor struct avec state
- on_start pour self_ref
- handle(Start) → schedule level 0
- handle(LevelDone) → process results → schedule next or finish
- Undo/rollback dans LevelDone error path

### Étape 3 : schedule_level helper
- Extraire les nodes (unsafe ptr::read)
- Vérifier triggers (skip if unsatisfied)
- Soumettre N tasks via submit_task
- collect_replies_to → LevelDone

### Étape 4 : execute_dag_async public function
- Spawn DagExecutor
- Send Start
- Target + map passés au constructeur

### Étape 5 : Tests
- Test async basic (linear DAG)
- Test async parallel (2 nodes same level)
- Test async with branch (trigger skip)
- Test async with undo (failure + rollback)
- Test async from actor handler (pas de task_pipe_to nécessaire)

### Étape 6 : Migration callers
- search_dag dans lucivy_core → execute_dag_async
- commit DAG dans segment_updater → execute_dag_async (via ShardActor)
