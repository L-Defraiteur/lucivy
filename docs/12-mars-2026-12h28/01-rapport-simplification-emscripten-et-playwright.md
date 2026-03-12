# Doc 01 — Simplification emscripten/playground + tests Playwright

**Date** : 12 mars 2026
**Branche** : `scheduler-beta`
**Réf** : docs `08`, `09`, `11` (scheduler, self-messages, Luciol vision)

---

## Objectif de la session

Simplifier le code JS/Rust du playground emscripten maintenant qu'on a le global scheduler avec threads persistants, et valider avec des tests Playwright.

---

## Analyse des "hacks" emscripten vs main

### Changements entre main et scheduler-beta (avant cette session)

| Changement | Fichier | Raison |
|---|---|---|
| `{ async: true }` sur tous les `ccall` | lucivy-worker.js | Requis par ASYNCIFY |
| `PROXY_TO_PTHREAD` + `ASYNCIFY` + `_main` | build.sh | Empêche les appels bloquants de geler l'event loop |
| `setTimeout(r, 0)` après create/open/importSnapshot | lucivy-worker.js | Laisser l'event loop activer les pthreads du global scheduler |
| `lucivy_commit` non-bloquant + `lucivy_commit_poll` + polling JS | lib.rs + worker.js | Commit sur thread dédié + polling pour ne pas bloquer l'event loop |
| `PTHREAD_POOL_SIZE=8 → 20` | build.sh | Plus de threads pour le scheduler |
| `console.log` debug partout | worker.js + index.html | Debug de développement |

### Analyse : qu'est-ce qui est encore nécessaire ?

**ASYNCIFY est toujours nécessaire.** Même avec le global scheduler et threads persistants, le thread qui appelle les FFI (= le thread du Web Worker) bloque sur :
- `Mutex::lock()` du writer (dans `lucivy_add`, `lucivy_remove`, etc.)
- `wait_blocking()` → `crossbeam recv()` → futex (dans `prepare_commit`, `rollback`)

Sans ASYNCIFY, un appel bloquant gèle l'event loop → les pthreads ne peuvent plus se coordonner → deadlock.

**`{ async: true }` sur ccall** : obligatoire avec ASYNCIFY.

**`setTimeout(r, 0)`** : nécessaire au premier `create` pour laisser les threads du global scheduler démarrer. Les suivants sont redondants (threads déjà vivants).

**`PROXY_TO_PTHREAD`** : nécessaire, sinon les FFI bloquants gèlent le thread principal du worker.

### Tentative de simplification du commit

**Hypothèse** : avec ASYNCIFY, le commit synchrone devrait marcher (ASYNCIFY yield automatiquement pendant les `wait_blocking`).

**Test** : remplacé `lucivy_commit` par version synchrone (lock writer → commit → reload → return "ok"). ASYNCIFY devrait intercepter le `crossbeam recv()` et yield.

**Résultat** : **DEADLOCK**. Le commit synchrone bloque indéfiniment. ASYNCIFY ne semble pas intercepter les futex waits de crossbeam correctement dans ce contexte (probablement car le commit traverse trop de couches : writer.commit → prepare_commit → flush indexers → wait_blocking → crossbeam recv → futex_wait).

**Conclusion** : le pattern `lucivy_commit` non-bloquant + `lucivy_commit_poll` + polling JS est **nécessaire**. Ce n'est pas un hack — c'est le design correct pour emscripten.

---

## Simplifications effectuées

### 1. Nettoyage console.log debug

Retiré tous les `console.log('[worker] ...')` et `console.log('[import] ...')` de :
- `playground/js/lucivy-worker.js` (add start/done, commit begin/poll)
- `playground/index.html` (import creating/created/add/committed)

### 2. Suppression setTimeout redondants

Retiré `await new Promise(r => setTimeout(r, 0))` dans :
- `open` — les threads du global scheduler sont déjà vivants après le premier `create`
- `importSnapshot` — idem

Gardé uniquement dans `create` (premier usage → activation des pthreads).

### 3. PTHREAD_POOL_SIZE

Remis à 8 (le global scheduler utilise `num_cpus` threads, en WASM rarement plus de 4-8 cores).

### 4. Commentaires du worker mis à jour

Le header du worker explique maintenant le modèle de threading (global actor scheduler + ASYNCIFY).

---

## Tests Playwright

### Script : `playground/test_playground.py`

Test E2E du playground avec Playwright (headless Chromium).

**Test 1 — Demo snapshot import + search** : ✅ PASSE
- Charge `dataset.luce` (532 docs du code source lucivy)
- Recherche "scheduler" → 6 résultats en ~150-240ms

**Test 2 — Programmatic create/add/commit/search** : 🔄 EN COURS
- Problème : le `lucivy` JS est dans un `<script type="module">`, pas accessible depuis `page.evaluate()`
- Solution en cours : exposer `window._lucivy` dans index.html
- Le `page.set_input_files` de Playwright ne déclenchait pas l'import correctement non plus
- L'instanciation d'un second worker emscripten échoue ("func is not a function" — un seul module PROXY_TO_PTHREAD par page)

### État des modifications pour le test

Dans `playground/index.html` :
```js
let lucivy = null;
window._lucivy = null;  // expose for test automation
// ... dans startup():
lucivy = new Lucivy('./js/lucivy-worker.js');
window._lucivy = lucivy;
```

Le test devrait utiliser cette instance exposée :
```python
result = page.evaluate("""async () => {
    const lv = window._lucivy;
    const idx = await lv.create('/test_index', [...]);
    await idx.add(0, { title: 'Hello', body: 'fuzzy matching' });
    await idx.commit();
    return await idx.search({ type: 'contains', field: 'body', value: 'fuzzy' }, ...);
}""")
```

**Ce test n'a pas encore été exécuté** — la session s'est terminée avant.

---

## État du build WASM

Le WASM a été rebuild avec les changements finaux :
```bash
bash bindings/emscripten/build.sh  # ✅ build OK, ~37s
# Output: pkg/lucivy.js (87K) + pkg/lucivy.wasm (6.8M)
```

Le build inclut le commit non-bloquant (`lucivy_commit` + `lucivy_commit_poll`).

---

## Fichiers modifiés (non commités)

| Fichier | Changements |
|---------|-------------|
| `bindings/emscripten/src/lib.rs` | `__main_argc_argv` dummy, `PROXY_TO_PTHREAD` doc, commit non-bloquant + poll (propre), `drop(writer)` avant reload |
| `bindings/emscripten/build.sh` | `PROXY_TO_PTHREAD`, `ASYNCIFY`, `_main` export, `_lucivy_commit_poll` export, pool size 8 |
| `playground/js/lucivy-worker.js` | Tous ccall `{ async: true }`, commit polling propre (sans debug logs), setTimeout uniquement dans create, header doc mis à jour |
| `playground/index.html` | Debug logs retirés, `window._lucivy` exposé pour tests |
| `playground/test_playground.py` | NOUVEAU — test Playwright (test 1 passe, test 2 en cours) |
| `src/actor/scheduler.rs` | Stress tests + TOCTOU fix + WakeHandle name (session précédente, non commité) |
| `src/actor/mailbox.rs` | `actor_name` dans WakeHandle (session précédente, non commité) |
| `docs/11-mars-2026-12h25/11-vision-actor-runtime-lib.md` | Vision Luciol (session précédente, non commité) |

---

## Prochaines étapes

1. **Finir le test Playwright** : exécuter le test avec `window._lucivy` et valider create/add/commit/search
2. **Commit tout** : scheduler fixes + stress tests + emscripten simplification + Playwright test + doc Luciol
3. **Merge scheduler-beta → main** : tous les pré-requis sont remplis (1142 tests OK, stress tests, benchmarks)

---

## Leçon clé

**ASYNCIFY ne suffit pas pour les commits.** Le `writer.commit()` traverse trop de couches de blocking (actor scheduler wait_blocking → crossbeam recv → futex) pour que ASYNCIFY puisse yield correctement. Le pattern thread dédié + polling est le bon design pour emscripten pthreads. Ce n'est pas un hack, c'est le pattern recommandé quand le blocking est profond.
