# Knowledge dump — scripts, tests, benchs, et où regarder quand ça casse

25 août 2026. Document autonome : tout ce qu'il faut pour relancer les mesures
et diagnostiquer, sans relire l'historique.

Préalable systématique : `export PATH="$HOME/.cargo/bin:$PATH"`.
**Toujours rediriger vers un fichier puis grepper** — jamais `| tail`, qui
tronque au mauvais endroit :
`cargo test ... > /tmp/x.txt 2>&1` puis `grep 'test result' /tmp/x.txt`.

---

## 1. Suites de tests

```bash
cargo test --lib                    # moteur, 1428 tests, ~17 s
cargo test -p luciole --lib         # acteurs/DAG, 169 tests
cargo test -p lucistore             # snapshot LUCE, 43 tests
cargo test -p lucivy-core --no-fail-fast --lib
```

Les suites d'intégration `lucivy-core` qui comptent après une modification du
format ou de l'écriture :

```bash
cargo test -p lucivy-core --no-fail-fast \
  --test test_luce_v3_roundtrip \   # export → import v3 shardé, comptes au bit
  --test test_snapshot_served \     # servir un LUCE sans l'extraire
  --test test_sfx_v3_pipeline \
  --test test_merge_contains \      # ~40 s, exerce la fusion
  --test test_two_fields \
  --test test_lazy_directory
```

Connus non verts et **pré-existants** : `bench_sharding` t01 (clone réseau) et
t04 (`sfx:false` n'existe plus).

Deux tests que la journée a montré manquants, ajoutés le soir :
`cargo test --lib merge_tests` (un merge sur un index **v2** et sur un v3 —
le merge v2 était cassé sans qu'aucun test le voie) et
`cargo test --lib v3_refuses_corrupt_block_counts` (comptes corrompus dans
un bloc SFP3).

### Tests de mesure (marqués `#[ignore]`, à lancer à la main)

| test | ce qu'il répond | commande |
|---|---|---|
| `test_playground_parity` | référence native du panel, tailles par extension | voir §2 |
| `test_touched_bytes` | octets **réellement** faultés par une requête | voir §4 |
| `test_wsp_density` | ce que gagnerait `.word_sfxpost` en varint | `WSP_DIR=<index> cargo test --release -p lucivy-core --test test_wsp_density -- --ignored --nocapture` |
| `test_sfxpost_density` | idem `.sfxpost`, en séparant en-tête/payload/checkpoints | `SFXPOST_DIR=<index> cargo test --release -p lucivy-core --test test_sfxpost_density -- --ignored --nocapture` |
| `test_sfx_v3_ground_truth` | vérité terrain des requêtes v3 (`V3_QUERIES=<q>`) | |

---

## 2. Le panel de parité — l'outil central

Le même panel de 21 requêtes tourne en natif et en navigateur, et les
résultats se comparent au bit. **C'est ce qui garantit qu'un changement de
format ne change pas les réponses.**

Panel : `playground/parity_panel.json` (contains strict/relax, split,
startsWith, term, phrase, fuzzy d1/d2, regex, parse simple et booléen, filtre
d'extension, chemin, no-hit).

### Référence native

```bash
head -10000 /tmp/corpus_indexed.list > /tmp/corpus_10k.list
PARITY_ROOT=/tmp/linux-bench \
PARITY_LIST=/tmp/corpus_10k.list \
PARITY_OUT=/tmp/parity_native_10k.json \
PARITY_COMPACT=100000 \
cargo test --release -p lucivy-core --test test_playground_parity -- --ignored --nocapture \
  > /tmp/parity.txt 2>&1
grep 'indexed\|on disk after' /tmp/parity.txt
```

Variables : `PARITY_ROOT` (racine du corpus), `PARITY_LIST` (liste de chemins
relatifs), `PARITY_OUT`, `PARITY_PANEL`, `PARITY_MAX_DOCS`, `PARITY_COMPACT`
(compacter à N docs/segment et remesurer), `PARITY_LIMIT`.

⚠️ **Le test écrit toujours dans `/tmp/lucivy_parity_native`** et l'efface au
début. Lancer un petit run détruit le gros index — je l'ai fait.

### Côté navigateur

```bash
cd playground && node serve.mjs        # port 9877, headers COOP/COEP
```

Puis, la page étant ouverte sur l'index voulu :

```bash
: > playground/diag.log
curl -s localhost:9877/eval/main -d @/tmp/parity_req.json   # lance le panel
# attendre que window._parityResult existe
curl -s localhost:9877/eval/main -d '{"js":"window._parityResult"}' > /tmp/parity_wasm.json
python3 playground/parity_diff.py /tmp/parity_native_10k.json /tmp/parity_wasm.json
```

`/tmp/parity_req.json` se fabrique depuis `playground/parity_run.js` :

```bash
python3 -c 'import json;print(json.dumps({"js":open("playground/parity_run.js").read()}))' \
  > /tmp/parity_req.json
```

`parity_diff.py` affiche `OK` / `TIE` / `DIFF` par requête, avec comptes,
top-10, scores et nombre de spans.

**Le seul DIFF attendu** est `path contains ethernet/intel` : neuf des dix
scores sont égaux au bit, et l'ordre des ex æquo dépend de l'adresse physique
(shard, segment, doc), qui diffère entre deux index construits séparément.

---

## 3. Le playground : paramètres d'URL et serveur de debug

`node playground/serve.mjs` sert sur **9877** avec les en-têtes COOP/COEP
nécessaires aux SharedArrayBuffer, et expose :

- `POST /eval/main` — évalue du JS **dans la page**
- `POST /eval/poll` — le worker vient chercher du JS à évaluer
- `POST /log` — la page et le worker y déversent leurs traces →
  **`playground/diag.log`**

### Paramètres d'URL

| paramètre | effet |
|---|---|
| `?corpus=<fichier.tar.gz>` | télécharge et indexe une archive (dans `playground/`) |
| `?open=<nom>` | ouvre un index OPFS existant, sans réindexer |
| `?nodemo` | ne charge pas le dataset embarqué |
| `?verbose` | `LUCIVY_VERBOSE` + `V3_PROFILE` |
| `?noopfs` | système de fichiers en mémoire (index perdu au rechargement) |
| `?cache=N` | `LUCIVY_FILE_CACHE_BYTES` en Mo |
| `?rammax=N` | `LUCIVY_RAM_INDEX_MAX` en Mo — au-delà, l'index est streamé |
| `?threads=N` | threads du planificateur luciole |
| `?wthreads=N` | threads d'écriture (le tas suit automatiquement) |
| `?compact=N` | compacte à N docs/segment après ouverture |

Corpus prêts (ignorés par git) : `playground/corpus-kernel-16k.tar.gz`,
`corpus-kernel-10k.tar.gz`, `corpus-kernel-2k.tar.gz`. Le 2k est le bon pour
itérer : ~82 s d'indexation contre ~25 min pour le 10k.

### Hook de test

`window._playground` expose `search`, `numDocs`, `memoryStatus`,
`getActiveIndex`, `importFiles`. Utile :

```bash
curl -s localhost:9877/eval/main -d '{"js":"document.getElementById(\"status\").textContent"}'
curl -s localhost:9877/eval/main -d '{"js":"document.getElementById(\"memory\").textContent"}'
```

### Build WASM

```bash
bash bindings/emscripten/build.sh                      # release, ~3 min
LUCIVY_WASM_DEBUG=1 bash bindings/emscripten/build.sh  # + symboles, assertions
```

Le build debug est **indispensable pour lire une pile d'allocation** : sans
lui, un échec ne dit que « allocation of N bytes failed ».

---

## 4. Mesurer les octets qu'une requête touche vraiment

```bash
TOUCHED_INDEX=/tmp/lucivy_parity_native \
TOUCHED_WARM=netdev \
TOUCHED_QUERY=kmalloc \
cargo test --release -p lucivy-core --test test_touched_bytes -- --ignored --nocapture
```

Protocole : ouvrir l'index, **chauffer avec un terme différent**, évincer tous
les fichiers du cache de pages (`posix_fadvise(DONTNEED)`), lancer la requête,
compter les pages résidentes (`mincore`) par extension.

Deux pièges appris :
- chauffer avec **le même** terme fausse tout (les pages sont déjà là) ;
- l'éviction ne peut pas libérer ce qu'un handle mappe encore, d'où l'ordre
  ouvrir → chauffer → évincer → mesurer.

Le test **affirme** aussi que `list_files_for` ne nomme aucun fichier absent.

---

## 5. Diagnostiquer une panne en navigateur

L'ordre qui a fonctionné aujourd'hui.

### 5.1 L'anneau de diagnostic — la seule trace qui survit à un thread mort

Le stderr d'un pthread est proxié vers le thread principal, que l'abandon
devance : **le message se perd**. Un anneau en SharedArrayBuffer est écrit
avant l'abandon, et la page le relaie vers la console et `window._ringLog`.

```bash
curl -s localhost:9877/eval/main \
  -d '{"js":"(window._ringLog||[]).filter(m=>m.indexOf(\"[alloc]\")===0).slice(0,20).join(\"\\n\")"}'
```

C'est ce qui a donné la pile exacte de l'échec à 384 Mo
(`BuildFstV3Node::execute`). **Sans ça on ne sait que « allocation failed ».**

### 5.2 Le graphe d'attente

Quand ça bloque, `playground/diag.log` contient périodiquement :

```
indexer_3 --[indexer_flush_finalize (0/1)]--> waiting (1415.1s)
```

Un acteur qui attend depuis 1 400 s ne rame pas : il est mort ou son signal
s'est perdu. `LUCIVY_WAIT_WARN_SECS` règle le seuil d'alerte.

### 5.3 Les traces utiles dans `diag.log`

| préfixe | ce qu'il dit |
|---|---|
| `[fs] load <nom> <N> B <ms>` | chaque matérialisation de fichier entier |
| `[search] prescan batch i/n` | découpage par lots et fichiers libérés |
| `[prescan] N segments, scatter DAG wall …` | profil détaillé, avec `peak concurrency` |
| `[bytes] shard i: cached/computed` | comptage des tailles, et sa fiabilité |
| `[preload] N fichiers, M Mo en Xms` | chargement anticipé |
| `[finalize] field F: sfx build Xms` | coût du constructeur de FST par segment |
| `[scheduler] starting with N threads (source)` | threads **et d'où vient le nombre** |
| `[alloc] / [panic]` | échec d'allocation ou panique, avec pile |

Agrégations utiles :

```bash
# volume chargé par extension pour une requête
grep '\[fs\] load' playground/diag.log | awk '{split($3,a,"."); e=a[length(a)]; n[e]++; s[e]+=$4} END {for(k in n) printf "%-14s %4d fichiers %8.1f Mo\n", k, n[k], s[k]/1e6}'

# taille des segments finalisés
grep -o 'finalize() [0-9]* docs' playground/diag.log | awk '{s+=$2;n++} END {print n" segments, "int(s/n)" docs/segment"}'
```

---

## 6. Les boutons (variables d'environnement)

| variable | défaut | rôle |
|---|---|---|
| `LUCIVY_VERBOSE` | — | traces moteur |
| `V3_PROFILE` | — | profil des briques SFX |
| `V3_DEBUG_QUERY=<texte>` | — | trace détaillée d'**une** requête |
| `LUCIVY_WRITER_HEAP` | 200 Mo / **15 Mo wasm** | tas de l'écrivain, **total** réparti entre les threads |
| `LUCIVY_WRITER_THREADS` | auto / **1 wasm** | threads d'écriture ; le tas par défaut suit |
| `LUCIVY_SFX_HEAP` | 1 Go / **128 Mo wasm** | ce que les collecteurs SFX tiennent avant de couper un segment — **global, divisé par les threads** |
| `LUCIVY_MERGE_CONCURRENCY` | ∞ / **1 wasm** | fusions simultanées |
| `LUCIVY_MAX_PENDING_FINALIZE` | 4 / **1 wasm** | segments en construction en plus de celui qu'on remplit ; au-delà l'indexeur attend |
| `LUCIVY_SCHEDULER_THREADS` | `available_parallelism()` / **4 wasm** | pool luciole |
| `LUCIVY_FILE_CACHE_BYTES` | 4 Go / **768 Mo wasm** | cache de fichiers entiers ; s'il est posé, il **fige** le budget |
| `LUCIVY_RAM_INDEX_MAX` | ∞ / **2 Go wasm** | au-delà, l'index est streamé |
| `LUCIVY_SHARD_BATCH_BYTES` | ∞ / **1 Go wasm** | taille d'un lot de shards |
| `LUCIVY_MIN_SUFFIX_LEN` | | longueur minimale de suffixe indexée |

**Reproduire la forme WASM en natif** (c'est comme ça qu'on itère vite) :

```bash
LUCIVY_WRITER_HEAP=15000000 LUCIVY_WRITER_THREADS=1 LUCIVY_SFX_HEAP=134217728 …
```

---

## 7. Benchs

```bash
cargo test --release -p lucivy-core --test bench_sharding -- --ignored --nocapture
cargo test --release -p lucivy-core --test bench_vs_tantivy -- --ignored --nocapture
cargo test --release -p lucivy-core --test bench_contains -- --ignored --nocapture
```

Vérité terrain historique : `docs/BENCHMARKS.md`.

---

## 8. Chiffres de référence (25 août, à comparer après un changement)

**Natif, 10 000 fichiers kernel, compacté** (SFP3 avec `headers_len`,
soir du 25) — 2 305 Mo, indexation 25,7 s, panel 1 664 ms soit
**79 ms/requête (médiane 49 ms)**. Avant la correction du soir, même
protocole : 1 943 ms, 93 ms/requête, médiane 59.

**Navigateur, même corpus, tout en RAM (`?rammax=3000` obligatoire : le
défaut est 2 Go)** — 2 600 Mo, panel 567 ms/requête (médiane 281 ms),
preload 837 fichiers / 2 600 Mo en 2,5 s. **Mesuré avec le SFP3 de
l'après-midi** : l'index OPFS ne se lit plus depuis la correction et doit
être reconstruit avant toute comparaison (05 §5.0) ; le chiffre à battre
est celui-là.

**Natif, 15 440 fichiers, compacté** — 3 392 Mo, soit 220 Ko/document.

**Débit OPFS** — ~3 ms de fixe par ouverture, ~1,15 Go/s asymptotique
(mesuré sur 23 105 chargements).

**Octets touchés par requête** (natif, mmap) — `kmalloc` 853 Mo sur 3 557
(24 %), `zzqqxxwwvv` 70 Mo (2 %).

---

## 9. Les pièges qui ont coûté du temps

- **Le test de parité efface `/tmp/lucivy_parity_native`.** Un petit run
  détruit le gros index.
- **`| tail` tronque au mauvais endroit.** Rediriger puis grepper.
- **Chauffer avec le même terme** invalide `test_touched_bytes`.
- **Le build WASM sans `LUCIVY_WASM_DEBUG=1`** ne donne aucune pile.
- **Une indexation navigateur doit être relancée après tout changement
  d'écriture** — une régression est restée invisible quinze heures parce que
  seules des requêtes étaient rejouées.
- **Le worker ne peut pas ouvrir un index que la page a déjà ouvert** (verrou
  d'écrivain) : utiliser le runner côté page (`parity_run.js`), pas
  `parity_worker.js`.
- **Le montage OPFS échoue quelques secondes après un rechargement** (les
  handles du worker précédent) : `ensure_opfs_mounted` réessaie à chaque point
  d'entrée.
- **`usize` fait 32 bits en wasm32.** Toute somme d'octets doit être en `u64`.
