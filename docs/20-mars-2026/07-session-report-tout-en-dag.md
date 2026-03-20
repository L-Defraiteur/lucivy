# Doc 07 — Rapport de session : tout en DAG

Date : 20 mars 2026
Branche : `feature/luciole-dag`
État : **1194 tests pass, 0 fail**

## Ce qui a été fait dans cette session

### Pipeline commit/merge redesigné (doc 05)

Suppression complète de la state machine de merge (~780 lignes) :
- `ActiveMerge`, `ExplicitMerge`, `pending_merges`, `segments_in_merge`
- `drain_all_merges()`, `do_end_merge()`, `SuMergeStepMsg`, `SuDrainMergesMsg`
- `gc_protected_segments` (hack `0..10` field_ids)

Remplacé par : boucle cascade dans `handle_commit()`. Merge candidates en pool unifié (committed + uncommitted).

### Bugs corrigés

1. **merge_sfxpost `if` → `else if`** : le check d'erreur Phase 3 s'exécutait TOUJOURS, même quand sfxpost existait. Causait `Done(None)` → `end_merge(ids, None)` → données perdues.

2. **`step()` silent error** : `MergeState::step()` avalait les erreurs avec `warn!` et retournait `Done(None)`. Maintenant propage `Err`.

3. **Double save_metas/GC** : `drain_all_merges()` faisait save+gc, puis le commit DAG refaisait save+gc → le deuxième `segment_manager.commit()` écrasait le segment mergé.

4. **Fan-out SIGSEGV** : `doc_id_mapping` connecté à 2 inputs → `Arc::try_unwrap` échouait silencieusement → downstream unwrap sur None → SIGSEGV. Fix : `PortValue::take()` panic avec message clair sur fan-out.

### Stemming supprimé (Phase 4)

- Supprimé `sfx_raw_analyzer` et le double tokenization path
- Un seul chemin : `SfxTokenInterceptor` capture les tokens pendant le BM25 indexing
- Le stemming n'a pas de sens pour du code search

### Merge en DAG complet (merge_dag.rs)

```
init ──┬── postings ──────────┐
       ├── store ─────────────┼── sfx ── close
       └── fast_fields ───────┘
```

- `SegmentSerializer::decompose()` sépare les writers indépendants
- Postings, Store, FastFields en parallèle
- `MergeState` supprimé (417 lignes) — remplacé par `merge_dag`
- `MergeNode` est un `Node` simple (plus de `PollNode`)

### SFX merge en sous-DAG (sfx_dag.rs branché)

```
collect_tokens ──┬── build_fst ─────────────────────────┐
                 ├── copy_gapmap ── validate_gapmap ─────┼── write_sfx
                 └── merge_sfxpost ── validate_sfxpost ──┘
```

- Adapté pour écrire via `Segment` directement (plus de SegmentSerializer)
- Imbriqué dans le merge_dag : commit_dag > merge_dag > sfx_dag

### Scatter DAG (luciole/scatter.rs)

Nouveau lego composable pour le pattern scatter-gather :
- Tâches nommées : `Vec<(&str, F)>`
- `CollectNode` produit `HashMap<String, PortValue>`
- `ScatterResults` wrapper avec `take::<T>("name")`
- Utilisé pour index opening et SFX build dans finalize

### Index opening parallélisé

`SegmentReader::open` en parallèle via scatter DAG quand multiple segments.

### Zéro submit_task

Plus aucun appel direct à `submit_task` dans le codebase. Tout passe par des DAGs composables.

## Architecture DAG finale

| Lego | Structure | Parallélisme |
|------|-----------|-------------|
| **commit_dag** | prepare → merges ∥ → finalize → save → gc → reload | Merges en parallèle |
| **merge_dag** | init → postings ∥ store ∥ fast_fields → sfx → close | 3 phases en parallèle |
| **sfx_dag** | collect → build_fst ∥ copy_gapmap ∥ merge_sfxpost → validate → write | 3 étapes en parallèle |
| **scatter** (opening) | seg_0 ∥ seg_1 ∥ ... → collect | N readers en parallèle |
| **scatter** (sfx build) | field_0 ∥ field_1 ∥ ... → collect | N fields en parallèle |
| **search_dag** | drain → flush → build_weight → shard_N ∥ → merge_results | N shards en parallèle |

Nesting : `commit_dag > merge_dag > sfx_dag` (3 niveaux)

## Commits de cette session

```
d108c42 refactor: single DAG pipeline, remove stemming, fix merge bugs (-779 lignes)
6729371 feat: wire sfx_dag into merge pipeline
a0b4e75 feat: parallel index opening + parallel SFX build
38b1019 feat: merge as full DAG — postings ∥ store ∥ fast_fields
d3cbb4e refactor: replace all submit_task with DAGs, delete MergeState
7d932a4 fix: PortValue::take() panics on fan-out instead of silent None
f7c8135 feat: scatter DAG with named results, zero submit_task
```

## Prochaine étape

**Bench 5K avec `LUCIVY_VERIFY=1`** : vérifier que les 16 docs manquants du contains search sont maintenant trouvés. C'est la validation de tout le travail SegmentComponent + merge_sfxpost + pipeline.
