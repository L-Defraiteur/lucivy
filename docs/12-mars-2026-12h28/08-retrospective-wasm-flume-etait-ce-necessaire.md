# Rétrospective : wasm-flume — qu'est-ce qui était nécessaire ?

## Vue d'ensemble

La branche `wasm-flume` comprend 7 commits depuis `main` (9-12 mars 2026). L'objectif : faire fonctionner l'indexation multithreadée de lucivy dans un browser via Emscripten + pthreads + ASYNCIFY.

Ce document analyse rétrospectivement chaque changement : était-il nécessaire pour atteindre l'objectif, ou était-ce une exploration qui s'est avérée superflue ?

---

## Inventaire des changements

### Commit 1 — `5bf7a6b` : Permanent indexer worker threads
**Changement** : Remplacement des threads éphémères (spawn/join par commit) par des workers permanents recevant des documents via un channel MPMC partagé.

**Nécessaire ?** OUI — fondamental.

Les threads éphémères (`std::thread::spawn` à chaque commit) étaient incompatibles avec Emscripten : chaque `pthread_create` pendant un `ccall` ASYNCIFY peut deadlocker (le nouveau Web Worker a besoin de l'event loop du proxy, qui est occupé par le ccall). Des workers permanents, créés une seule fois à l'init quand l'event loop est libre, éliminent ce problème.

### Commit 2 — `62b6b14` : Actor architecture complète (scheduler, mailboxes, actors)
**Changement** : Remplacement de rayon + thread::spawn par un scheduler global avec N threads persistants, des mailboxes FIFO, et un modèle acteur (IndexerActor, SegmentUpdaterActor).

**Nécessaire ?** OUI, mais **surdimensionné** pour l'objectif WASM seul.

**Ce qui était strictement nécessaire** : remplacer `rayon::ThreadPool` (qui fait du work-stealing avec des futex) par un pool de threads persistants plus simple. L'actor model complet (mailbox, ActorRef, Reply, EventBus) est une architecture propre et extensible, mais pour juste faire marcher le commit WASM, un pool de threads plus simple aurait suffi.

**Cela dit** : cette architecture est un investissement pour le futur (observabilité, isolation des responsabilités, testabilité). Ce n'est pas du travail "perdu". C'est une fondation solide, pas un détour.

### Commit 3 — `8170399` : Fix scheduler deadlocks (notify_one + TOCTOU)
**Changement** : Fix de deux vrais bugs dans le scheduler (threads parkés sans notification, race condition entre `has_pending()` et `is_idle`). Ajout des self-messages pour les étapes de merge.

**Nécessaire ?** OUI — c'est la conséquence directe d'avoir écrit un scheduler custom. Ces bugs auraient causé des deadlocks en natif aussi, pas seulement en WASM.

### Commit 4 — `bdf43d5` : Benchmark NGram
**Changement** : Ajout d'un benchmark Criterion pour l'indexation et la recherche NGram.

**Nécessaire pour WASM ?** NON — c'est un outil de mesure de performance, orthogonal au problème WASM. Utile indépendamment.

### Commit 5 — `4cb6491` : reply.rs Mutex+Condvar + thread/poll commit
**Changement** : Remplacement du oneshot `crossbeam::bounded(1)` dans `reply.rs` par `Mutex<State<T>> + Condvar`. Pattern thread+poll pour le commit côté emscripten.

**Nécessaire ?** PARTIELLEMENT.

Le remplacement dans `reply.rs` était motivé par l'hypothèse que le futex de crossbeam empêchait ASYNCIFY d'intercepter les waits. C'était **vrai** mais **insuffisant** : le deadlock avait d'autres causes (eprintln yield points, ASYNCIFY stack overflow silencieux). Le pattern thread+poll a été l'ancêtre de la solution finale (commit_async + SAB polling), mais sous cette forme il ne marchait pas encore.

**Verdict** : stepping stone nécessaire dans le processus de diagnostic, mais le code de ce commit a été largement remplacé par le commit suivant.

### Commit 6 — `1546060` : Commit async + SAB ring buffer + ZIP fix
**Changement** :
- Migration crossbeam → flume dans tout le codebase
- Commit sur pthread dédié + AtomicU32 polling via SAB
- Ring buffer 64KB en SharedArrayBuffer pour l'observabilité
- serve.mjs avec COOP/COEP natifs
- ZIP central directory parsing

**Nécessaire ?** En partie. Décomposons :

| Sous-changement | Nécessaire ? | Justification |
|---|---|---|
| **crossbeam → flume** | NON | Drop-in qui n'a pas résolu le deadlock. La solution réelle (pthread dédié + SAB) fonctionne indépendamment du channel impl. |
| **Commit sur pthread dédié + AtomicU32** | OUI — c'est LA solution | Élimine ASYNCIFY du write path. Le commit tourne sur une vraie stack 2MB. |
| **Ring buffer SAB** | OUI — critique pour le diagnostic | Sans le ring buffer, on n'aurait jamais vu que le code Rust terminait mais qu'ASYNCIFY ne retournait pas. C'est ce qui a permis de pivoter de "deadlock Mutex" vers "bug ASYNCIFY yield points". |
| **serve.mjs COOP/COEP** | OUI | Requis pour SharedArrayBuffer. Le coi-serviceworker causait des bugs de cache (WASM stale servi malgré rebuild). |
| **ZIP central directory** | OUI mais indépendant | Bug réel, fix nécessaire, mais pas lié au multithreading. |

### Commit 7 — `1b43d1e` : UTF-8 char boundary fix
**Changement** : `floor_char_boundary` / `ceil_char_boundary` dans ngram_contains_query.rs.

**Nécessaire ?** OUI — bug de crash pur. Pas lié à WASM directement mais découvert grâce au playground fonctionnel.

---

## La question flume

### Pourquoi on a migré vers flume
L'hypothèse était : crossbeam utilise des futex, ASYNCIFY ne peut pas intercepter les futex, donc remplacer crossbeam par flume (qui utilise `Mutex<VecDeque> + Condvar`) rendrait le commit compatible ASYNCIFY.

### Pourquoi ça n'a pas résolu le problème
Le deadlock commit avait **trois couches de causes** :

1. **`eprintln!` dans les fonctions FFI** = yield points ASYNCIFY parasites. Chaque `fd_write` proxyé vers le main thread ajoute un yield/restore. Trop de yield points dans une même callstack dépasse `ASYNCIFY_STACK_SIZE`.

2. **`writer.commit()` descend trop profond** — même avec flume au lieu de crossbeam, le chemin commit passe par des dizaines de locks imbriqués (scheduler → mailbox → acteur → merge → segments). La profondeur totale de stack + restauration ASYNCIFY était ingérable.

3. **Le mécanisme ASYNCIFY lui-même a des limites** — il ne peut pas gérer des fonctions qui font des dizaines de yield points imbriqués profondément.

Flume corrigeait la couche 1 (futex → condvar), mais les couches 2 et 3 rendaient cette correction insuffisante.

### Peut-on revenir à crossbeam ?
**Techniquement oui.** La solution finale (commit sur pthread dédié) n'a aucune interaction ASYNCIFY. Le pthread commit a sa propre stack 2MB, pas de yield ASYNCIFY. Flume ou crossbeam : le commit marche dans les deux cas.

**En pratique** : garder flume est le choix pragmatique :
- Le diff crossbeam→flume est minime (alias d'import)
- Flume est activement maintenu
- Si un jour on veut faire passer d'autres opérations par ASYNCIFY (pas seulement le commit), flume sera un avantage
- Pas de raison de revenir en arrière pour sauver 0 lignes de code

---

## Le chemin minimal (avec recul)

Si on avait su dès le départ ce qu'on sait maintenant, voici le chemin minimal :

### 1. Workers permanents (nécessaire)
Remplacer les spawns éphémères par un pool de threads créés à l'init. Pas besoin du modèle acteur complet — un thread pool simple avec une queue de travail aurait suffi.

### 2. Supprimer eprintln! des fonctions FFI (nécessaire)
Zéro `fd_write` → zéro yield point ASYNCIFY parasite → `lucivy_add` retourne normalement.

### 3. Commit sur pthread dédié (nécessaire)
`std::thread::spawn` dans `lucivy_commit_async`, status via `AtomicU32`, polling JS via `Atomics.load` sur SAB.

### 4. serve.mjs avec COOP/COEP (nécessaire)
Pour que `SharedArrayBuffer` soit disponible.

### Pas nécessaire dans le chemin minimal :
- ❌ crossbeam → flume
- ❌ reply.rs Mutex+Condvar (le reply n'est jamais sur le chemin ASYNCIFY avec la solution pthread dédié)
- ❌ Ring buffer SAB (indispensable pour le **diagnostic**, mais pas pour la **solution** une fois connue)
- ❌ Modèle acteur complet (un simple thread pool suffisait pour WASM)
- ❌ Benchmark NGram

---

## Le chemin réel vs le chemin minimal

| Étape | Chemin réel | Chemin minimal | Commentaire |
|---|---|---|---|
| Architecture threading | Actor model complet (scheduler, mailbox, events) | Thread pool simple | L'actor model est un investissement futur, pas un détour |
| Channel impl | crossbeam → flume migration | Garder crossbeam | Changement superflu pour WASM, mais inoffensif |
| Reply channel | Mutex+Condvar custom | Garder crossbeam bounded(1) | Nécessaire pour le debug, remplacé ensuite |
| Commit WASM | 4 itérations (sync → thread+poll → persistent worker → pthread dédié) | Pthread dédié directement | Chaque itération a apporté un diagnostic |
| Observabilité | Ring buffer SAB + scheduler LOG_HOOK | Rien (si on connaît la solution) | Le ring buffer a été **l'outil de diagnostic décisif** |
| Serveur | serve.mjs custom | serve.mjs custom | Identique — nécessaire dans les deux cas |

### Coût de l'exploration
- ~5 commits d'exploration avant la solution
- ~3 sessions de debug (dont 1 perdue à cause du WASM stale)
- Migration flume techniquement inutile mais pas coûteuse

### Valeur de l'exploration
- L'actor model est une fondation solide (réutilisable hors WASM)
- Le ring buffer SAB est un outil d'observabilité permanent
- Le scheduler LOG_HOOK donne de la visibilité même en prod
- Chaque itération de commit a affiné la compréhension d'ASYNCIFY
- Le bug du WASM stale (playground/pkg vs bindings/emscripten/pkg) a mené au fix du build.sh step 3

---

## Leçons pour le futur

### 1. Vérifier d'abord que le bon binaire est servi
Le bug le plus coûteux a été de tester un ancien WASM pendant des heures. **Checksum automatique en fin de build** aurait économisé une session entière.

### 2. ASYNCIFY n'est pas fait pour du code profond
Règle pragmatique : toute opération qui descend à plus de ~3 niveaux de locks/condvar dans la callstack ne devrait **jamais** passer par ASYNCIFY. Utiliser un pthread dédié + signalisation atomique.

### 3. Observer avant de changer
Le ring buffer SAB a été la clé. Sans lui, on changeait du code à l'aveugle (flume, reply.rs) sans comprendre la vraie cause. La bonne séquence est : **observer → diagnostiquer → corriger**, pas **hypothèse → migration → tester**.

### 4. Les migrations "drop-in" ont un coût psychologique
Flume était un "drop-in" pour crossbeam. Ça a pris 30 minutes. Mais ça a créé l'illusion qu'on avait corrigé quelque chose, et retardé la recherche de la vraie cause.

### 5. L'actor model valait le coup malgré tout
Même si un thread pool simple aurait suffi pour WASM, le scheduler + acteurs donnent :
- Observabilité native (events, LOG_HOOK)
- Isolation des responsabilités (IndexerActor vs SegmentUpdaterActor)
- Testabilité (on peut tester les acteurs individuellement)
- Foundation pour le futur (async/await, batching, priorités)

Ce n'est pas du travail superflu — c'est du travail anticipé.

---

## Verdict

**Est-ce qu'on est allés trop loin ?** Partiellement. La migration flume était inutile pour résoudre le problème WASM. Le modèle acteur complet était surdimensionné pour l'objectif immédiat. Mais aucun de ces changements n'est du **mauvais** code — ce sont des fondations qui serviront.

**Le vrai surcoût a été le temps de diagnostic**, pas le code écrit. La session perdue sur le WASM stale et les itérations de commit (sync → poll → persistent → pthread dédié) sont le prix de l'exploration d'un territoire mal documenté (ASYNCIFY + Rust multithreadé + Emscripten pthreads).

**En résumé** : le chemin minimal aurait été ~3 changements ciblés. On en a fait ~7. Mais les 4 "en trop" ne sont pas du poids mort — ils constituent une infrastructure d'observabilité et une architecture qui vaudront leur investissement à moyen terme.
