# Doc 19 — Recap progression 19 mars 2026

Branche : `feature/luciole-dag` (~30 commits)

## Luciole — framework (132+ tests)

15 primitives : Node, PollNode, Dag, GraphNode, StreamDag, Pool,
Scope, submit_task, TapRegistry, CheckpointStore, ServiceRegistry,
DagEvent bus, display_progress, DagResult (take_output, total, display),
undo/rollback.

Un seul pool de threads persistants. WASM compatible. Zero-cost observability.

## Lucivy — migration

| Quoi | Status |
|------|--------|
| Commit (rebuild_sfx) → DAG | ✅ |
| Commit (fast) → DAG | ✅ |
| Search → DAG | ✅ |
| IndexWriter workers → Pool | ✅ |
| Sharded actors → typés (ShardMsg, RouterMsg, ReaderMsg) | ✅ |
| drain_pipeline → Pool.drain + request | ✅ |
| merge_sfx → fonctions standalone | ✅ |
| IndexMerger.readers → Arc (partageable) | ✅ |
| sfx_dag nodes prêts | ✅ (pas encore branché en parallèle) |
| MergeNode per-phase metrics | ✅ |
| Gapmap validation post-merge | ✅ |
| Bench 5K | ✅ (passe en debug + release) |

## Bugs corrigés

6 bugs de merge corrigés par le DAG (7 fails → 1).
Le 1 restant est `test_merge_single_filtered` (merger.write standalone).

## Tests

| Crate | Pass | Fail |
|-------|------|------|
| luciole | 132 | 0 |
| ld-lucivy | 1188 | 1 |
| lucivy-core | 83 | 0 |

## Prochaines étapes

- Brancher sfx_dag pour paralléliser build_fst / copy_gapmap / merge_sfxpost
- Bench 20K / 90K
- Phase 5 : unifier events (supprimer eprintln, lucivy_trace)
- Phase 6 : fix test_merge_single_filtered
- Migrer IndexerActor/FinalizerActor vers typés
- Publier luciole comme crate séparé
