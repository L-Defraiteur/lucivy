# Diagnostic — Emscripten commit hang (26 avril 2026)

## Résumé

L'ingestion 4-shard en emscripten bloque au commit. Le problème n'est
PAS la mailbox (fix unbounded OK) mais le **cooperative waiting** dans
le système d'actors de luciole, incompatible avec l'environnement
emscripten/ASYNCIFY.

## Chronologie des découvertes

### 1. Mailbox bounded (résolu)
- Shard actors avaient mailbox bounded(64) → unbounded(0)
- Reader pool bounded(128) → unbounded(0)
- Router bounded(256) → unbounded(0)
- **Résultat** : les adds passent, mais le commit bloque

### 2. Commit sync ASYNCIFY bloque
- `handle.commit()` appelé directement depuis le thread ASYNCIFY
- `drain_pipeline()` fait `wait_cooperative_named(|| run_one_step())`
- Le thread ASYNCIFY a un stack limité (65536 bytes) et entre en
  conflit avec les scheduler threads pour le `ready_queue` Mutex
- **Résultat** : bloque au drain des readers

### 3. Commit dans thread spawné bloque aussi
- `std::thread::spawn` → `handle.commit()` + spin-wait AtomicU32
- Le drain des readers PASSE cette fois
- Mais `scatter(commit_shard)` envoie aux shard actors → le commit
  interne fait `writer.commit()` → `flush_indexer` qui attend les
  indexer workers via cooperative waiting
- Les indexer workers sont idle avec messages → bug de wake
- **Résultat** : bloque au scatter commit (flush_indexer waiting)

### 4. Safety net run_one_step (rewake idle actors avec messages)
- Ajouté scan des actors idle avec `has_pending()` dans `run_one_step`
- Quand la ready_queue est vide, scan + rewake les actors orphelins
- **Résultat** : les indexer workers SE RÉVEILLENT et commencent à
  traiter le flush. Mais...

### 5. `commit_direct` + bypass pipeline
- Nouvelle méthode `ShardedHandle::commit_direct()` :
  - Poll-wait que les mailboxes reader/router/shard se vident
  - Appelle `writer.commit()` directement (pas via shard actors)
- La pipeline se draine correctement via les scheduler threads
- Mais `writer.commit()` fait `finalize_segment_blocking()` qui bloque

### 6. Découverte finale : `finalize_segment_blocking` (BLOQUÉ ICI)
- `handle_flush()` dans IndexerActor appelle :
  1. `finalize_current_segment_blocking()` — synchrone, sur thread scheduler
  2. `wait_pending_finalize()` — attend finalize background via
     `rx.wait_cooperative(|| run_one_step())`
- Les 4 scheduler threads exécutent chacun un indexer actor (un par shard)
- Chaque indexer est BUSY "processing" pendant 300+ secondes
- `finalize_segment()` = SFX build + écriture segment
- Avec 505 docs par shard, ça devrait prendre quelques secondes max
- **Hypothèse** : l'I/O vers WASMFS/OPFS bloque ou est infiniment lent

## État actuel du code

### Fichiers modifiés (diag, à nettoyer)
- `luciole/src/scheduler.rs` — safety net run_one_step, thread stats,
  diag logs run_loop
- `luciole/src/pool.rs` — diag logs drain/scatter
- `luciole/src/mailbox.rs` — `mailbox_depth()`, log "NO notifier"
- `lucivy_core/src/sharded_handle.rs` — `commit_direct()`, diag logs
  commit/drain, mailboxes unbounded (reader_pool, router, shard_pool)
- `bindings/emscripten/src/lib.rs` — commit via thread + spin-wait,
  appelle `commit_direct()`
- `src/indexer/index_writer.rs` — diag logs flush_indexer
- `playground/index.html` — 4 shards, COMMIT_EVERY=2000, champ extension

### Changements fonctionnels à garder
- Mailboxes unbounded (reader, router, shard) → nécessaire
- `commit_direct()` sur ShardedHandle → contourne drain/scatter
- Safety net `run_one_step` (rewake actors idle avec messages) → fix
  générique du bug de wake
- Playground : 4 shards, extension field, batch 2000, progression /100

## Pistes pour la suite

### P1 : Investiguer pourquoi `finalize_segment` bloque
- Ajouter des logs dans `finalize_segment()` (src/indexer/index_writer.rs)
- Identifier si c'est le SFX build, l'écriture I/O, ou autre chose
- Tester si le problème est spécifique à WASMFS/OPFS (tester avec
  RamDirectory / pas d'OPFS)

### P2 : Désactiver SFX pour l'ingestion emscripten
- `commit_fast()` skip le SFX rebuild → tester si ça passe
- Si oui, faire N `commit_fast()` pendant l'ingestion + 1 `commit()`
  final avec SFX

### P3 : `finalize_segment` dans un thread dédié
- Au lieu de bloquer le scheduler thread, spawner la finalisation
  dans un `std::thread::spawn` dédié
- Les scheduler threads restent libres pour d'autres travaux

### P4 : Réduire à 1 indexer worker par shard en emscripten
- Moins de threads scheduler occupés simultanément
- Mais ne résout pas le fond du problème

### P5 : DAG batch ingestion (solution long terme)
- Un seul appel `lucivy_ingest_batch(docs_json)` 
- Parse → route → add direct (pas d'actors) → commit direct
- Zéro cooperative waiting, zéro ASYNCIFY round-trips

## Bug de wake identifié

Les actors deviennent idle avec des messages dans leur mailbox.
Le safety net dans `run_one_step` (scan + rewake) est un workaround.

La cause racine semble être un timing subtil entre le `send()` qui
fait `is_idle.swap(false)` et le scheduler qui fait `store(idle=true)`
+ `has_pending()`. Le race fix existant devrait le couvrir, mais il
y a peut-être un edge case avec emscripten threading (memory ordering
différent sur SharedArrayBuffer/Atomics ?).
claude --resume c267404f-7e44-42f7-8b4c-7717ae0b16b2  