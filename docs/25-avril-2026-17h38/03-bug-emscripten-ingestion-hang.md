# Bug — Ingestion emscripten bloque de façon intermittente

## Symptôme

L'indexation de fichiers via le playground (git clone) bloque après un
nombre variable de documents (1600, 3000, etc.). Intermittent.

Le dernier log visible : `[callStr] calling lucivy_add` — pas de retour.
Parfois c'est `lucivy_add` qui bloque, parfois le `lucivy_commit`.

## Contexte

Avant la migration ShardedHandle :
- `lucivy_add` → `writer.lock().add_document(doc)` (direct, pas d'actors)
- `lucivy_commit_async` → `std::thread::spawn` → `writer.commit()` (thread dédié)
- Fonctionnait sans blocage

Après la migration ShardedHandle :
- `lucivy_add` → `handle.add_document(doc, node_id)` → fast path pour
  1 shard → `route_and_send(doc, node_id, hashes)` → `shard_pool.send(ShardMsg::Insert)`
- `lucivy_commit` (sync via ASYNCIFY) → `handle.commit()` → `drain_pipeline()`
  → `shard_pool.worker(0).request(ShardMsg::Commit)`

## Hypothèses

### H1 : Mailbox shard actor pleine (pour add)

Le shard actor a un mailbox borné de **64 messages**. L'add via le fast
path envoie `ShardMsg::Insert` au shard actor. Si l'actor ne consomme pas
assez vite (parce qu'il est sur un thread scheduler occupé par autre chose),
le `send()` bloque en attendant de la place.

En emscripten avec ASYNCIFY, le blocking d'un `flume::send()` est
problématique — ASYNCIFY a une stack limitée (65536 bytes) et le unwinding
pourrait échouer silencieusement.

**Mais** : le fast path pour 1 shard bypass la pipeline. Le shard actor
reçoit juste des Insert. Chaque Insert est rapide (write au SegmentWriter
en mémoire, pas d'I/O). Le scheduler devrait consommer à la vitesse
d'envoi. Sauf si le thread scheduler est bloqué sur autre chose.

### H2 : Commit bloque les actors (pour commit)

`handle.commit()` fait `drain_pipeline()` qui envoie `DrainMsg` aux
stages du pipeline (readers, router, shards). Le drain attend que chaque
stage ait traité tous ses messages en attente.

Le drain utilise `wait_cooperative_named(|| scheduler.run_one_step())` :
le thread appelant (worker JS via ASYNCIFY) pompe le scheduler en attendant.

**Problème possible** : `run_one_step()` depuis un thread non-scheduler
(le worker ASYNCIFY) pourrait entrer en conflit avec les vrais threads
scheduler. Le `shared.ready_queue.lock()` est un Mutex — si un thread
scheduler le tient, le worker attend. Si tous les threads scheduler
attendent aussi (deadlock circulaire), tout bloque.

### H3 : ASYNCIFY + threading = interactions subtiles

Chaque `ccall` avec `{async: true}` passe par ASYNCIFY. ASYNCIFY
"suspend" le stack WASM et reprend plus tard. Avec des milliers d'appels
rapides (`lucivy_add` × 4308), les rewinds/unwinds ASYNCIFY pourraient
accumuler de la pression ou interférer avec les pthreads.

### H4 : Le drain_pipeline est le vrai problème

L'ancien code ne faisait **pas** de drain. Il appelait `writer.commit()`
directement. Le ShardedHandle fait un `drain_pipeline()` qui est conçu
pour le cas multi-shard où des messages sont en vol dans la pipeline.

Pour le cas 1-shard avec le fast path (qui bypass la pipeline), le drain
est théoriquement un no-op (readers et router n'ont rien). Mais le drain
envoie quand même des `DrainMsg` aux readers et router, et attend les
acks. Si un de ces actors est bloqué...

## Pistes de solution

### S1 : Bypass les actors pour l'ingestion emscripten

Pour le binding emscripten, appeler `shard(0).writer.lock().add_document()`
directement comme avant. Pas de fast path actor. C'est un retour arrière
mais ça marchait.

**Con** : duplique la logique, pas cohérent avec les autres bindings.

### S2 : Augmenter la capacité du mailbox

Passer de 64 à 4096 ou plus. Ça repousse le problème mais ne le résout pas
si le vrai blocage est ailleurs (scheduler, ASYNCIFY).

### S3 : DAG d'ingestion batch

Comme discuté : un seul appel `lucivy_ingest_batch(docs_json)` qui fait
tout dans un DAG exécuté par le scheduler :

```
parse_docs → add_to_shard_0..N → commit → reload
```

Avantages :
- Pas de milliers de ccalls ASYNCIFY
- Le DAG tourne entièrement dans le scheduler (pas de thread externe)
- Le commit est un node du DAG (pas de cooperative waiting externe)

**Con** : plus de travail, change l'API.

### S4 : Investiguer le blocage exact

Ajouter des logs granulaires pour identifier OÙ exactement ça bloque :
- Avant/après `shard_pool.send()` dans `route_and_send`
- Avant/après `drain_pipeline()` dans `commit()`
- Avant/après chaque stage du drain

## Fix appliqué (25 avril)

**Mailbox unbounded** : changé le shard actor mailbox de `bounded(64)` à
`unbounded()` (capacity=0 dans luciole). Modifié `mailbox()` dans
`luciole/src/mailbox.rs` pour supporter capacity=0 → `channel::unbounded()`.

Justification : le backpressure n'a pas de sens pour l'ingestion interne.
Chaque `ShardMsg::Insert` est petit (~1KB) et consommé quasi-immédiatement.
Avec 4308 docs × 1KB = ~4MB au pire en vol — négligeable.

**À retester** : le bug était intermittent. Vérifier plusieurs fois que
l'ingestion complète passe (4308 docs + commit) sans blocage.

**Si ça bloque encore** : le problème serait dans le commit (drain_pipeline
ou ASYNCIFY), pas dans la mailbox. Dans ce cas → investiguer avec des logs
ciblés (S4) puis envisager le DAG d'ingestion batch (S3).

## Recommandation long terme

S3 (DAG batch) reste la solution propre : un seul appel
`lucivy_ingest_batch(docs_json)` qui fait tout dans le scheduler.
Pas de milliers de ccalls ASYNCIFY, pas de cooperative waiting externe.
