# Doc 03 — Rapport de session pour continuation

Date : 20 mars 2026
Branche : `feature/luciole-dag`
Dernier commit : `4a957be` (validation sfxpost)
État : **43 tests fail** (volontairement — merge_sfxpost refuse les segments sans sfxpost)

## Ce qui a été fait dans cette session

### Luciole framework (complet, 132+ tests, 0 fail)
Crate séparé dans `luciole/`. Framework de coordination multi-threadé :
- **Node, PollNode, Dag, GraphNode, StreamDag** : exécution DAG
- **Pool, Scope, DrainableRef** : gestion d'acteurs
- **submit_task, WorkItem** : pool de threads unifié (acteurs + tâches)
- **TapRegistry, DagEvent bus, display_progress** : observabilité
- **CheckpointStore, undo/rollback** : persistence + recovery
- **DagResult::take_output()** : extraction typée des outputs DAG
- **Dag::set_initial_input()** : injection de valeurs avant exécution
- Tout WASM compatible via cooperative wait

### Migration lucivy vers DAG
- **Commit DAG** : prepare → merges ∥ → finalize → save → gc → reload
- **Search DAG** : drain → flush → build_weight → search_shard_N ∥ → merge_results
- **Commit fast** : aussi via DAG (même observabilité)
- **IndexWriter** : `Pool<Envelope>` remplace Vec<ActorRef> + round-robin
- **Sharded pipeline** : acteurs typés (ShardMsg, RouterMsg, ReaderMsg) + Pool
- **merge_sfx parallélisé** : build_fst, copy_gapmap, merge_sfxpost via submit_task
- **IndexMerger.readers** : `Arc<Vec<SegmentReader>>` (partageable)
- **sfx_merge.rs** : 6 fonctions standalone extraites de merger.rs

### SegmentComponent refactoring (PHASE 1 — COMPLÈTE)
- `SegmentComponent::SuffixFst { field_id }` et `SuffixPost { field_id }` comme variants natifs
- `InnerSegmentMeta.sfx_field_ids: Vec<u32>` — persisté dans meta.json
- `list_files()` inclut automatiquement les per-field .sfx/.sfxpost
- `SegmentMeta::with_sfx_field_ids()` pour propager
- `segment_writer::finalize()` retourne `(Vec<u64>, Vec<u32>)` (opstamps + sfx_field_ids)
- Manifest .sfx supprimé (no-op), backward compat via legacy manifest reader
- GC protège nativement les fichiers per-field
- **1194 tests passaient AVANT la phase 3**

### Validation (PHASE 2 — COMPLÈTE)
- `validate_sfxpost()` dans sfx_merge.rs : checks doc_ids, num_tokens, offsets
- `validate_gapmap()` existait déjà
- Les deux derrière `LUCIVY_SKIP_VALIDATION=1` env var (enabled par défaut)
- Appelées après chaque construction (segment_writer + merge)

### Merge fiable (PHASE 3 — EN COURS, 43 TESTS FAIL)
- `merge_sfxpost` retourne `Err` si un segment source a un terme mais pas de sfxpost
- 43 tests fail : tests d'aggregation tantivy qui créent des segments SANS SfxCollector
- Le segment_writer lucivy a le SfxCollector → crée toujours sfxpost
- Mais les tests internes ld-lucivy utilisent `Index::writer_for_tests()` sans SfxCollector

## Le bug diagnostiqué

### Symptôme
contains search "function" : 1285 hits vs 1305 ground truth (16-20 docs manquants)

### Root cause confirmée
sfxpost merge perd des docs quand un segment source n'a PAS de sfxpost.
Le `reverse_doc_map.get(&doc_id)` retourne None pour les docs de ces segments → entries silencieusement ignorées.

### Preuve
Deep inspection shard_0 doc_298 :
- term "function" IS dans le term dict (ordinal 15542)
- sfx file exists, sfxpost exists
- sfxpost a 637 entries pour "function"
- MAIS doc_id 298 PAS dans le sfxpost
- Le doc vient d'un segment source qui n'avait pas de sfxpost

### Pourquoi des segments sans sfxpost ?
- Le SfxCollector EST appelé dans segment_writer::finalize() → il ÉCRIT le sfxpost
- Mais les fichiers per-field n'étaient PAS dans SegmentComponent → le GC pouvait les supprimer
- CORRIGÉ par la phase 1 (sfx_field_ids dans le meta)

## État actuel du code

### Tests
- luciole : 132 pass, 0 fail
- ld-lucivy : 1151 pass, 43 fail (merge_sfxpost error intentionnelle)
- lucivy-core : 83 pass, 0 fail

### Les 43 tests qui fail
Tous des tests d'aggregation internes à tantivy qui font des merges.
Ils n'ont pas de SfxCollector → segments sans sfxpost → merge_sfxpost refuse.

Options pour les fixer :
1. **Le SfxCollector devrait être dans le segment_writer core** (pas juste lucivy_core)
2. Ou : merge_sfxpost tolère les segments sans sfx SI aucun segment n'a de sfx pour ce champ
3. Ou : les tests d'aggregation n'utilisent pas de champs text indexés (peu probable)

L'option 1 est la plus propre mais c'est un gros refactoring.
L'option 2 est un compromis raisonnable.

## Fichiers clés modifiés

### Luciole (tout nouveau)
```
luciole/src/port.rs, node.rs, dag.rs, runtime.rs, observe.rs,
checkpoint.rs, pool.rs, scope.rs, graph_node.rs, stream_dag.rs,
scheduler.rs (WorkItem), mailbox.rs (request), events.rs, lib.rs
```

### ld-lucivy
```
src/index/segment_component.rs    — RÉÉCRIT (SuffixFst{field_id}, SuffixPost{field_id})
src/index/index_meta.rs           — sfx_field_ids, with_sfx_field_ids()
src/index/segment_reader.rs       — load_sfx_files via sfx_field_ids (+ legacy fallback)
src/indexer/segment_writer.rs     — finalize retourne (opstamps, sfx_field_ids)
src/indexer/segment_serializer.rs — write_sfx_manifest est un no-op
src/indexer/segment_updater.rs    — list_files simplifié (plus de manifest)
src/indexer/segment_updater_actor.rs — handle_commit_dag, handle_commit_fast
src/indexer/merge_state.rs        — step_sfx parallélisé, sfx_field_ids propagé
src/indexer/merger.rs             — readers Arc, merge_sfx thin wrapper
src/indexer/sfx_merge.rs          — NOUVEAU : 6 fonctions standalone + validation
src/indexer/sfx_dag.rs            — NOUVEAU : DAG nodes (pas encore branché)
src/indexer/commit_dag.rs         — NOUVEAU : commit DAG complet
src/indexer/index_writer.rs       — Pool<Envelope>, sfx_field_ids propagation
src/indexer/single_segment_index_writer.rs — sfx_field_ids propagation
src/indexer/mod.rs                — nouveaux modules
src/suffix_fst/gapmap.rs          — validate(), GapMapError
```

### lucivy_core
```
lucivy_core/Cargo.toml            — dépendance luciole ajoutée
lucivy_core/src/lib.rs            — search_dag module
lucivy_core/src/sharded_handle.rs — acteurs typés, Pool, search DAG
lucivy_core/src/search_dag.rs     — NOUVEAU : search DAG complet
lucivy_core/tests/test_search_mismatch.rs — diagnostic des docs manquants
```

## Docs de la session (dans docs/19-mars-2026/ et docs/20-mars-2026/)
```
05-18 dans 19-mars : DAG design, vision, plans, recaps, observabilité
01    dans 20-mars : architecture SFX propre (6 principes)
02    dans 20-mars : progression SegmentComponent
03    dans 20-mars : ce rapport
```

## Prochaine étape prioritaire

Fixer les 43 tests. L'approche recommandée :

Dans `merge_sfxpost`, distinguer :
- Si AUCUN segment source n'a de sfx pour ce champ → skip (pas de sfxpost à merger)
- Si AU MOINS UN a du sfx mais d'autres non → erreur (segments incomplets)

C'est l'option 2 ci-dessus. Le code actuel dans `merger.rs::merge_sfx` fait déjà `if !any_has_sfx { continue; }` au début de chaque champ. Il suffit de passer cette info à `merge_sfxpost`.

Après ça : bench 5K avec LUCIVY_VERIFY=1 pour vérifier que les 16 docs manquants sont trouvés.
