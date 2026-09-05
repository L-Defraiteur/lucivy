# Knowledge dump — baselines, tests, outils, pour la session suivante (fin du 5 septembre)

Tout ce qu'il faut pour reprendre la branche `v4` sans l'historique.
Remplace [03](03-knowledge-dump-baselines-tests-outils.md) (état du soir,
avant les postings sans octets) ; complète
`docs/28-08-2026/09-knowledge-dump-tests-benchs-publication.md`
(publication, corpus, pièges de vérification), toujours valable.

Toujours : `export PATH="$HOME/.cargo/bin:$PATH"` ; sortie dans un fichier
puis `grep`, jamais `| tail` ; **le shell de l'outil est `fish`** (voir
§10) ; pas de `pip` sur la machine (`uv` oui) ; pas de `/usr/bin/time` ni de
`perf`.

---

## 1. État du dépôt

- **`v4`** = la branche de travail, poussée (`origin/v4`), **4.0.0 non
  publié** ; `main` = `8301b55`, trois commits après `v3.0.8`. Tag
  **`stable-avant-fuzzy-fenetres`** (= `137b03b`) : point de retour posé
  avant l'essai sur la fuzzy ; les tags `v*` déclenchent `release.yml`
  (construction toujours, publication si `PUBLISH_ENABLED == 'true'`,
  environnement `release` sans réviseur) — **ne jamais pousser `v4.0.0`
  sans le feu vert de Lucie, et vérifier la variable avant**.
- Ne jamais lancer `gh` sans son accord (compte de travail) ; `git push
  origin v4` autorisé ; jamais de `cargo publish` sans son feu vert ;
  commits en français, sans trailer.

---

## 2. Les corpus

| chemin | contenu | usage |
|---|---|---|
| `/tmp/lucivy-cmp` | 10 000 fichiers du noyau, 65 Mo | la référence du protocole |
| `/tmp/lucivy-cmp-90k` | 93 983 fichiers, 898 Mo (dont `Documentation/translations/zh_CN`) | 30 000 premiers pour les A/B, entier pour le noyau |
| `lucivy_core/tests/fixtures/index-3.0.8/` | index du wheel 3.0.8, 18 documents, réponses à 14 requêtes | `test_compat_308` (commité) |

---

## 3. Construire un index de référence et lancer les panels

Harnais : `lucivy_core/tests/test_sfx_v3_ground_truth.rs`, test ignoré
`v3_ground_truth_demo` ; il réutilise `V3_INDEX_DIR` si `.v3_shape` n'a pas
changé (corpus, nombre de fichiers, `V3_COMMIT_EVERY`, `V3_SFX_VERSION`,
`V3_DERIVED_IN_RAM`).

```bash
T="cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture"
V3_CORPUS=/tmp/lucivy-cmp V3_MAX_DOCS=10000 V3_INDEX_DIR=/chemin/idx $T          # référence 10 000 (~8 s v3, ~19 s dict)
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=30000 V3_COMMIT_EVERY=2000 V3_INDEX_DIR=/chemin/idx30k $T   # A/B de temps
V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=100000 V3_COMMIT_EVERY=10000 V3_INDEX_DIR=/chemin/idx90k $T # noyau (~65 s v3, ~255 s dict)
# V3_SFX_VERSION=4 : dictionnaire ; V3_DERIVED_IN_RAM=1 : sans les trois dérivés ; V3_PROFILE=1 : le profil ; LUCIVY_VERBOSE=1 : [derived], [reader], [merge]
grep -o '[0-9]* pass, [0-9]* fail' out.txt                              # 9 pass, 0 fail
grep -E "^[a-z_]+.*(strict|relax|term|sw|fz1|fz2|rx|jw1) +[0-9—]+ +[0-9]+ +(OK|n/a)" out.txt | sed -E 's/ +/ /g' | awk '{print $1,$2,$6}'
```

Les autres vérités du fichier, **non ignorées** (`--ignored` les filtre :
« running 0 tests ») : `v3_ground_truth_contains` (15), `v3_ground_truth_
coherence` (31) — `<nom> -- --exact --nocapture`, même `V3_INDEX_DIR`.
Piège RAM : jamais tout le fichier avec `V3_MAX_DOCS=100000` ; un test à la
fois ; `free -g` avant ; jamais deux constructions 90k en parallèle.

---

## 4. Mesurer les tailles

`python3 benches/scan_index_size.py /chemin/idx > scan.txt` ; le bloc
`=== TOTALS` donne par type de fichier `{'file': octets, …}` ; il lit
`SFP5`, `WSP5`, `PMP4`, `WMP3` et les anciens. Additionner les `file` :
un petit script Python (voir le scratchpad). `du -sm` compte aussi tantivy.

Baselines (fichiers SFX / `du`) : 10 000 v3 420,9 Mo / 455 ; dictionnaire
290,8 / **345** ; 30 000 v3 1 352,7 / 1 445, dictionnaire 977,9 / **1 128**,
avec `derived_in_ram` **829** ; noyau dictionnaire 4 259,1 / **4 938**, avec
`derived_in_ram` **3 344** (`main` 3.0.x : 18 057 ; texte : 857 Mo).

---

## 5. Les A/B de temps

Au même binaire, entre deux index, trois passes alternées, min, machine au
repos (script `run-ab-sfp5.sh` du scratchpad : boucle sur les deux index,
puis un Python qui extrait `(\S+)\s+(strict|…)\s+…\s+(?:OK|n/a) \(([0-9.]+)ms
search`). Baselines 30 000 dictionnaire (ms, min/3, index `idx30k-dict5`) :
mutex_lock strict 2,8 · relax 2,4 · spin_lock 2,5 · sched term 4,6 · sched
strict 2,7 · printk 3,7 · fz1 9,3 · fz2 141,4 · rx 14,8 · jw1 12,0. Noyau
dictionnaire (une passe) : 11 / 11 / 12 / 20 / 11 / 13 / 46-50 / 755-830 /
223-238 / 71-79. Profil (`V3_PROFILE=1`) : `stage totals`, `place_spans`,
`fuzzy: resolve … window … dp … | hits= regions= windows= spans=`.

---

## 6. Les tests

```bash
cargo test --lib                                              # ld-lucivy : 1 461 verts, 22 ignorés
cargo test --release -p lucivy-core --test test_compat_308    # v4 lit la 3.0.8, convertit, compacte, rouvre
cargo test --release -p lucivy-core --test test_derived_in_ram
cargo test --release -p lucivy-core --test test_dictionary_index
cargo test --release -p lucivy-core --test test_sfx_v3_pipeline --test test_federated_search --test test_filtered_search_truth --test test_luce_v3_roundtrip --test test_fuzzy_ground_truth --test test_regex_ground_truth
cargo test -p lucivy-cpp                                      # 17 tests, dont les options en objet schéma
cargo check -p lucivy-napi -p lucivy-cpp -p lucivy-fts -p lucivy   # (pas de maturin/napi ici : tests bindings en CI, node tests/*.mjs et pytest à la main)
```

Tests ignorés d'outillage (`cargo test --release --lib <nom> -- --ignored
--nocapture`) : `byte_spans_are_derivable` (`LUCIVY_POSTINGS_DIR` : le
`byte_at` du `PMP4` contre une somme cumulée indépendante, chaque posting
de mot contre les chunks sous lui, les spans stockés d'un ancien index ;
0 partout attendu), `postings_without_byte_spans` (tailles par layout),
`derived_files_match_the_index` (`LUCIVY_DERIVED_DIR` : les trois dérivés
rebâtis contre les fichiers, moins le pied du répertoire), `dictionary_
compact::compaction_of_an_index_on_disk` (`LUCIVY_DICT_BENCH_*`).

---

## 7. Le navigateur

`bash bindings/emscripten/build.sh` (~1 min, estampille `index.html` depuis
`Cargo.toml`), `cd playground && node serve.mjs 9877`. URL : `?dict`,
`?dict&ram`, `?corpus=corpus-kernel-16k.tar.gz&dict&commit=1000`,
`&merges=N`, `&verbose`, `?open=user_index` ; un seul onglet qui indexe.
**Les corpus du terminal** (`index mdn`, `index linux`, `index go`,
`godot`, `typescript`, `postgres`, `cpython`, `redis`, `git`, `curl`,
`sqlite`, `nginx`) : `playground/corpora.json` les décrit, `python3
playground/tools/build_corpus.py all` (ou des noms ; `--dry-run` compte
seulement ; cache `~/.cache/lucivy-corpora`) fabrique les
`corpus-<nom>.tar.gz` à côté de la page et écrit `stats` dans le manifeste ;
`pages.yml` fait pareil au déploiement ; git les ignore. Le filtre est
celui de la page (`TEXT_EXTENSIONS`, ≤ 100 000 octets, pas de NUL dans les
512 premiers) — **les deux listes d'extensions doivent rester identiques**
(`index.html` et `build_corpus.py`). Le nombre de documents indexés doit
égaler `stats.files` : s'il est plus petit, le lecteur tar de la page perd
des entrées (noms longs : ustar `prefix`, GNU `L`, PAX `x`).
Depuis Chrome (outils `claude-in-chrome`, onglet du playground) : le plus
fiable est `javascript_tool` avec `window._playground.search(q, {limit,
highlights: true, fields: true})` puis **découper le texte en octets UTF-8**
(`TextEncoder`) aux offsets rendus — `slice` sur la chaîne JS ment dès le
premier caractère non ASCII. Attendre `numDocs() > 0` en boucle avant de
chercher. Les règles du serveur de debug (`/eval/main`) sont dans
[03](03-knowledge-dump-baselines-tests-outils.md) §7 bis.

Piloter le terminal du playground depuis `javascript_tool` : le script de
la page est un module, ses fonctions ne sont pas atteignables ; attendre
`document.querySelector('.term-input')` (le prompt n'existe qu'après la
démo, ~17 s ; pas avec `?nodemo`), poser `value` (`index linux`, `open
linux`, `drop linux`, `index mdn`…) et envoyer un `KeyboardEvent`
`keydown` `Enter`. Lire les réponses dans
`document.querySelector('.prompt').closest('[id]').innerText` (« reopened
in X s », « its N index files … loaded in X s », « N hits … X ms ») —
**filtrer et assainir la sortie** (garder `[A-Za-z0-9 .,:;()_-]`) : l'outil
bloque toute réponse qui ressemble à une query string ou à un cookie, et
le `innerText` de la page en contient. Un `await` de plus de 45 s dans
l'outil échoue (« timed out ») : attendre par tranches de 30 s. Pic
mémoire : `_playground.memoryStatus().heap_bytes` (taille de la mémoire
linéaire, ne redescend jamais) avant et après. Taille OPFS : parcourir
`navigator.storage.getDirectory()` → `lucivy/<path>/shard_*`.

Baselines navigateur (dictionnaire, 4 shards, `index kernel` 15 429
fichiers / `index mdn` 14 629 pages, [04](04-progression-et-a-faire.md)
§2 ter) : noyau 41 s, OPFS 1 571 Mo, pic 3 335 Mo, réouverture 1,7 s +
`preload` 3,9 s, mémoire 2 803 Mo ouvert ; avec `?ram` 40 s, 1 159 Mo,
**pic 3 859**, 2,7 + 2,5 s, 3 055 Mo. MDN 14 s, 478 Mo, pic 1 646,
0,8 + 2,2 s ; avec `?ram` 14 s, 369 Mo, pic 1 906, 1,3 + 1,5 s. Panel
noyau après ouverture : strict 71-80 ms, relâché 20-23, fuzzy 1 43, regex
164-172. Corpus de la vitrine (indexation / index / pic) : Linux 2.6.0
28 s / 1 087 Mo / 3 391 ; MDN 14 / 475 / 1 650 ; Go 19 / 686 / 2 291 ;
Godot 19 / 809 / 3 323 ; TypeScript 33 / 462 / 1 522 ; PostgreSQL 10 /
483 / 2 943 ; CPython 10 / 466 / 2 811 ; Git 5 / 242 ; curl 3 / 110 ;
Redis 2 / 115 ; SQLite 2 / 97 ; nginx 1 / 32 ([04](04-progression-et-a-faire.md)
§2 bis, panels dans `browser-ram.md` du scratchpad).

---

## 8. La fixture 3.0.8

`lucivy_core/tests/fixtures/index-3.0.8/{single,sharded,panel-3.0.8.json,
build.py,README.md}`. Rebâtir seulement pour une raison : `uv venv v &&
uv pip install --python v/bin/python lucivy==3.0.8`, puis `v/bin/python
build.py <dossier>` (lit `/tmp/lucivy-cmp` et `/tmp/lucivy-cmp-90k`). Le
wheel expose `Index.create(path, fields, shards=)`, `add(doc_id, **fields)`,
`commit()`, `wait_merges_quiet()`, `search(dict, limit=, highlights=)` →
`SearchResult(doc_id, score, highlights={"content": [(s, e)]})`.

---

## 9. Le scratchpad de la session (à recréer sinon)

`/tmp/claude-1000/-home-lucied-git-workspaces-lucivy/de715c8d-…/scratchpad/` :
`idx10k-v3-sfp5`, `idx10k-dict-sfp5` (**références 10 000 au format
courant**), `idx30k-dict5`, `idx30k-v3-sfp5` (**30 000 au format courant**),
`idx90k-dict-sfp5` (noyau), `idx30k-dict-ram`, `idx90k-dict-ram` (avec
`derived_in_ram`), `idx30k-dict3` / `idx30k-dict4` / `idx30k-v7` /
`idx10k-*-pmp4` (anciens layouts, pour la compatibilité), `idx90k-dict2`,
`idx90k-v8`, `venv308` (le wheel 3.0.8), `build-fixture-308.py`, les
scripts `run-step1.sh`, `run-step3*.sh`, `run-derived*.sh`, `run-ab-sfp5.sh`,
`run-fz.sh`, `run-bump.sh`, `run-opt.sh` et leurs sorties (`step3*.txt`,
`derived*.txt`, `absfp5*.txt`, `fz-*.txt`). Rien d'irremplaçable.

---

## 10. Pièges rencontrés dans la soirée

- **L'outil Bash tourne sous `fish`** : `S=...; cmd $S` ne marche pas
  (`= not found`) et `set S ...; cmd $S` perd la variable après le `;` ;
  `for f in $files` colle tous les noms en un seul argument ; `--include=*.rs`
  veut des guillemets ; `echo ====` aussi. Écrire les chemins en entier, ou
  passer par un script `#!/bin/bash`.
- Un fichier géré par le répertoire finit par un **pied** (~93 octets, CRC
  et version) que `open_read` cache : comparer un fichier rebâti au disque
  demande `Footer::extract_footer`.
- Un test qui ouvre le `.posmap` sans le `.gmap` reçoit des ordinaux
  **locaux** : la méta attend le global (89 % de « désaccords » qui étaient
  ceux du test).
- Pour un dernier jeton qui est un mot, le texte commence à son **premier**
  chunk ; un seuil converti d'octets en positions garde sa valeur (une
  position ≥ 1 octet, sur-ensemble), mais un **jeu** de séparateurs non.
- `--ignored` filtre les tests non ignorés ; `cargo test --test X a b` = un
  seul filtre ; le nom d'un paquet du workspace se vérifie avant `-p`
  (`lucivy-wasm` n'existe pas).
- `pkill -f motif` / `pgrep -f motif` tuent le shell qui porte le motif :
  `pgrep -f 'run-step[3].sh'` (le crochet casse l'auto-correspondance).
- Une fixture 3.0.8 pèse ×45 le texte : 60 documents faisaient 36 Mo.
- La première requête d'un index à dérivés paresseux mentait sur le moteur
  (288 ms puis 3 ms) : Lucie a tranché, tout à l'ouverture.
