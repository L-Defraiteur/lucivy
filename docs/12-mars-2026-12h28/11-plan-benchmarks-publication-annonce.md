# Plan : benchmarks, publication, annonce communauté

## Contexte

Lucivy a franchi un cap majeur. Trois axes à communiquer :

1. **WASM multithreadé à 100%** — indexation et recherche tournent sur Web Workers, y compris le commit (thread dédié + Mutex/Condvar). Plus de limitation single-thread.
2. **`startsWith` query** — nouveau type de recherche exploitant le FST (trie natif) pour du prefix search en O(len(prefix)), zéro I/O stored text. Multi-token, fuzzy optionnel.
3. **Fix UTF-8** — normalisation Unicode manquante dans le pipeline ngram qui causait des panics sur les caractères multi-octets.

## Chantier 1 : Benchmarks & stress test threading

**Objectif principal** : valider que toute la refacto threading (crossbeam→flume, Mutex/Condvar reply, commit thread dédié) n'a rien cassé en usage **natif**. Le WASM multithreadé est un bonus qu'on est allé chercher loin — mais si la base natif régresse ou deadlock, ça ne sert à rien.

**Infra existante** : `benches/ngram_bench.rs` (criterion) — indexation 1/2/4 threads + recherche contains. Compare entre branches via `--save-baseline` + `critcmp`.

### 1a. Stress test threading natif (priorité)

C'est le test critique. On a touché aux channels (flume), au reply oneshot (Mutex/Condvar), au commit thread. Il faut prouver que ça tient sous charge.

- **Indexation haute concurrence** : 1, 2, 4, 8 threads sur le même corpus
- **Index + search concurrent** : writer qui indexe pendant que des readers cherchent (simule un usage réel)
- **Commit sous pression** : boucle d'add_document + commit répétés, vérifier pas de deadlock ni corruption
- **Longue durée** : 1000 cycles index/commit/search pour détecter des races intermittentes
- **Vérification intégrité** : après chaque cycle, count total des docs == attendu

### 1b. feature/startsWith vs main (régression check)

Comparer sur le même corpus (source files du repo) :
- Indexation 1/2/4 threads : pas de régression attendue (pas de changement au pipeline d'indexation)
- Recherche contains : mêmes queries, même perf (la cascade simplifiée ne devrait pas impacter le path ngram)
- Utiliser `--save-baseline main` sur main, `--save-baseline feature-startswith` sur la branche, puis `critcmp`

### 1c. contains vs startsWith

Sur la branche feature/startsWith :
- Mêmes termes de recherche adaptés en préfixe (ex: `"handle_b"` au lieu de `"handle_batch"`)
- `contains` (ngram path) vs `startsWith` (FST path)
- Mesurer latence moyenne via criterion
- Varier la longueur du préfixe (2 chars, 5 chars, mot complet + préfixe partiel)

**Attendu** : startsWith significativement plus rapide (FST range direct, zéro stored text I/O).

### 1d. Stress test WASM (bonus)

- Valider dans le playground Emscripten que l'indexation multithreadée + commit + search marchent
- Pas de criterion ici, juste fonctionnel : pas de crash, pas de deadlock
- Test dans Chrome (SharedArrayBuffer + COOP/COEP)

## Chantier 2 : Publication bindings

### Ordre de publication

1. **Emscripten (npm)** — priorité, c'est le binding qui bénéficie le plus des changements (WASM threads + startsWith)
2. **Node.js (npm)** — napi-rs, publication standard
3. **Python (PyPI)** — maturin, publication standard

### Pour chaque binding

- Bump version
- Vérifier que `startsWith` est accessible via le JSON query API (devrait être automatique)
- Tests de non-régression
- Publish

### Emscripten spécifiquement

- Vérifier le build avec `-pthread` et `PTHREAD_POOL_SIZE`
- Tester dans le playground avec les nouveaux features
- Package npm : `@nicmusic-bam/lucivy-wasm` ou nom actuel

## Chantier 3 : Annonce communauté

### Post LinkedIn

**Angle** : lucivy est maintenant un moteur de recherche full-text qui tourne **entièrement** dans le navigateur, multithreadé, avec des performances qui rivalisent avec le natif.

**Points clés** :
- WASM multithreadé : indexation + recherche + commit, tout en parallèle dans le browser
- `startsWith` : recherche par préfixe en O(len(prefix)) via FST, plus rapide que les approches trigram classiques
- Fix UTF-8 : support complet des caractères Unicode
- Zéro serveur : tout tourne côté client

**Ton** : technique mais accessible, montrer que c'est un vrai moteur pas un toy project.

### Structure du post

```
[Accroche] — Un moteur de recherche full-text, multithreadé, dans votre navigateur.

[Problème] — Les solutions full-text browser existantes sont single-thread ou limitées.

[Solution] — lucivy compile en WASM avec Web Workers, SharedArrayBuffer,
et exploite des structures de données (FST) qui permettent des recherches
par préfixe en temps constant.

[Résultats] — Benchmarks à inclure ici (chiffres concrets).

[Call to action] — Lien vers le playground, le package npm, le repo.
```

### Timing

1. D'abord les benchmarks (chiffres concrets pour le post)
2. Puis publication Emscripten sur npm (lien concret)
3. Puis le post LinkedIn avec les deux

## Ordre d'exécution

```
1. Stress test threading natif (valider la refacto flume/Mutex/Condvar/commit)
2. Benchmark feature/startsWith vs main (régression check natif)
3. Benchmark contains vs startsWith (chiffres pour le post)
4. Stress test WASM (fonctionnel, playground)
5. Publication Emscripten npm
6. Publication Node.js npm
7. Publication Python PyPI
8. Rédaction post LinkedIn avec chiffres + liens
```
