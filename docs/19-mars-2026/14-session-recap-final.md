# Doc 14 — Recap session : luciole framework + migration lucivy complète

Date : 19 mars 2026
Branche : `feature/luciole-dag` (20 commits)

## Vue d'ensemble

Une session de construction d'un framework complet de coordination
multi-threadé (luciole) et migration de lucivy pour l'utiliser.

**Résultat** : 6 bugs corrigés par construction, code réduit, observable.

## Luciole — framework (132+ tests, 15 fichiers source)

### Fichiers

| Fichier | Lignes | Contenu |
|---------|--------|---------|
| `port.rs` | 135 | PortType (TypeId), PortValue (Arc, fan-out, Clone) |
| `node.rs` | 450 | Node sync, PollNode coopératif, undo/rollback, ServiceRegistry, as_any_mut |
| `dag.rs` | 380 | Dag, connect (types vérifiés), chain, taps, validate, topo sort, node_mut_by_name |
| `runtime.rs` | 1300 | execute_dag, execute_dag_with_checkpoint, rollback, display_progress, NodeLog, total() |
| `observe.rs` | 130 | TapRegistry zero-cost, TapEvent |
| `checkpoint.rs` | 230 | CheckpointStore trait, Memory + File impls |
| `pool.rs` | 370 | Pool (spawn, send, scatter, drain, shutdown, from_refs), DrainableRef, Clone |
| `scope.rs` | 100 | Scope (drain ordonné, execute_dag lifecycle) |
| `graph_node.rs` | 350 | GraphNode (sub-DAG comme nœud), InjectNode, CollectNode |
| `stream_dag.rs` | 200 | StreamDag (topologie sur acteurs, drain ordonné, display) |
| `scheduler.rs` | +100 | WorkItem (Actor + Task unifié), submit_task |
| `mailbox.rs` | +20 | ActorRef::request() helper |
| `events.rs` | existant | EventBus zero-cost |

### Primitives

| Primitive | Description | WASM |
|-----------|-------------|:----:|
| **Node** | Nœud sync, ports typés, métriques/logs, undo | ✅ |
| **PollNode** | Yield coopératif (merge long-running) | ✅ |
| **Dag** | Graphe + topo sort par niveaux + validation + chain | ✅ |
| **execute_dag** | Parallèle via pool, events, taps, rollback, checkpoint | ✅ |
| **GraphNode** | Sub-DAG comme nœud (composition hiérarchique) | ✅ |
| **StreamDag** | Topologie pipeline sur acteurs, drain ordonné | ✅ |
| **Pool** | N workers, round-robin/key-routed, scatter, drain, shutdown | ✅ |
| **Scope** | Drain cascadé + execute_dag lifecycle | ✅ |
| **submit_task** | Tâche one-shot sur le même pool que les acteurs | ✅ |
| **TapRegistry** | Interception zero-cost des données entre nœuds | ✅ |
| **CheckpointStore** | Persistence du progrès (Memory + File) | ✅ |
| **ServiceRegistry** | Services partagés optionnels | ✅ |
| **DagEvent bus** | Subscribe depuis n'importe quel thread | ✅ |
| **display_progress** | Arbre ASCII du DAG | ✅ |
| **DagResult** | display_summary(), total(), Display | ✅ |

## Migration lucivy

### Commit DAG (ld-lucivy)

Remplace `drain_all_merges()` + flags + locks par un DAG structurel :

```
prepare ──┬── merge_0 ──┐
          ├── merge_1 ──┼── finalize ── save_metas ── gc ── reload
          └── merge_2 ──┘
```

- **PrepareNode** : purge_deletes + commit + start_merge
- **MergeNode** : PollNode wrapping MergeState::step()
- **FinalizeNode** : end_merge + advance_deletes
- **SaveMetasNode, GCNode, ReloadNode** : séquentiels

6 bugs corrigés par construction :
- GC race, merge cascade, segments sans sfx, mmap cache stale,
  delete visibility, event emission

### IndexWriter (ld-lucivy)

`Vec<ActorRef<Envelope>>` + `AtomicUsize` → `Pool<Envelope>`
- send_add_documents_batch : `pool.send()` (round-robin)
- prepare_commit : `pool.worker(i)` scatter
- wait_merging_threads : `pool.broadcast(Shutdown)`

### Sharded Pipeline (lucivy_core) — ACTEURS TYPÉS

Migration de GenericActor<Envelope> vers Actor<Msg=enum> :

**ShardActor** `Pool<ShardMsg>` :
```rust
enum ShardMsg { Search{..}, Insert{..}, Commit{..}, Delete{..}, Drain }
```
- search : `shard_pool.scatter(ShardMsg::Search{..})`
- commit : `shard_pool.scatter(ShardMsg::Commit{..})`
- delete : `shard_pool.send_to(id, ShardMsg::Delete{..})`
- delete all : `shard_pool.broadcast(ShardMsg::Delete{..})`

**RouterActor** `ActorRef<RouterMsg>` :
```rust
enum RouterMsg { Route{doc, node_id, hashes, pre_tokenized}, Drain }
```

**ReaderActor** `Pool<ReaderMsg>` :
```rust
enum ReaderMsg { Tokenize{doc, node_id}, Batch{docs}, Drain }
```

**drain_pipeline** :
```rust
self.reader_pool.drain("readers");
self.router_ref.request(|r| RouterMsg::Drain(DrainMsg(r)), "router");
```

## Tests

| Crate | Pass | Fail | Notes |
|-------|------|------|-------|
| luciole | 132 | 0 | Framework complet |
| ld-lucivy | 1188 | 1 | 1 fail pré-existant (merger.write standalone) |
| lucivy-core | en cours | - | Migration typed actors |

## Docs de la session

```
05 — proposition merge DAG (autre instance Claude)
06 — design merge DAG pour lucivy
07 — vision luciole comme DAG threading lib
08 — plan d'implémentation par phases
09 — design luciole framework complet (Pool, Scope, submit_task)
10 — feedback depuis rag3weaver
11 — plan observabilité + convergence rag3weaver (PollNode, no async)
12 — recap intermédiaire
13 — plan migration acteurs typés
14 — ce doc
```

## Commits (20)

```
3c19d4c DAG core (port, node, dag, runtime)
74b4686 Unified thread pool (WorkItem, submit_task)
f96b140 Pool + ActorRef::request()
98fd32a Scope + Dag::chain()
b7e9463 Pool shutdown, DrainableRef, from_refs
156d975 Global DagEvent bus
72e630e Advanced observability — taps, live logs, display, metrics
3f9f002 PollNode (cooperative async) + ServiceRegistry
4923fc1 Undo/rollback on DAG failure
55a253d Checkpoint store + ASCII progress tree
471457c Commit DAG nodes + diagnostic test
5bfa000 Wire commit DAG into handle_commit
893cf1d Rewrite commit DAG — all steps in DAG
dca3ae9 IndexWriter workers use Pool
1cc3ec3 Session recap doc
cb2f2d1 GraphNode — sub-DAG as composable Node
9bf77a5 StreamDag — observable pipeline topology
56600ba Typed actors for sharded pipeline
```

## Ce qui reste

### Court terme
- Tests lucivy-core (vérifier que le sharding typé fonctionne E2E)
- Supprimer le code legacy (GenericActor create_shard_actor, etc.)
- Bench 20K / 90K : valider que le DAG résout le panic gapmap

### Moyen terme
- Migrer IndexerActor/FinalizerActor vers typed
- StreamDag avec taps (observer les items en transit)
- GraphNode pour l'ingestion pipeline complet

### Long terme (convergence rag3weaver)
- Publier luciole comme crate séparé (crates.io)
- Migrer rag3weaver dataflow → luciole (supprimer ~3800 lignes)
- PollNode pour les DB calls (sync + WASM compatible)
