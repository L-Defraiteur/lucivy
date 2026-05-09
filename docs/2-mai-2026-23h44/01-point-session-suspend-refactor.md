# Point de session — 2 mai 2026, 23h44

## Contexte

Session focalisée sur le deadlock cooperative waiting en emscripten.
Le ShardedHandle (4 shards) bloque indéfiniment au commit dans le
playground WASM.

## Cause racine identifiée

Le problème est **structurel** dans l'interaction luciole ↔ lucivy :

1. `handle_batch(BATCH_SIZE=1024)` capture un scheduler thread pour
   traiter jusqu'à 1024 messages d'un actor
2. L'indexer, pendant le traitement d'un doc, atteint le budget mémoire
   (~13MB par doc avec SFX, budget 50MB → tous les 3-4 docs)
3. `finalize_current_segment_background()` envoie un `FinalizeMsg` au
   finalizer actor et fait `wait_cooperative(|| run_one_step())`
4. Le thread est **coincé dans la call stack du handler** — il ne peut
   pas rendre la main au scheduler
5. Avec 4 shards × 1 indexer = 4 threads capturés → 0 threads libres
   → deadlock

## Ce qui a été fait

### Commit c037d41

- **Helper thread** (luciole) : thread persistant qui pompe
  `run_one_step_impl` en boucle. Parké sur le condvar avec timeout 5ms.
  Quand tous les threads réguliers sont en cooperative wait, le helper
  est le seul parké → `notify_one()` le cible directement.

- **commit_direct()** (lucivy_core) : poll-drain la pipeline puis
  `writer.commit()` directement par shard (séquentiel). Évite la
  pipeline actor pour le commit.

- **Diag infrastructure** : serve.mjs endpoints `/log`, `/eval`,
  `/eval/main`. Worker hooks `printErr`, eval pollers. Permettent
  `tail -f diag.log` et `curl localhost:9877/eval` depuis le terminal.

- **Playground shardé** : index 4 shards, champ extension, batch 2000.

### Résultat du test

- Premier run : bloqué 300s+ (probablement vieux WASM en cache)
- Deuxième run : **succès** — 4 shards, 4308 docs indexés et recherchés
- Mais pas de certitude que c'est robuste (potentiellement intermittent)

## Analyse : pourquoi le helper ne suffit pas

Le helper résout le cas "tous les threads en cooperative wait". Mais il
ne résout PAS le cas où un thread est capturé dans `handle_batch` sans
faire de cooperative wait (finalize_segment bloquant, mutex emscripten,
allocateur...).

Le vrai problème : **un handler ne devrait jamais bloquer un thread du
scheduler pour attendre un autre actor**. C'est une violation du contrat
implicite d'un actor system à thread pool fixe.

## Plan : ActorStatus::Suspend

### Le concept

Ajouter `Suspend(ResumeHandle)` à `ActorStatus`. Quand un handler
retourne `Suspend` :

1. Le scheduler **libère le thread** immédiatement
2. L'actor est parké (pas dans la ready_queue)
3. Quand le `ResumeHandle` fire → actor replanifié
4. Le prochain message est traité normalement

### Cas d'usage dans l'indexer

**handle_docs (budget mémoire atteint)** :
```
add_document → mem >= budget → send FinalizeMsg → return Suspend(rx.resume_handle())
// Thread libéré. Finalizer tourne sur un thread libre.
// Quand finalizer répond → resume → indexer replanifié → prochain doc.
```

**handle_flush (pending finalize)** :
```
finalize_current_segment_blocking() → OK
pending_finalize exists? → store flush reply → return Suspend(rx.resume_handle())
// Quand finalize fini → resume → poll_idle envoie la flush reply.
```

### Fichiers à modifier

| Fichier | Changement |
|---------|------------|
| `luciole/src/lib.rs` | `Suspend(ResumeHandle)` dans ActorStatus |
| `luciole/src/reply.rs` | `ResumeHandle`, callback dans `Reply::send` |
| `luciole/src/scheduler.rs` | handle Suspend, park actor, resume |
| `src/indexer/indexer_actor.rs` | state machine handle_docs/handle_flush |
| Tests luciole | test suspend/resume basique |

### Propriétés garanties

- **Zéro thread bloqué** dans un handler qui attend un autre actor
- **Additif** : actors existants (Continue/Stop/Yield) inchangés
- **Structurellement compatible WASM** : pas de cooperative wait
- **Performance** : identique (le thread est libre au lieu de spinner)

## Diag temporaires à nettoyer après Suspend

- `src/indexer/index_writer.rs` : eprintln finalize_segment, flush_indexer
- `src/indexer/indexer_actor.rs` : eprintln handle_docs counter, handle_flush
- `luciole/src/pool.rs` : eprintln scatter, drain
- `luciole/src/scheduler.rs` : eprintln thread started, iterations
- `lucivy_core/src/sharded_handle.rs` : eprintln drain_pipeline, commit
- `luciole/src/mailbox.rs` : eprintln NO notifier
