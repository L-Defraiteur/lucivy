# Rapport session 3-9 mai 2026

## Objectif

Eliminer les deadlocks du commit shardé en WASM (emscripten) de manière
structurelle — pas de scotch, architecturalement invulnérable.

## Ce qui a été fait

### 1. WaitGraph (luciole/src/wait_graph.rs) — NOUVEAU

Tracking automatique de toutes les dépendances inter-thread dans luciole.
Chaque `scheduler.wait()`, `wait_cooperative`, `Suspend`, `pipe_to`,
`collect_replies_to` enregistre un edge dans un graph global.

- `dump_mermaid()` / `dump_text()` à tout moment
- RAII via `WaitGuard` — cleanup garanti
- C FFI : `lucivy_dump_wait_graph()` / `lucivy_dump_wait_graph_text()`
- Suspend tracking dans le scheduler (edge créé au Suspend, nettoyé au resume)

### 2. pipe_to / collect_to / task_pipe_to — NOUVEAU

Pattern request-reply déclaratif : "j'envoie cette tâche à tel acteur,
rappelle-moi quand c'est fait." Le résultat arrive comme un message FIFO.

- `ActorRef::pipe_to(msg, target, label, map)` — 1 requête → 1 message retour
- `Pool::collect_to(msg, target, label, map)` — N requêtes → 1 message retour
- `Scheduler::task_pipe_to(task, target, label, map)` — tâche CPU → message retour
- `collect_replies_to(rxs, target, label, map)` — free function pour receivers existants

**Invariant** : callback posé AVANT envoi → pas de race condition.

**on_send dans Inner\<T\>** : callback appelé par Reply::send, protégé par
le même Mutex que value → race-free même si la reply arrive avant set_pipe.

**Cleanup** : `pipe_edge_id` dans Inner (AtomicU64) nettoyé par Reply::send
(normal) ou Reply::drop (sender died). CollectState::Drop pour les N:1.

### 3. ShardActor migré — collect_to + task_pipe_to

Avant (Suspend + poll_idle, ~50 lignes) :
```
handler(Commit) → flush_workers → JoinResume → set_resume × N → Suspend
poll_idle → collect → finalize → commit (blocking!) → reply
```

Après (4 étapes non-bloquantes) :
```
handler(Commit) → drain_workers → collect_replies_to → IndexersDrained
handler(IndexersDrained) → flush_workers → collect_replies_to → FlushDone
handler(FlushDone) → task_pipe_to(finalize + commit DAG) → CommitDone
handler(CommitDone) → reply.send
```

Supprimé : `PendingShardCommitState`, `pending_commit`, tout le `poll_idle`.

### 4. execute_dag_async — NOUVEAU

`DagExecutor` : acteur éphémère qui pilote un DAG niveau par niveau via
`collect_replies_to`. Parallélisme réel (submit_task par node), aucun thread
capturé.

```rust
execute_dag_async(dag, &target, "label", |result| Msg::DagDone(result));
```

`execute_dag` synchrone reste inchangé (backward compat).

### 5. Enforcement

`cooperative_wait inside actor handler` → **panic** (plus un warning).

### 6. IndexerActor — finalize en background + yield périodique

- `submit_finalize_task()` : finalize_segment tourne sur un task thread
  (submit_task), pas inline dans le handler
- `handle_flush()` retourne les receivers au lieu de bloquer. Le FlushMsg
  handler utilise `collect_replies_to` sur self_ref → `IndexerFinalizeCompleteMsg`
  envoie la reply quand tous les finalizes sont terminés
- `YIELD_EVERY_N_DOCS = 64` : l'indexer yield périodiquement pour ne pas
  monopoliser un scheduler thread
- GenericActor a déjà `on_start` qui stocke `self_ref: ActorRef<Envelope>`
  dans l'ActorState

### 7. drain_workers sur IndexWriter

`IndexWriter::drain_workers()` envoie un DrainMsg aux indexers. Quand
tous répondent (FIFO), on sait que tous les DocsMsg ont été traités.
Utilisé par ShardActor avant flush_workers() pour garantir l'ordre.

Messages ajoutés : `IndexerDrainMsg`, `IndexerDrainReply`.

### 8. Cleanup

- Tous les `eprintln!("[commit]...")`, `[scatter]`, `[diag]` supprimés
  dans sharded_handle.rs et pool.rs
- `min_num_segments` remis à 8 (standard)

## Commits sur feature/unified-sharded-handle

```
19e012f feat(luciole): JoinResume, scheduler.wait() hybride, ShardActor non-blocking commit
7626c1b feat(luciole): pipe_to, collect_to, WaitGraph, execute_dag_async — deadlock-free by design
fdf16a1 fix: drain indexers before flush in ShardActor commit chain
cf0cb99 fix: indexer finalize_segment runs on task thread, not in handler
71e4beb fix: indexer yields every 64 docs to prevent scheduler starvation
```

## Tests

- **154 tests luciole** (151 + 3 async_dag) — tous passent
- **1200 tests lib** — passent (9 pre-existing failures de merge/GC)
- **9 tests sharded** — passent
- **Playground rag3db** (4308 docs) — **ingestion complète, zero warnings**

## Etat actuel — le bug non résolu

### Symptôme

L'ingestion du Linux kernel (75K fichiers) bloque au commit des 2000
premiers documents. Le WaitGraph montre :

```
shard_35 --[shard_2_drain (0/1)]--> waiting (141s)
shard_34 --[shard_1_drain (0/1)]--> waiting (141s)
```

2 indexers sont TAKEN q:0 pendant 141+ secondes. Pas d'OOM, pas de panic,
pas de crash. Les 2 autres scheduler threads sont IDLE.

### Ce qu'on sait

- Le drain attend que les indexers traitent tous leurs DocsMsg (FIFO)
- Les indexers sont dans un handler (TAKEN) avec mailbox vide (q:0)
- q:0 est trompeur quand l'actor est TAKEN (`unwrap_or(0)` dans le dump)
- Le YIELD_EVERY_N_DOCS=64 est en place mais les indexers restent TAKEN
- Le finalize est background (submit_task) — pas inline
- Le problème existait AVANT nos changements (même pattern avec
  l'ancien code) mais passait au commit de rag3db (4308 petits fichiers)
- Avec rag3db (4308 docs), tout passe en <30s, zero warnings
- Avec Linux kernel (75K docs), ça bloque au premier commit (2000 docs)

### Hypothèses à investiguer

1. **add_document lent sur gros fichiers** — les .c du kernel sont gros
   (10K+ lignes). Chaque add_document fait tokenization + postings + SFX
   collector. 500 × 60ms = 30s. Mais 141s c'est trop.

2. **Finalize task jamais exécuté** — submit_finalize_task met un task dans
   la ready queue. Si le yield fonctionne, le thread devrait le dispatcher.
   Mais peut-être que le yield ne sort pas du batch correctement ?

3. **OPFS I/O bloquant** — WASMFS + OPFS peut faire des écritures synchrones
   qui bloquent le thread. Si finalize_segment écrit sur OPFS et que ça
   bloque, le task thread est capturé indéfiniment.

4. **Interaction ancien index OPFS** — au reload de la page, l'ancien index
   est chargé depuis OPFS, créant des actors. Le nouvel index crée un second
   jeu d'actors. Les deux partagent le scheduler global. Un actor de l'ancien
   index pourrait capturer un thread.

### Ce qu'il faut pour diagnostiquer

Le problème c'est qu'on ne voit pas CE QUE FAIT l'indexer pendant qu'il
est TAKEN. On voit "TAKEN q:0" mais pas "en train de faire add_document
sur quel fichier" ou "en train de finalize quel segment".

**Il faudrait** :
- Un "activity label" sur le ThreadInfo : `set_activity("add_doc file.c")`
  avant chaque add_document, `clear_activity()` après
- Ou un log dans le ring buffer à chaque handle_docs avec le nom du fichier
- Ou un timer : si un handle_docs prend plus de 5s, log automatiquement

## Comment lancer le playground

```bash
# Terminal 1 : serveur
cd packages/rag3db/extension/lucivy/ld-lucivy/playground
node serve.mjs
# → http://localhost:9877

# Terminal 2 : build
cd packages/rag3db/extension/lucivy/ld-lucivy
bash bindings/emscripten/build.sh

# Reload page
curl -s http://localhost:9877/eval/main -d '{"js":"if(window._lucivy)window._lucivy._worker.terminate(); location.href=location.origin+\"/?v=\"+Date.now(); \"ok\""}'

# Clear logs
echo "" > playground/diag.log

# Lancer ingestion rag3db (FONCTIONNE — 4308 docs, zero warnings)
curl -s http://localhost:9877/eval/main -d '{"js":"document.getElementById(\"gitUrl\").value = \"https://github.com/L-Defraiteur/rag3db\"; cloneGitRepo(); \"started\""}'

# Lancer ingestion Linux kernel (BLOQUE au commit des 2000)
curl -s http://localhost:9877/eval/main -d '{"js":"document.getElementById(\"gitUrl\").value = \"https://github.com/torvalds/linux\"; cloneGitRepo(); \"started\""}'

# Check status
curl -s http://localhost:9877/eval/main -d '{"js":"document.getElementById(\"status\").textContent"}'

# Logs
tail -f playground/diag.log
grep "WARNING" playground/diag.log -A 15

# Dump wait graph (depuis le main thread — toujours dispo)
curl -s http://localhost:9877/eval/main -d '{"js":"(async()=>await Module.ccall(\"lucivy_dump_wait_graph_text\",\"string\",[],[],{async:true}))()"}'
# Note: Module n'est pas dans le main thread. Utiliser le worker :
curl -s http://localhost:9877/eval -d '{"js":"(async()=>await Module.ccall(\"lucivy_dump_wait_graph_text\",\"string\",[],[],{async:true}))()"}'
# (timeout si le worker est bloqué)
```

## Fonctions de diag exportées (C FFI)

| Fonction | Description |
|----------|-------------|
| `lucivy_dump_mermaid()` | Graph mermaid threads + actors |
| `lucivy_dump_state()` | Dump texte actors + queue |
| `lucivy_dump_wait_graph()` | WaitGraph mermaid (qui attend quoi) |
| `lucivy_dump_wait_graph_text()` | WaitGraph texte |
| `lucivy_test_condvar()` | Test condvar entre threads |
| `lucivy_test_coop()` | Test cooperative wait |

## Fichiers clés modifiés

| Fichier | Changement |
|---------|-----------|
| `luciole/src/wait_graph.rs` | NOUVEAU — WaitGraph global |
| `luciole/src/reply.rs` | on_send, pipe_edge_id, set_pipe, collect_replies_to |
| `luciole/src/mailbox.rs` | ActorRef::pipe_to |
| `luciole/src/pool.rs` | Pool::collect_to, cleanup scatter eprintln |
| `luciole/src/scheduler.rs` | task_pipe_to, dump_wait_graph, Suspend tracking |
| `luciole/src/runtime.rs` | DagExecutor, execute_dag_async |
| `luciole/src/lib.rs` | Exports |
| `lucivy_core/src/sharded_handle.rs` | ShardActor migré, drain_pipeline nettoyé |
| `src/indexer/index_writer.rs` | drain_workers() |
| `src/indexer/indexer_actor.rs` | submit_finalize_task, collect_replies_to flush, YIELD_EVERY_N_DOCS, IndexerDrainMsg |
| `src/indexer/log_merge_policy.rs` | min_num_segments = 8 |
| `bindings/emscripten/src/lib.rs` | dump_wait_graph exports |
| `bindings/emscripten/build.sh` | dump_wait_graph exports |

## Docs de design (dans docs/3-mai-2026-15h00/)

| Fichier | Contenu |
|---------|---------|
| 05-design-pipe-to.md | Design pipe_to/collect_to, acteurs vs tâches |
| 06-plan-action-pipe-to.md | Plan d'action en 4 phases |
| 07-architecture-pipe-to.md + .excalidraw | Diagrammes avant/après |
| 08-design-execute-dag-async.md | Design DagExecutor |

## Prochaine session — plan d'action

1. **Diagnostiquer le hang Linux kernel** — ajouter un activity label
   aux threads (set_activity dans ThreadInfo) pour voir ce que fait
   l'indexer quand il est TAKEN pendant 141s

2. **Vérifier que le yield fonctionne** — ajouter un log au moment du
   yield pour confirmer que les indexers yield bien tous les 64 docs

3. **Vérifier que le finalize task s'exécute** — log quand le task
   est dispatché et quand il complète

4. **Tester avec COMMIT_EVERY=500** — réduire la taille des batches
   pour voir si ça passe (confirme que c'est un problème de volume)

5. **Si c'est OPFS I/O** — le finalize écrit les segments sur le
   filesystem WASMFS/OPFS. Si l'écriture est synchrone et lente, le
   task thread est capturé. Solution : utiliser RamDirectory au lieu
   de FsDirectory pour le WASM emscripten (les données persistent
   via snapshot/delta, pas via OPFS direct)
