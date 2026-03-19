# Doc 16 — Progression : tout en DAG

Date : 19 mars 2026

## Fait dans cette session (suite du doc 15)

### Phase 1 : Search DAG ✅

```
drain → flush → build_weight → [search_shard_0, ..N] ∥ → merge_results
```

- 5 nœuds typés dans `search_dag.rs`
- ShardedHandle::search() appelle execute_dag()
- Résultats extraits via DagResult::take_output()

### Phase 2 : Commit fast DAG ✅

Même DAG que le commit full mais avec `merge_ops = vec![]` :
```
prepare → save_metas → gc → reload
```

### Architecture améliorée ✅

**DagResult::take_output::<T>(node, port)**
- Le DAG retourne les outputs des nœuds leaf
- Plus besoin de Arc<Mutex> hacks
- N'importe quel DAG peut retourner des données typées

**Dag::set_initial_input(node, port, value)**
- Injection de valeurs avant exécution
- collect_inputs les trouve automatiquement
- Utilisé par GraphNode pour injecter les inputs du parent

**GraphNode réécrit proprement**
- Utilise set_initial_input() au lieu de InjectNode hack
- Utilise take_output() au lieu de CollectNode hack
- Supprimé as_any_mut() du trait Node
- -167 lignes net

### Bench 5K ✅
- Passe en debug et en release
- Pas de panic gapmap
- Search correcte avec highlights
- DagResult affiché pour chaque commit

## État des tests

| Crate | Pass | Fail |
|-------|------|------|
| luciole | 132 | 0 |
| ld-lucivy | 1188 | 1 (test_merge_single_filtered — pré-existant) |
| lucivy-core | 83 | 0 |

## Reste à faire (doc 15 phases 3-7)

### Phase 3 : MergeState → GraphNode
6 sous-nœuds : init → postings_N → store → fast_fields → sfx → close
Le MergeNode du commit DAG deviendrait un GraphNode.

### Phase 4 : merge_sfx → sous-graphe
6 sous-nœuds : collect_tokens → build_fst → copy_gapmap → merge_sfxpost → validate → write
Le nœud sfx du phase 3 deviendrait lui-même un GraphNode.

### Phase 5 : Unification events
Supprimer eprintln, lucivy_trace!, IndexEvent duplicates.
Un seul subscribe_dag_events() pour tout.

### Phase 6 : Fix test_merge_single_filtered
Faire passer merger.write() par le même GraphNode que MergeState.

### Phase 7 : Delete DAG + Ingestion observabilité

## Commits de cette sous-session

```
f08e3f5 docs: plan — tout en DAG, observabilité totale
21877b3 feat(lucivy): gapmap validation in merge_sfx + bench 5K passes
84aa4f9 feat: search DAG + DagResult output extraction
6035047 feat: DagResult output extraction + commit_fast DAG + GraphNode cleanup
```

## Architecture actuelle des DAGs

```
COMMIT (rebuild_sfx=true):
  prepare ──┬── merge_0 ──┐
            ├── merge_1 ──┼── finalize ── save ── gc ── reload
            └── merge_2 ──┘

COMMIT (rebuild_sfx=false):
  prepare ── save ── gc ── reload

SEARCH:
  drain ── flush ── build_weight ──┬── search_0 ──┐
                                   ├── search_1 ──┼── merge_results
                                   └── search_2 ──┘
```

Tout observable via subscribe_dag_events(). Tout tappable via dag.tap().
