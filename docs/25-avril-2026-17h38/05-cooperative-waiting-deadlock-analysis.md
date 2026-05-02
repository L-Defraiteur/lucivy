# Analyse — Deadlock cooperative waiting (2 mai 2026)

## Le problème

Le commit ShardedHandle en emscripten bloque indéfiniment.
`writer.commit()` fait `flush_indexer` → les 4 indexer workers (un par shard)
sont BUSY "processing" pendant 200+ secondes sans terminer.

## Cause racine identifiée

`handle_batch` traite jusqu'à BATCH_SIZE=1024 messages d'un coup. Quand
un indexer worker reçoit ~500 `IndexerDocsMsg` + 1 `IndexerFlushMsg`, il
les traite tous dans UN batch. Pendant le traitement des docs :

1. L'indexer accumule des tokens en mémoire (SfxCollector)
2. Quand le budget mémoire est atteint, il fait `finalize_current_segment_background()`
3. Ce background finalize **envoie** un message au `FinalizerActor`
4. Puis `wait_pending_finalize()` fait du **cooperative waiting** (`run_one_step()`)

Le problème : les **4 scheduler threads** sont TOUS occupés à traiter un
indexer actor chacun. Quand les 4 font du cooperative waiting simultanément :

```
Thread 0: indexer_0 → wait_pending_finalize → run_one_step() → ???
Thread 1: indexer_1 → wait_pending_finalize → run_one_step() → ???
Thread 2: indexer_2 → wait_pending_finalize → run_one_step() → ???
Thread 3: indexer_3 → wait_pending_finalize → run_one_step() → ???
```

Chaque `run_one_step()` essaie de pop un finalizer de la ready_queue. Les
finalizers Y SONT (poussés par `SchedulerNotifier::wake()`). MAIS :
- `notify_one()` dans `wake()` n'a personne à réveiller (tous busy)
- Les threads busy DEVRAIENT pop via `run_one_step()`, mais ils sont
  coincés dans leur propre cooperative wait (récursion)

## Ce qui marche vs ce qui bloque

| Étape | Statut | Mécanisme |
|-------|--------|-----------|
| Pipeline drain (readers/router/shards) | OK | `commit_direct` poll-wait sur mailbox depths |
| Shard 0 commit | OK (parfois) | Si son indexer a 0 docs → skip |
| Shard N commit avec docs | **BLOQUE** | `writer.commit()` → `flush_indexer` → cooperative wait |

## Pistes de solution (non implémentées)

### P1 : Réduire BATCH_SIZE (quick win)
```rust
#[cfg(target_os = "emscripten")]
const BATCH_SIZE: usize = 1;
```
Chaque `handle_batch` traite 1 seul message → retourne au scheduler →
le scheduler pop le prochain item (finalizer ou autre). Les 4 threads
ne sont jamais tous coincés en cooperative wait simultanément.

**Inconvénient** : perf d'ingestion dégradée (overhead scheduler per-doc).

### P2 : Cooperative wait spawne un helper thread
Chaque `wait_cooperative_named` spawne un thread temporaire qui pompe
`run_one_step()`. Garantit qu'il y a toujours un thread libre.

**Inconvénient** : `std::thread::spawn` en emscripten utilise le pthread
pool (PTHREAD_POOL_SIZE=8). Risque d'exhaustion si trop de waits
simultanés. Les threads spawnés ont aussi des problèmes avec condvar
timeouts en emscripten.

### P3 : `spawn_pinned` pour IndexerActor
Les indexer workers tournent sur leur propre thread dédié (pas le scheduler).
Leur cooperative waiting ne bloque pas un scheduler thread.

**Inconvénient** : 4 shards × 1 indexer = 4 threads supplémentaires.
PTHREAD_POOL_SIZE=8 déjà utilisé par 4 scheduler + autres.

### P4 : Ne pas utiliser d'actors pour le commit
`commit_direct` bypass la pipeline sharded, mais `writer.commit()`
utilise toujours les actors internes (indexer/finalizer). Refactorer
`IndexWriter::commit()` pour un mode direct (pas d'actors) en WASM.

**Inconvénient** : refactor significatif de IndexWriter.

### P5 : Continuations au lieu de cooperative waiting
L'indexer retourne `ActorStatus::Yield` au lieu de bloquer. Le scheduler
le re-schedule quand le finalizer a fini.

**Inconvénient** : refactor massif, change le modèle d'exécution.

### P6 : Commit séquentiel avec BATCH_SIZE=1
Combiner P1 + `commit_direct` (séquentiel par shard). Avec BATCH_SIZE=1,
le scheduler alterne entre indexers et finalizers. Un seul shard committe
à la fois (séquentiel dans `commit_direct`).

**Avantage** : fix minimal, pas de refactor.

## Infrastructure diag mise en place

### Diag server (serve.mjs)
- `POST /log` → append `playground/diag.log` (gitignored)
- `POST /eval` → exécute JS dans le **worker** (accès Module/WASM)
- `POST /eval/main` → exécute JS dans le **main thread** (accès DOM)
- `GET /eval/poll`, `GET /eval/main/poll` → workers et main thread pollent

### Hooks
- `Module.printErr` intercepté → `diagSendLog()` → `POST /log`
- `console.error` monkey-patché dans le worker
- Eval poller dans worker (500ms) et main thread (500ms)

### Usage CLI
```bash
# Logs en temps réel
tail -f playground/diag.log

# Exécuter du JS dans le worker
curl -s localhost:9877/eval -d '{"js":"Module._lucivy_num_docs(0)"}'

# Exécuter du JS dans la page
curl -s localhost:9877/eval/main -d '{"js":"document.title"}'

# Lancer le clone GitHub
curl -s localhost:9877/eval/main -d '{"js":"document.getElementById(\"gitUrl\").value=\"https://github.com/L-Defraiteur/rag3db\"; cloneGitRepo(); \"ok\""}'
```

## Logs diag dans le code (à nettoyer après fix)

- `luciole/src/scheduler.rs` : thread stats, run_loop iterations, safety net
- `luciole/src/pool.rs` : drain/scatter mailbox depths
- `luciole/src/mailbox.rs` : `mailbox_depth()`, "NO notifier" warning
- `lucivy_core/src/sharded_handle.rs` : `commit_direct()`, drain progress
- `src/indexer/index_writer.rs` : `finalize_segment` timing, flush_indexer
- `src/indexer/indexer_actor.rs` : `handle_flush` state
- `bindings/emscripten/src/lib.rs` : search segment count

## Fichiers modifiés non committés

- `playground/serve.mjs` — diag endpoints (/log, /eval, /eval/main)
- `playground/js/lucivy-worker.js` — diagSendLog, eval poller, printErr hook
- `playground/index.html` — eval/main poller, drainMerges
- `playground/js/lucivy.js` — drainMerges method
- `.gitignore` — playground/diag.log
- `luciole/src/scheduler.rs` — safety net, thread stats, diag logs
- `luciole/src/pool.rs` — drain/scatter diag logs
- `luciole/src/mailbox.rs` — mailbox_depth()
- `lucivy_core/src/sharded_handle.rs` — commit_direct()
- `bindings/emscripten/src/lib.rs` — sync commit_direct, drain_merges
- `bindings/emscripten/build.sh` — export drain_merges
- `src/indexer/index_writer.rs` — finalize_segment diag
- `src/indexer/indexer_actor.rs` — handle_flush diag
