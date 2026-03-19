# Plan — Supprimer le deferred sfx, revenir au merge_sfx complet

Date : 19 mars 2026

## Raison

Le deferred sfx (skip FST au merge, rebuild après) cause des problèmes
insolubles avec le mmap cache : le rebuild écrit le .sfx via atomic_write
mais le reader voit l'ancien contenu en cache. Résultat : les segments
mergés sont invisibles au search.

## Approche

Revenir à `merge_sfx` complet pour TOUS les merges. La perf de commit_fast
vient de ne pas drainer les merges, pas de skipper le FST.

- `commit_fast()` : flush + save_metas, pas de drain. Les merges async
  utilisent merge_sfx complet en background.
- `commit()` : drain (merges avec merge_sfx complet) + save_metas.

Les fix qu'on garde :
- `wait_cooperative` partout (fix deadlock)
- `lucivy_trace!` + observabilité luciole
- `commit_fast()` / `commit()` API
- diagnostics.rs

## Fichiers à modifier

### 1. merge_state.rs
- Supprimer `use_deferred_sfx` flag
- `step_sfx()` appelle toujours `merge_sfx` (pas deferred)

### 2. segment_updater_actor.rs
- Supprimer `rebuild_deferred_sfx()` entièrement
- `handle_commit` : drain seulement (plus de rebuild)
- Garder le `!msg.rebuild_sfx` check pour ne pas relancer de merges après commit()
- Supprimer les imports inutiles (SuffixFstBuilder, SfxFileWriter, etc.)

### 3. merger.rs
- Garder `merge_sfx_deferred()` comme code mort (on pourra le réactiver
  si on résout le mmap cache plus tard)
- Ou le supprimer si on veut un code propre
- Supprimer les eprintln de debug dans merge_sfx

### 4. segment_updater_actor.rs (drain)
- Supprimer `state.use_deferred_sfx = false` dans drain_all_merges
  (plus nécessaire)

### 5. segment_reader.rs
- `load_sfx_files` : chargement simple (déjà fait, on avait retiré le skip)
- Supprimer `rebuild_sfx_inline` (code mort)

### 6. segment_manager.rs
- Garder `all_segment_metas()` (utile pour les diagnostics)

### 7. suffix_fst/file.rs
- Garder le handle FST vide dans SfxFileReader (safety)

### 8. Queries (automaton_phrase_weight, regex_continuation, suffix_contains_query)
- Garder le EmptyScorer fallback quand sfx_file() retourne None (safety)
- Mais en pratique ça ne devrait plus arriver

## Ce qu'on NE touche PAS

- luciole/reply.rs (wait_cooperative_named — on garde)
- luciole/scheduler.rs (ActorActivity, dump_state — on garde)
- index_writer.rs (commit_fast, wait_cooperative — on garde)
- prepared_commit.rs (commit_fast — on garde)
- sharded_handle.rs (commit_fast, labels — on garde)
- segment_updater.rs (wait_cooperative — on garde)
- diagnostics.rs (on garde tout)
- lib.rs (lucivy_trace — on garde)
