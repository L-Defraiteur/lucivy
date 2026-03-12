# Diagnostic commit WASM — session 2

## Contexte

Suite du doc 03. On a corrigé le bug critique (playground servait un ancien WASM) et testé toutes les approches de commit.

## Résultats des tests

### Test 1 : Commit bloquant avec CommitWorker persistant (thread dédié)

**Setup** : `CommitWorker` global (singleton via `OnceLock`), thread spawné dans `main()`, job envoyé via `flume::bounded(1)`, réponse via reply channel.

**Résultat index seul** : **FONCTIONNE** — commit en 27ms, search OK, `ALL PASSED`.

```
[commit] calling lucivy_commit (blocking)...
[commit] lucivy_commit returned: ok
commit done in 27ms: {"numDocs":1}
search results: [{"docId":1,"score":0.28768212}]
ALL PASSED
```

**Résultat avec 2 index (playground)** : **DEADLOCK** — le commit bloque indéfiniment.

Le playground charge un snapshot (532 docs via `importSnapshot`), puis l'utilisateur importe un fichier → `create` + `add` + `commit` sur un 2ème index. Le commit ne retourne jamais.

**Hypothèse** : pool pthread épuisé. Avec `PTHREAD_POOL_SIZE=8` :
- 1 proxy pthread (main)
- 4 scheduler threads
- 1 commit worker thread
- 1+ warm-gc threads (un par reader/index)
- = 7-8 threads → pool plein

### Test 2 : Commit poll (begin + poll)

**`lucivy_commit_begin`** envoyait le job via `try_send` (non-bloquant), puis JS poll `lucivy_commit_poll` toutes les 10ms.

**Résultat** : `lucivy_commit_begin` lui-même ne retourne jamais. Même le `try_send` bloque — probablement parce que le ccall via ASYNCIFY proxy vers le proxy pthread qui est déjà occupé ou parce que le thread commit n'a jamais démarré.

### Test 3 : Commit direct (sans thread, sans CommitWorker)

**Setup** : `writer.commit()` appelé directement dans la fonction FFI `lucivy_commit`, sur le proxy pthread. Zéro thread supplémentaire. ASYNCIFY devrait yield pendant les locks internes (Mutex, Condvar).

**Résultat** : **DEADLOCK** — le ccall `lucivy_commit` ne retourne jamais, même sans aucun thread commit.

```
[callStr] calling lucivy_commit
← bloque ici, ne retourne jamais
```

Ceci prouve que le problème n'est **PAS** l'exhaustion du pool pthread. Le `writer.commit()` lui-même deadlock sous ASYNCIFY sur le proxy pthread.

## Analyse du deadlock `writer.commit()`

`writer.commit()` dans lucivy/tantivy fait :
1. Envoie un message au `SegmentUpdater` actor (via mailbox flume)
2. Attend la réponse (via `Reply` channel — aussi flume)
3. Le `SegmentUpdater` tourne sur un thread du scheduler

Chaîne de causalité probable :
1. `lucivy_commit` tourne sur le proxy pthread
2. `writer.commit()` envoie un message au SegmentUpdater et attend la réponse
3. Le SegmentUpdater (sur un thread scheduler) traite le commit
4. Le SegmentUpdater doit peut-être communiquer avec un autre actor ou faire un callback
5. Ce callback a besoin du proxy pthread (via ASYNCIFY) mais celui-ci est bloqué en attente
6. **Deadlock** : proxy pthread attend SegmentUpdater, SegmentUpdater attend proxy pthread

### Pourquoi ça marche avec 1 seul index ?

Quand on teste avec un seul index fraîchement créé (`create` + `add 1 doc` + `commit`), le commit est trivial : un seul segment, pas de merge nécessaire. Le SegmentUpdater retourne immédiatement.

Avec le playground (snapshot de 532 docs déjà indexés + 2ème index), le scheduler a déjà 4 threads actifs avec des actors enregistrés. Le commit sur le 2ème index peut déclencher des interactions plus complexes entre actors.

## Observabilité — état des lieux

### Ce qui ne marche PAS
- `eprintln!` dans Rust : les logs des pthreads emscripten ne sont **jamais visibles** dans la console browser
- `rlog!` (buffer global Rust) + `lucivy_read_logs` FFI : la fonction existe dans le WASM mais le ccall échoue avec `func is not a function` (probablement le coi-serviceworker qui sert un ancien WASM malgré les rebuilds)
- Log poller (setInterval + ccall read_logs) : ne peut pas s'exécuter pendant que le proxy pthread est bloqué par le commit
- `Module.asm._lucivy_read_logs` : `Module.asm` n'existe pas sous PROXY_TO_PTHREAD (retourne "no asm")

### Ce qui MARCHE
- `wlog()` dans le worker JS : `self.postMessage({ type: 'log', msg })` → capturé par main thread `console.log('[worker]', ...)`
- Logs JS dans `callStr` : on voit chaque appel FFI et son retour
- `window._allLogs` : patch console.log dans index.html pour capturer tout

### Ce qu'il faudrait
- Mécanisme de log Rust qui n'utilise PAS le proxy pthread (SharedArrayBuffer ring buffer écrit par Rust, lu par JS ?)
- Ou ASYNCIFY stack trace pour voir exactement où le commit bloque

## Bug coi-serviceworker (cache)

Le coi-serviceworker intercepte TOUTES les requêtes HTTP et sert potentiellement des versions cachées des fichiers WASM. Malgré :
- `Cache-Control: no-store` sur le serveur
- `caches.delete()` côté JS
- `serviceWorker.unregister()` côté JS

Le WASM chargé ne contenait pas `_lucivy_read_logs` alors que le fichier sur disque et le serveur HTTP le servaient correctement. Le coi-serviceworker se re-enregistre automatiquement via `<script src="coi-serviceworker.js">` dans le HTML.

**Note** : sans coi-serviceworker, les headers COOP/COEP doivent être ajoutés par le serveur. Un serveur Python avec `credentialless` COEP a été testé mais génère toujours 8 erreurs COEP pour les em-pthread workers.

## Corrections apportées (conservées)

### Build system
- **`build.sh` step 3** : copie automatique de `bindings/emscripten/pkg/` vers `playground/pkg/` après chaque build. Plus jamais de désynchronisation.

### Architecture actuelle du commit (après simplification)
- Plus de `CommitWorker`, plus de thread commit dédié
- `lucivy_commit` fait directement `writer.commit()` + `reader.reload()` sur le proxy pthread
- Approche la plus simple, mais deadlock quand il y a interaction entre actors

### Observabilité
- `rlog!` macro + `LOG_BUF` global en Rust (pour quand on aura un mécanisme de lecture fonctionnel)
- `lucivy_read_logs` FFI exportée (fonctionne en théorie, bloquée par cache coi-serviceworker en pratique)
- `drainRustLogs()` dans le worker JS avant chaque callStr
- Traces `wlog` détaillées dans le worker pour chaque callStr

## Fichiers modifiés (cette session)

```
bindings/emscripten/src/lib.rs    # CommitWorker supprimé, commit direct, rlog!, lucivy_read_logs
bindings/emscripten/build.sh      # step 3 copie playground, +_lucivy_read_logs export
playground/js/lucivy-worker.js    # drainRustLogs, traces callStr, commit direct
playground/js/lucivy.js           # (inchangé)
playground/index.html             # window._allLogs capture
```

## Prochaines pistes

1. **Investiguer le deadlock writer.commit()** :
   - Ajouter des logs dans le code Rust de `writer.commit()` / `SegmentUpdater` pour tracer le parcours exact
   - Vérifier si le `Reply` channel du commit utilise un mécanisme bloquant incompatible avec ASYNCIFY
   - Tester avec `ASYNCIFY_STACK_SIZE` plus grand (actuellement 65536)

2. **Résoudre l'observabilité** :
   - Implémenter un ring buffer en SharedArrayBuffer écrit par Rust, lu par JS sans ccall
   - Ou servir le playground avec un vrai serveur HTTP (pas python) qui gère les headers correctement sans coi-serviceworker

3. **Alternative au commit sur proxy pthread** :
   - Faire tourner le commit sur un pthread dédié (pas le proxy), mais sans passer par ASYNCIFY
   - Le JS poll un flag en SharedArrayBuffer (pas via ccall) pour savoir quand c'est fini

4. **Réduire les threads** :
   - Scheduler à 2 threads au lieu de 4
   - Désactiver warm-gc en WASM
   - `PTHREAD_POOL_SIZE=16` pour avoir de la marge
