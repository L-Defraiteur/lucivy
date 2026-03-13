# Benchmarks threading, optimisation BATCH_SIZE, build Emscripten

## Contexte

Après l'implémentation de `startsWith` et le nettoyage de la cascade, on valide que la refacto threading (crossbeam→flume, scheduler acteur, Mutex/Condvar reply) n'a rien cassé en natif, et on compare les performances startsWith vs contains.

## Stress test threading natif — PASSÉ

Nouveau bench `benches/stress_threading.rs` (criterion). Aucun deadlock, aucune corruption, toutes les assertions doc count passent.

### Commit cycles (5 commits, 588 fichiers source)

| Threads | Temps |
|---------|-------|
| 1t | 910ms |
| 2t | 528ms |
| 4t | 396ms |
| 8t | 379ms |

Scaling correct, pas de saturation précoce.

### Index concurrent + search

Writer indexe pendant que 2 threads de recherche hammèrent le reader. Stable, ~10-16 search iterations concurrentes pendant l'indexation. Pas de crash.

### Rapid-fire 50 commits, 8 threads

1.48s, toutes assertions passent. Le pipeline flume + Mutex/Condvar commit tient sous pression.

## startsWith vs contains — résultats

| Query | contains (ngram) | startsWith (FST) | Ratio |
|-------|-----------------|-------------------|-------|
| `fn` | 107ms | 16µs | **6700x** |
| `handle_b` | 16.6ms | 2.6µs | **6400x** |
| `segment` | 74.7ms | 48.5µs | **1540x** |
| `auto` | 11.8ms | 18.6µs | **630x** |
| `query_p` | 30.7ms | 2.4µs | **12800x** |
| multi `fn new` | 100.9ms | 231µs | **437x** |

startsWith est entre 437x et 12800x plus rapide que contains. Le FST range direct élimine tout l'overhead trigram + stored text verification.

## Comparaison main vs feature/startsWith — régression threading

Bench `index-bench` (criterion) exécuté sur les deux branches via `critcmp`.

### Régression identifiée

Sur le bench HDFS (gros dataset), indexation sans commit :
- main (crossbeam, threads dédiés) : **348ms**
- feature BATCH_SIZE=32 (scheduler acteur) : **503ms** → **1.45x plus lent**

Cause : le scheduler acteur ajoute de l'overhead par rapport aux threads dédiés de main. Pas flume vs crossbeam (les deux sont rapides), mais le mécanisme de dispatch : `Mutex<BinaryHeap>` ready queue + `Mutex<HashMap>` actors (take/put à chaque batch de messages).

### Optimisation : BATCH_SIZE

Le scheduler traite N messages par batch avant de yield. Augmenter N amortit le coût des locks.

| BATCH_SIZE | Temps | vs main |
|------------|-------|---------|
| 32 (avant) | 503ms | 1.45x |
| 256 | 407ms | 1.17x |
| 512 | 394ms | 1.13x |
| **1024** | **387ms** | **1.11x** |

Plateau atteint à ~1024. Le delta résiduel (11%) est l'overhead structurel du scheduler (HashMap take/put). **BATCH_SIZE=1024 retenu.**

### Tentative spawn_pinned (thread dédié par worker)

Ajout de `spawn_pinned()` au scheduler : l'acteur tourne en `loop { recv(); handle(); }` sur un thread dédié, sans passer par la ready queue ni le HashMap.

Résultat : **414ms** — plus lent que BATCH_SIZE=1024 seul (387ms). Le context switching entre threads dédiés est plus coûteux que le batching du scheduler sur un même thread. **Reverté**, `spawn_pinned` reste disponible dans le code mais non utilisé.

## Build Emscripten

Build WASM réussi avec les changements (startsWith + BATCH_SIZE=1024 + cascade cleanup).

```
lucivy.wasm : 6.6 MB
lucivy.js   : 87 KB
```

Copié dans `playground/pkg/`. Serveur playground fonctionnel sur port 8787.

## Fichiers modifiés/créés

```
src/actor/scheduler.rs          # BATCH_SIZE 32→1024, ajout spawn_pinned (non utilisé)
src/actor/mailbox.rs            # receiver pub(super) pour spawn_pinned
benches/stress_threading.rs     # nouveau bench stress + startsWith vs contains
Cargo.toml                      # [[bench]] stress_threading
docs/12-mars-2026-12h28/11-*    # plan mis à jour (priorité stress test natif)
docs/12-mars-2026-12h28/12-*    # draft LinkedIn
bindings/emscripten/pkg/*       # rebuild WASM
playground/pkg/*                # rebuild WASM
```

## Prochaines étapes

1. Commit + push les changements (bench, BATCH_SIZE, docs)
2. Tester startsWith dans le playground navigateur
3. Publication Emscripten npm
4. Post LinkedIn avec les chiffres de bench
