# Session 19 mars 2026 — Récapitulatif complet

## Point de départ

Session précédente (18 mars) avait identifié le bug highlight parasites
(fuzzy distance default = 1) et commencé l'investigation du merge_sfx bottleneck.
Branche : `experiment/decouple-sfx`.

## Bugs corrigés cette session

### 1. Contains fuzzy distance default (commit 5fc5871)
- `build_contains_query` : `unwrap_or(1)` → `unwrap_or(0)` (2 endroits)
- Tests régression : `test_no_parasitic_matches_function_disjunction` + `test_single_handle_highlights`

### 2. Deadlock wait_blocking (commit 545315b)
- `index_writer.rs` : `prepare_commit()` et `rollback()` → `wait_cooperative_named`
- `segment_updater.rs` : `schedule_commit_with_rebuild()` → `wait_cooperative_named`
- Cause : shard actor handler appelait `writer.commit()` qui faisait `wait_blocking`,
  bloquant le thread scheduler et empêchant le segment_updater d'être dispatché.

### 3. GC supprimait les sfxpost pendant les merges (commit 51bd1b4)
- `segment_updater.rs` : ajout `gc_protected_segments: Mutex<HashSet<SegmentId>>`
  dans `SegmentUpdaterShared`
- `segment_updater_actor.rs` : `track_segments()` synchronise vers le shared set
- `list_files()` inclut les fichiers des segments protégés → GC ne les supprime plus
- Vérifié : 20K docs, SFX mutex=1375 = ground truth exact, 0 mismatch

### 4. Deferred sfx retiré (commit 545315b)
- `merge_sfx_deferred` gardé dans le code mais plus utilisé
- `step_sfx()` appelle toujours `merge_sfx` complet
- `rebuild_deferred_sfx` supprimé entièrement
- Raison : le mmap cache rendait le rebuild non-fiable (fichier réécrit mais
  cache retournait l'ancien contenu)

## Features implémentées

### Observabilité luciole (commit 545315b)
- `wait_cooperative_named(label, run_step)` dans `reply.rs` :
  warning périodique si wait > seuil + dump scheduler state
- `ActorActivity` dans `scheduler.rs` : track ce que chaque acteur fait
- `dump_state()` : état de tous les acteurs (nom, activité, durée, queue depth)
- `lucivy_trace!()` macro : conditionnel via `LUCIVY_DEBUG=1`
- Labels sur tous les `wait_cooperative` dans `sharded_handle.rs`

### commit_fast() / commit() (commit 545315b)
- `IndexWriter::commit_fast()` / `PreparedCommit::commit_fast()`
- `ShardedHandle::commit_fast()`
- `SuCommitMsg` avec flag `rebuild_sfx`
- `commit()` : drain merges + save_metas
- `commit_fast()` : save_metas seulement (pas de drain)

### diagnostics.rs (commit 545315b)
- `inspect_term(handle, field, term)` → TermReport (doc_freq par segment)
- `inspect_term_verified(...)` → + ground truth (itère stored docs)
- `inspect_sfx(handle, field, term)` → SfxTermReport (prefix_walk → parents → docs)
- `compare_postings_vs_sfxpost(handle, field, term)` → ordinal + doc count comparison
  avec détail des doc_ids manquants
- `dump_segment_keys(handle, field, n)` → term dict keys + FST probes
- `inspect_segments(handle)` → SegmentSummary
- Bench : `LUCIVY_VERIFY=1` pour ground truth, section post-mortem automatique

### Bench Linux kernel
- Dataset : `/home/luciedefraiteur/linux_bench` (git clone --depth 1 torvalds/linux)
- ~91K fichiers texte, 2GB
- Queries adaptées : mutex_lock, function, sched, printk, drivers (path)
- Protection symlinks récursifs dans `collect_files`
- `BENCH_DATASET` env var pour choisir le dataset
- Index préservé après bench (pas de cleanup)

## Résultats du bench

### 20K docs (dernier run clean) :
```
Indexation : 15s (commit_fast) + 4s (final commit)
Queries : 20 hits partout
SFX mutex : 1375 docs = ground truth exact
Mismatch : 0
```

### 90K docs (dernier run — panic) :
```
Indexation : 110s + 7s final commit
Panic au search : gapmap index out of bounds
Cause : segments sans sfx (47-140 docs, has_sfx=false)
```

## Bug ouvert : segments sans sfx (doc 03)

Certains petits segments (47-140 docs) n'ont ni .sfx ni .sfxpost.
Quand merge_sfx les fusionne, le gapmap résultant a une taille incorrecte
→ panic `index out of bounds` au search.

`has_sfx=false` → les fichiers n'ont JAMAIS été créés (pas un problème de GC).

### Hypothèses :
1. Segments produits par un merge dont le merge_sfx a échoué silencieusement
   (collector.build() Err, log::warn mais continue)
2. Cascade : un merge produit un segment sans sfx → re-mergé → le nouveau
   merge n'a pas les sfxpost des source → le problème se propage
3. Le SegmentWriter ne crée pas de SfxCollector pour certains champs

### Prochaine étape :
- Ajouter un log dans `finalize_segment` pour vérifier que les .sfx et .sfxpost
  sont bien écrits pour chaque segment
- Ajouter un log dans `merge_sfx` quand `collector.build()` échoue
- Vérifier si les segments sans sfx sont des segments mergés (produits par
  merge_sfx) ou des segments initiaux (produits par segment_writer)
- Le `segment_writer` log déjà les erreurs (ajouté cette session) mais
  aucune erreur n'a été vue → le problème vient du merge path

## Commits de la session

| Hash | Description |
|------|-------------|
| 5fc5871 | fix: contains default fuzzy distance 0 + tests |
| 12a12e9 | revert: remove deferred sfx experiment (baseline) |
| 545315b | WIP: commit_fast + observability + diagnostics |
| 51bd1b4 | fix: GC protection for segments in merge |
| fb3e8d7 | docs: bug report segments without sfx |

## Fichiers clés modifiés

| Fichier | Changements |
|---------|-------------|
| `luciole/src/reply.rs` | wait_cooperative_named + timeout warning + dump |
| `luciole/src/scheduler.rs` | ActorActivity + dump_state() |
| `src/indexer/merger.rs` | merge_sfx diagnostic warnings |
| `src/indexer/merge_state.rs` | timing instrumentation |
| `src/indexer/segment_updater_actor.rs` | commit flag, drain, track_segments |
| `src/indexer/segment_updater.rs` | gc_protected_segments, wait_cooperative |
| `src/indexer/index_writer.rs` | commit_fast, wait_cooperative |
| `src/indexer/prepared_commit.rs` | commit_fast |
| `src/indexer/segment_writer.rs` | sfxpost write diagnostics |
| `src/indexer/segment_manager.rs` | all_segment_metas() |
| `src/index/segment_reader.rs` | simplified load_sfx_files |
| `src/lib.rs` | lucivy_trace!() macro |
| `src/suffix_fst/mod.rs` | pub visibility for builder/file/gapmap |
| `lucivy_core/src/diagnostics.rs` | NEW — full diagnostic toolkit |
| `lucivy_core/src/sharded_handle.rs` | commit_fast, wait labels |
| `lucivy_core/src/query.rs` | contains distance fix |
| `lucivy_core/benches/bench_sharding.rs` | Linux kernel, commit_fast, post-mortem |
| `lucivy_core/tests/test_cold_scheduler.rs` | test_single_handle_highlights |
| `lucivy_core/tests/test_diagnostics.rs` | NEW — standalone index verification |

## Ground truth script

`/tmp/verify_ground_truth.rs` — script standalone qui itère les fichiers source
avec la même logique `collect_files` que le bench et compte les occurrences
de tokens/substrings. Utilisé pour valider les résultats du search.

Résultats pour 90K fichiers du kernel Linux :
```
mutex    (token):     8762 docs
lock     (token):     18479 docs
function (token):     16365 docs
printk   (token):     4456 docs
mutex_lock (substring): 5262 docs
```
