# Knowledge dump — 11 septembre 2026 (branche `v4.1`)

## Corpus et index de mesure

- Noyau : `~/lucivy_bench/linux-7.2` (`git clone --depth=1 --branch v7.2`,
  commit `8d3ae59`). Liens : `ln -sfn ~/lucivy_bench/linux-7.2 /tmp/lucivy-cmp`
  et `/tmp/lucivy-cmp-90k` (à recréer si `/tmp` les a nettoyés).
- Index de mesure : `~/lucivy_bench/idx-{10k,30k,kernel}-pos{1,0}` (1 = défaut,
  0 = sans positions). Réutilisés par le harnais si la clé de forme concorde
  (`.v3_shape`) ; supprimer le répertoire pour forcer la reconstruction.
- Vérifier dans la sortie : jamais « Skipping: corpus not found » ni un
  corpus synthétique (« corpus: 300 files » avec chemins `synthetic/`).

## Harnais de vérité terrain (`lucivy_core/tests/test_sfx_v3_ground_truth.rs`)

```bash
V3_CORPUS=/tmp/lucivy-cmp V3_MAX_DOCS=10000 V3_SFX_VERSION=4 \
V3_INDEX_DIR=$HOME/lucivy_bench/idx-10k-pos0 V3_POSITIONS=0 \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
```

- `V3_POSITIONS=0` : index sans positions. `V3_SFX_VERSION=4` : dictionnaire.
- A/B 30 000 : `V3_CORPUS=/tmp/lucivy-cmp-90k V3_MAX_DOCS=30000 V3_COMMIT_EVERY=2000`.
  Noyau : `V3_MAX_DOCS=1000000 V3_COMMIT_EVERY=10000`.
- `V3_QUERIES='de:strict,sched:relax,schdule:fz1,x:rx,y:jw1,z:term,w:sw'`
  (modes : strict, relax, fz1-3, rx, sw, sws, term, terms, jw1, jw2 ; `\s` = espace).
- `LUCIVY_HIGHLIGHT_SPAN_CAP=0` pour `de` (7,9 M spans).
- `V3_DIAG_STORED=1` : une ligne par segment sans positions — candidats,
  documents trouvés, Mo de texte relus, temps de chaque phase.
- Colonne « grep » = la vérité (relecture de tous les fichiers) : c'est elle
  qui prend l'essentiel du temps d'une passe (Jaro-Winkler : 69 s sur le noyau).
- Pas d'option de répétition : relancer N fois, alterner les layouts, prendre
  la médiane. Machine seule pendant une mesure.

## Scripts (dans le scratchpad de la session ; à recopier au besoin)

Toujours via `bash script.sh` (l'outil est zsh). Modèle de mesure avec pic
mémoire (pas de `/usr/bin/time`) :

```bash
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth --no-run
BIN=$(ls -t target/release/deps/test_sfx_v3_ground_truth-* | grep -v '\.d$' | head -1)
V3_…=… $BIN v3_ground_truth_demo --ignored --nocapture > log 2>&1 &
pid=$!; peak=0
while kill -0 $pid 2>/dev/null; do h=$(awk '/VmHWM/{print $2}' /proc/$pid/status); \
  [ -n "$h" ] && [ "$h" -gt "$peak" ] && peak=$h; sleep 0.2; done
echo "peak $((peak/1024)) MB"
```

Composition d'un index par type de fichier : `find DIR -type f -printf '%s %f\n'`
puis agrégat par extension (`dict-*` à part). Taille du texte du corpus :
reproduire le filtre du harnais (exclus `target node_modules .git build
__pycache__ playground`, 0 < taille ≤ 100 000, sans octet nul, UTF-8, non
vide) **en suivant les liens symboliques** (`os.walk(followlinks=True)`).

## Tests

```bash
cargo test --lib                                   # 1 471 ; --no-default-features : 1 437
cargo test -p lucivy-core --no-fail-fast           # tout vert
cargo test -p lucivy-core --test test_positions_off -- --nocapture   # l'option : 4 tests
cargo test -p lucivy-cpp                           # 19, dont schema_object_with_positions_false
cargo clippy --lib -- -D warnings && cargo clippy -p lucivy-core --lib -- -D warnings
(cd bindings/python && source .venv/bin/activate && bash build.sh && python -m pytest tests -q)  # 113
(cd bindings/nodejs && npm run build && node tests/positions.mjs && node tests/v3_api.mjs)
```

Unitaires clés : `sfxpost_v2::docs_only_layout…`, `word_sfxpost::docs_only_layout…`
(`to_docs_only` octet pour octet), `fuzzy_spans::the_long_path…` (Myers, sortie
anticipée), `stored::windowed_jaro…`, `stored::the_predicate_is_the_ground_truths`.

## Où est quoi (sans positions)

- Réglage : `IndexSettings.positions` (`src/index/index_meta.rs`),
  `skips_derived_files()` ; `SchemaConfig.positions` + validation
  (`lucivy_core/src/query.rs`) ; `handle.rs`.
- Formats : `SFP6` (`sfxpost_v2.rs`, `is_docs_only`, `for_each_doc`), `WSP6`
  (`word_sfxpost.rs`, `to_docs_only`). Résolveur : `has_positions`, `for_each_doc`.
- Écriture : `SfxCollectorV3::without_positions` (posé dans `segment_writer.rs`),
  `sfx_dag_v3.rs` (assemblage, `merge_segments_v3`, `merge_segments_dict` :
  positions fictives `0..tf` pour porter les fréquences).
- Requête : `briques/stored.rs` (`contains_prescan`, `fuzzy_prescan`,
  `regex_prescan`, `verify_stored`) branché dans `contains_query_v3.rs`,
  `fuzzy_query_v3.rs`, `regex_query_v3.rs` sur `!pr.has_positions()`.
- Bindings : Python `positions=` (`create`, `create_with_blob_store`), Node
  7ᵉ argument de `Index.create` et `BlobIndexOptions.positions`, C++ et
  emscripten par l'objet schéma (`js/lucivy.d.ts`).

## Pièges
- **Un span en double ne se voit pas dans un ensemble.** Jusqu'au 11 au soir le harnais comparait les
  spans en `HashSet` : 542 doublons de `lock` sur 10 000 fichiers passaient « exact ». Il compte
  maintenant `highlights.len() − ensemble` comme spans en trop (panel et comparaison des formes). Le
  panel de parité du playground compte les spans (longueur des listes) : c'est lui qui l'a montré.

- Un test ou une mesure qui ne trouve pas son corpus retombe en silence sur
  du synthétique : lire la sortie.
- Deux builds du noyau jamais en parallèle ; `free -g` avant.
- Ne jamais rendre les sous-chaînes d'un seul jeton sans relecture ni filtrer
  les pièces par jeton sans preuve et accord de Lucie (exactitude d'abord).

## Tester dans le navigateur (ajouté le 11 au soir)

- Build : `bash bindings/emscripten/build.sh` (source `~/emsdk`, nightly ; ~1 min en incrémental), copie
  dans `playground/pkg/`. Serveur : `cd playground && node serve.mjs` (port 9877).
- Un corpus bâti par `?corpus=corpus-kernel-2k.tar.gz` (ou `-10k`, `-16k`) → `/user_index`, recréé à
  chaque chargement (`lucivy_create` efface le répertoire) ; `&nopos` = sans positions (la console dit
  `[playground] positions: false`, puis `indexed N files in Xs; wasm memory high-water mark …, index … MB`).
- Panel de parité : dans la page, `eval(await (await fetch('parity_run.js')).text())`, puis
  `window._parityResult` (21 requêtes, `parity_panel.json`). Le récupérer **avant** de recharger :
  `curl -s localhost:9877/eval/main -d '{"js":"window._parityResult"}' > rapport.json` — **mais**
  `eval/main` est servi par n'importe quelle page du playground ouverte (un autre onglet a rendu
  `"undefined"`) : plus sûr, la page envoie elle-même son rapport,
  `fetch('/log', {method: 'POST', body: 'MARQUEUR ' + window._parityResult + '\n'})`, puis on reprend la
  ligne `MARQUEUR` de `playground/diag.log`. Comparer deux
  rapports : `python3 playground/parity_diff.py a.json b.json` (comptes, top-10, scores à 1e-4, nombre de
  spans ; « TIE » = ex æquo ordonnés autrement).
- Vérité relâchée d'un document en JS : `window._playground.userFile(docId).content`, garder
  `[0-9A-Za-z]` en minuscules avec l'offset source de chaque caractère, occurrences chevauchantes.
- Reproduire en natif sans Rust : le binding Node construit (`bindings/nodejs/index.js`), un script
  `.mjs` hors dépôt (`~/lucivy_bench/scratch-positions/`) ; `V3_DIAG_LITERAL=<aiguille>` imprime chaque
  match de la phase littérale (position, octets, entrée mot ou morceau). Le paquet publié se teste de même
  (`npm install lucivy@4.0.2` dans un dossier jetable).

## CI (depuis le 11 au soir)

- `ci.yml` : push sur `main` et `v…`, PR vers `main`, appelé par `release.yml`. `build.yml` : PR vers `main`, push
  de `main` touchant aux bindings, à la main, appelé par `release.yml`. `release.yml` : tag `v*` (ou à la main,
  `publish` décoché = bâtir sans publier).
- Lire l'état sans toucher au compte `gh` : `curl -s "https://api.github.com/repos/L-Defraiteur/lucivy/actions/runs?branch=v4.1&per_page=10"`.
  Ouvrir une PR ou relancer : `gh auth switch -u L-Defraiteur` d'abord (le compte actif par défaut est le pro).
- Avant d'ajouter une suite à la CI, la lancer en local et vérifier qu'elle sort en erreur quand elle échoue
  (`tests/smoke_warnings.mjs` attend le chemin absolu du `.node` en argument).
