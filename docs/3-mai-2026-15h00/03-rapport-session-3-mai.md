# Rapport session 3 mai 2026

## Résumé

Objectif : éliminer les deadlocks du commit shardé en WASM (emscripten).

On a fait du progrès structurel (JoinResume, scheduler.wait hybride, ShardActor
refactoré, merges déférées, shard drain) mais on tourne en rond sur le diag
car on n'a pas de visibilité sur ce qui se passe DANS les handlers. Chaque
fix révèle un nouveau blocage qu'on met 30min à diagnostiquer.

**Verdict** : l'architecture de diag doit être repensée AVANT de continuer à
fixer des bugs à l'aveugle.

## Ce qui fonctionne

- **JoinResume** : N:1 resume, testé, validé
- **scheduler.wait()** : hybride (external → wait_blocking, scheduler → cooperative)
- **ShardActor** : flush_workers non-bloquant → JoinResume → Suspend → poll_idle finalize
- **Condvar emscripten** : testé via `lucivy_test_condvar`, fonctionne parfaitement
- **drain_pipeline** : draine readers → router → shards (ajout du shard drain)
- **Merges déférées** : handle_commit ne fait plus de merge inline
- **Indexer Yield** : rend le thread après finalize_current_segment_blocking
- Les 1209 tests lib passent (sauf 13 liés à min_num_segments=2, pas notre code)

## Où ça bloque (état actuel)

Au commit de 2000 docs (4 shards, ~500/shard). Le scatter envoie CommitMsg
aux 4 shards. 3 shards passent en <1s. Le 4ème bloque.

L'indexer du 4ème shard est TAKEN (pris par un scheduler thread) pendant 5+
minutes. La mailbox est vide (q:0). Aucun log de FlushMsg dispatch, aucun log
de handle_flush ou finalize. Les 3 autres scheduler threads sont IDLE.

Hypothèse : l'indexer est dans un handler DocsMsg qui fait un `add_document`
qui bloque sur un write I/O interne, OU l'indexer est dans le handle de la
session PRÉCÉDENTE (OPFS reload crée des actors de l'ancien index qui
interfèrent).

## Le vrai problème : on diagnostique à l'aveugle

On n'a AUCUNE visibilité sur :
1. La chaîne de dépendances entre threads/actors
2. Ce qu'un handler fait en interne (quel sous-appel bloque)
3. Les waits imbriqués et leurs dépendances
4. Le lien entre "actor X attend reply de actor Y"

Chaque ajout de log nécessite un rebuild emscripten (~3min) + reload playground
+ attente ingestion (~1min) + attente blocage (~1min). Cycle de 5min minimum
par hypothèse testée.

## Comment lancer le playground

```bash
# Terminal 1 : serveur
cd packages/rag3db/extension/lucivy/ld-lucivy/playground
node serve.mjs
# → http://localhost:9877

# Terminal 2 : build
cd packages/rag3db/extension/lucivy/ld-lucivy
bash bindings/emscripten/build.sh

# Reload page (via eval)
curl -s http://localhost:9877/eval/main -d '{"js":"if(window._lucivy)window._lucivy._worker.terminate(); location.href=location.origin+\"/?v=\"+Date.now(); \"ok\""}'

# Clear diag log
echo "" > playground/diag.log

# Lancer ingestion rag3db
curl -s http://localhost:9877/eval/main -d '{"js":"document.getElementById(\"gitUrl\").value = \"https://github.com/L-Defraiteur/rag3db\"; cloneGitRepo(); \"started\""}'

# Check status
curl -s http://localhost:9877/eval/main -d '{"js":"document.getElementById(\"status\").textContent"}'

# Eval dans le worker (quand il n'est pas bloqué)
curl -s http://localhost:9877/eval -d '{"js":"1+1"}'

# Eval dans le main thread (toujours dispo)
curl -s http://localhost:9877/eval/main -d '{"js":"1+1"}'

# Test condvar (doit retourner "ok: 42")
curl -s http://localhost:9877/eval -d '{"js":"(async()=>await Module.ccall(\"lucivy_test_condvar\",\"string\",[],[],{async:true}))()"}'

# Logs
tail -f playground/diag.log
grep "[commit]\|[luciole]\|[shard_]\|[finalize]" playground/diag.log
```

## Fonctions de diag exportées (C FFI)

| Fonction | Description |
|----------|-------------|
| `lucivy_dump_mermaid()` | Graph mermaid threads + actors |
| `lucivy_dump_state()` | Dump texte actors + queue |
| `lucivy_test_condvar()` | Test condvar entre threads |
| `lucivy_test_coop()` | Test cooperative wait |

## Fichiers modifiés cette session

| Fichier | Changement |
|---------|-----------|
| `luciole/src/reply.rs` | JoinResume, wait_blocking_with_diag |
| `luciole/src/lib.rs` | Export JoinResume |
| `luciole/src/scheduler.rs` | scheduler.wait() hybride |
| `luciole/src/pool.rs` | scatter/drain/shutdown → scheduler.wait() |
| `luciole/src/mailbox.rs` | request → scheduler.wait() |
| `luciole/src/envelope.rs` | TypedActorRef → scheduler.wait() |
| `luciole/src/async_executor.rs` | FutureHandle → scheduler.wait() |
| `luciole/src/runtime.rs` | execute_dag → scheduler.wait() |
| `luciole/src/generic_actor.rs` | dispatch log (type_tag) |
| `src/indexer/index_writer.rs` | flush_workers(), finalize_flush_and_prepare(), diag logs |
| `src/indexer/indexer_actor.rs` | handle_docs→bool (Yield), diag logs |
| `src/indexer/segment_updater.rs` | pending_merge_tasks AtomicBool, wait_merging_thread |
| `src/indexer/segment_updater_actor.rs` | merges déférées, run_deferred_merges |
| `lucivy_core/src/sharded_handle.rs` | ShardActor JoinResume+Suspend, drain_pipeline+shards, diag logs |
| `bindings/emscripten/src/lib.rs` | lucivy_test_condvar, lucivy_test_coop, diag logs |
| `bindings/emscripten/build.sh` | exports test functions |
