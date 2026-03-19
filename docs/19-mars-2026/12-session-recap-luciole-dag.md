# Doc 12 — Recap session : luciole DAG framework + migration lucivy

Date : 19 mars 2026
Branche : `feature/luciole-dag` (14 commits)

## Ce qui a été fait

### Luciole — framework complet de coordination (zéro dépendance lucivy)

| Fichier | Lignes | Contenu |
|---------|--------|---------|
| `port.rs` | 135 | PortType (TypeId), PortValue (Arc, fan-out, Clone) |
| `node.rs` | 400 | Node sync, PollNode coopératif, undo/rollback, ServiceRegistry |
| `dag.rs` | 370 | Dag, connect (types vérifiés), chain, taps, validate, topo sort par niveaux |
| `runtime.rs` | 1250 | execute_dag, execute_dag_with_checkpoint, rollback auto, display_progress ASCII, NodeLog events |
| `observe.rs` | 130 | TapRegistry zero-cost, TapEvent, tap par edge ou tap_all |
| `checkpoint.rs` | 230 | CheckpointStore trait, MemoryCheckpointStore, FileCheckpointStore |
| `pool.rs` | 350 | Pool (spawn, send, scatter, drain, shutdown), DrainableRef |
| `scope.rs` | 100 | Scope (drain ordonné, execute_dag lifecycle) |
| `scheduler.rs` | +100 | WorkItem (Actor + Task unifié), submit_task |
| `mailbox.rs` | +20 | ActorRef::request() helper |

**127 tests luciole, 0 failures.**

### Primitives luciole

| Primitive | Description | WASM compatible |
|-----------|-------------|:---------------:|
| **Node** | Nœud sync, inputs/outputs typés, métriques/logs | ✅ |
| **PollNode** | Nœud coopératif (yield entre steps) | ✅ |
| **Dag** | Graphe avec topo sort par niveaux, validation types | ✅ |
| **execute_dag** | Parallèle via pool, events, taps, rollback | ✅ |
| **Pool** | N workers identiques, round-robin/key-routed, scatter | ✅ |
| **Scope** | Drain ordonné + execute_dag lifecycle | ✅ |
| **submit_task** | Tâche one-shot sur le pool (même threads que acteurs) | ✅ |
| **TapRegistry** | Interception zero-cost des données entre nœuds | ✅ |
| **CheckpointStore** | Persistence du progrès DAG (Memory + File) | ✅ |
| **Undo/rollback** | Annulation en ordre inverse si un nœud fail | ✅ |
| **DagEvent bus** | Subscribe depuis n'importe quel thread | ✅ |
| **display_progress** | Arbre ASCII du DAG avec statut par nœud | ✅ |
| **DagResult** | display_summary(), total("metric"), Display impl | ✅ |
| **ServiceRegistry** | Services partagés optionnels dans NodeContext | ✅ |

### Migration lucivy (en cours)

| Composant | Avant | Après | Status |
|-----------|-------|-------|--------|
| **Commit (rebuild_sfx)** | drain_all_merges + flags | DAG: prepare → merges ∥ → finalize → save → gc → reload | ✅ fait |
| **IndexWriter workers** | Vec\<ActorRef\> + AtomicUsize | Pool\<Envelope\> | ✅ fait |
| **Commit (fast)** | inline purge + save | handle_commit_fast (inchangé) | ✅ existant |
| **SuDrainMergesMsg** | drain_all_merges seul | Full DAG commit | ✅ fait |
| **Sharded pipeline** | Manual Vec + drain | Pool + Scope | ❌ à faire |
| **Close** | Manual gather-sync | Scope.shutdown() | ❌ à faire |
| **IndexerActor** | GenericActor\<Envelope\> | GenericActor (inchangé, Pool wraps) | ✅ |
| **SegmentUpdaterActor** | GenericActor | GenericActor + handle_commit_dag | ✅ |

### Bugs corrigés par le DAG

| Bug | Cause | Resolution |
|-----|-------|------------|
| **GC race** (doc 04) | GC pendant merge | GC est un nœud APRÈS les merges |
| **Merge cascade** (garbage_collect test) | Résultats de merge pas re-mergés | collect_merge_candidates exhaustif |
| **Segments sans sfx** (doc 03) | Timing FinalizerActor ↔ merge | Merges après prepare (commit + start_merge) |
| **Mmap cache stale** (doc 01) | Reload avant écriture finie | Un seul reload à la fin du DAG |
| **test_delete_during_merge** | Delete pas visible post-merge | Purge avant merges, save après finalize |
| **test_index_events** | Events pas émis depuis DAG | MergeNode émet IndexEvent::MergeStarted/Completed |

**Tests : 1188 pass, 1 fail** (test_merge_single_filtered_segments —
bug dans merger.write() standalone, pas lié au DAG).

Avant le DAG : **7 fails**. Le DAG corrige **6 bugs** par construction.

## Architecture du commit DAG

```
prepare ──┬── merge_0 ──┐
          ├── merge_1 ──┼── finalize ── save_metas ── gc ── reload
          └── merge_2 ──┘
```

- **prepare** : purge_deletes + commit segment manager + start_merge (lock segments)
- **merge_N** : PollNode wrapping MergeState::step() (coopératif, parallel)
- **finalize** : end_merge pour chaque résultat + advance deletes
- **save_metas** : écriture atomique meta.json
- **gc** : garbage collect (safe, aucun merge en cours)
- **reload** : reader voit les nouveaux segments

Tout sur le **même pool de threads** que les acteurs. Pas de thread temporaire.
Compatible WASM (cooperative pumping via run_one_step).

## Ce qui reste à faire

### Court terme
1. **Sharded pipeline** : Pool + Scope pour ReaderActors, RouterActor, ShardActors
2. **Close** : Scope.shutdown() au lieu de la logique manuelle
3. **test_merge_single_filtered** : investiguer le bug merger.write()

### Moyen terme
4. **GraphNode** : un DAG comme nœud d'un autre DAG (composition)
5. **Acteurs typés** : migrer GenericActor → Actor\<Msg=MyEnum\> (IndexerActor, ShardActor)
6. **Bench 20K/90K** : valider que le DAG résout le panic gapmap

### Long terme (convergence rag3weaver)
7. **Publier luciole** comme crate séparé
8. **Migrer rag3weaver** : supprimer les 3800 lignes de dataflow, importer luciole
9. **sync DB calls** pour WASM compat (block_on en natif, sync en WASM)

## Commits de la session

```
3c19d4c feat(luciole): add DAG runtime — core structure + parallel execution
74b4686 feat(luciole): unified thread pool — submit_task + DAG on scheduler
f96b140 feat(luciole): add Pool abstraction + ActorRef::request() helper
98fd32a feat(luciole): add Scope (drain + DAG lifecycle) and Dag::chain()
b7e9463 feat(luciole): Pool shutdown, DrainableRef, from_refs, Drainable impl
156d975 feat(luciole): global DagEvent bus — subscribe from any thread
72e630e feat(luciole): advanced observability — taps, live logs, display, metrics
3f9f002 feat(luciole): PollNode (cooperative async) + ServiceRegistry
4923fc1 feat(luciole): undo/rollback on DAG failure
55a253d feat(luciole): checkpoint store + ASCII progress tree
471457c feat(lucivy): commit DAG nodes + diagnostic test exposing merge bug
5bfa000 feat(lucivy): wire commit DAG into handle_commit + SuDrainMergesMsg
893cf1d fix(lucivy): rewrite commit DAG — all steps in DAG, correct ordering
dca3ae9 refactor(lucivy): IndexWriter workers use Pool instead of Vec<ActorRef>
```
