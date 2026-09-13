# Knowledge dump — 13 septembre 2026

*Autonome : comment lancer les tests, le playground, les bancs, le comparatif, et
comment on publie. Les chiffres sont dans `03-rapport-session.md`, l'architecture
dans `04-architecture.md`.*

## 0. Règles de la maison

- `export PATH="$HOME/.cargo/bin:$PATH"` avant tout `cargo`.
- **Toujours rediriger dans un fichier puis filtrer** (`> /tmp/x.txt 2>&1`),
  jamais `| tail` : une sortie tronquée fait conclure à tort.
- L'outil shell est **zsh** : `--include=*.rs` non quoté est mangé par le glob,
  et une commande stockée dans une variable n'est pas redécoupée. Passer par un
  script bash pour tout ce qui est composé.
- `free -g` avant une construction d'index à l'échelle du noyau ; **jamais deux
  en parallèle** (pic de 13 à 15 Go chacune).
- `/tmp` est vidé à 10 jours : les corpus vivent dans `~/lucivy_bench/`.
- **Publier est une décision de Lucie.** Aucun `cargo publish`, aucun tag sans
  son accord explicite.

## 1. Tests

```bash
cargo test --lib                                  # 1 471
cargo test --lib --no-default-features            # 1 437
cargo test -p lucivy-core --no-fail-fast          # 42 lots
cargo test -p lucivy-cpp                          # 19
cargo clippy --lib -- -D warnings
cargo clippy -p lucivy-core --lib -- -D warnings

# les tests d'une option précise
cargo test --release -p lucivy-core --test test_positions_off -- --nocapture
cargo test --release -p lucivy-core --test test_relaxed_duplicate_spans
cargo test --release -p lucivy-core --test test_snapshot_served
```

**Bindings** (à lancer avant d'ajouter une suite à la CI, et pour vérifier
qu'elle sort en erreur quand elle échoue) :

```bash
(cd bindings/python && source .venv/bin/activate && bash build.sh && python -m pytest tests -q)   # 113
(cd bindings/nodejs && npm run build && node test.mjs && for t in tests/*.mjs; do node "$t"; done)
#   tests/smoke_warnings.mjs attend le chemin ABSOLU du .node en argument
bash bindings/emscripten/build.sh                  # WASM : emsdk + nightly, ~1 min en incrémental
```

## 2. Vérité terrain (le harnais)

`lucivy_core/tests/test_sfx_v3_ground_truth.rs`, test `v3_ground_truth_demo`.
Il **balaie les fichiers** pour établir la vérité, puis compare comptes **et**
spans. Depuis le 13, **un span rendu deux fois compte comme un span en trop**.

```bash
V3_CORPUS=$HOME/lucivy_bench/linux-7.2 V3_MAX_DOCS=1000000 V3_COMMIT_EVERY=10000 \
V3_SFX_VERSION=4 V3_INDEX_DIR=$HOME/lucivy_bench/compare-4.1/dict \
LUCIVY_HIGHLIGHT_SPAN_CAP=0 \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
```

| variable | effet |
|---|---|
| `V3_SFX_VERSION` | 3 = une FST par segment, 4 = dictionnaire partagé |
| `V3_POSITIONS=0` | index sans positions |
| `V3_DERIVED_IN_RAM=1` | dérivés reconstruits à l'ouverture |
| `V3_QUERIES` | `'de:strict,sched:relax,schdule:fz1,x:rx,y:jw1,z:term,w:sw'` (modes : strict, relax, fz1-3, rx, sw, sws, term, terms, jw1, jw2 ; `\s` = espace) |
| `V3_INDEX_DIR` | index réutilisé si la **clé de forme** concorde (`.v3_shape`) ; supprimer le dossier pour reconstruire |
| `V3_DUMP_DOCS=<fichier>` | une ligne JSON par requête avec les chemins trouvés et tous les spans — c'est l'outil pour diffusion deux dispositions |
| `V3_SPANS=0` | ne construit pas les spans (levier de mesure ; le harnais passe toujours un collecteur, donc il signalera des spans manquants) |
| `V3_DIAG_LITERAL=<aiguille>` | imprime chaque match de la phase littérale (position, octets, entrée mot ou morceau) |
| `V3_DIAG_STORED=1` | une ligne par segment sans positions : candidats, documents, Mo relus |

**Piège** : sans corpus, le harnais retombe sur un texte **synthétique** et ne le
crie pas. Vérifier `corpus: N files` et l'absence de `Skipping`.

## 3. Playground

```bash
cd playground && node serve.mjs          # http://localhost:9877
```

| paramètre | effet |
|---|---|
| `?nopos` | index sans positions |
| `?ram` | `derived_in_ram` |
| `?nodict` | une FST par segment |
| `?corpus=corpus-kernel-10k.tar.gz` | corpus servi à côté de la page (2k, 10k, 16k, mdn, linux-2.6.0…) |
| `?commit=N`, `?commitmb=M` | cadence des commits |
| `?verbose` | traces dans `diag.log` |

**Un seul onglet qui indexe à la fois** : deux onglets partagent le répertoire
OPFS et échouent au commit.

**Rejouer le panel de parité** (21 requêtes) dans la page :

```js
eval(await (await fetch('parity_run.js')).text());   // puis window._parityResult
fetch('/log', {method:'POST', body:'MARQUEUR ' + window._parityResult + '\n'});
```

puis reprendre la ligne `MARQUEUR` de `playground/diag.log` et comparer deux
rapports : `python3 playground/parity_diff.py a.json b.json`.

**Piège** : `curl localhost:9877/eval/main` est servi par **n'importe quelle**
page ouverte du playground — elle a rendu `undefined` un jour où un autre onglet
répondait. Le `POST /log` depuis la page elle-même est fiable.

**Tester au-delà de 2 000 documents** : c'était le palier où les fusions
cassaient. Un test navigateur qui s'arrête à 2 000 ne prouve rien.

## 4. Bancs et comparatif

### Le comparatif à trois moteurs

```bash
benches/compare_engines.sh kernel ~/lucivy_bench/compare-4.1
```

- **Corpus épinglé** : `torvalds/linux` v7.2, commit `8d3ae59288f1`. Le script le
  clone s'il manque, le remet sur ce commit s'il a dérivé, écrit l'empreinte dans
  `corpus.id` / `corpus.json` et **jette les index en cache si elle change**.
  Surcharge : `KERNEL_URL`, `KERNEL_REF`, `KERNEL_SHA`, `LUCIVY_BENCH_DIR`.
- **Quatre dispositions lucivy** : `dict`, `dict-ram`, `dict-nopos`, `v3`,
  chacune passant le panel de vérité terrain.
- **Elasticsearch** (optionnel, `ES_URL`) : ses index portent l'empreinte du
  corpus dans `_meta` et sont **réutilisés** quand elle concorde — une reprise ne
  coûte alors que les requêtes. Le conteneur :

```bash
docker run -d --name lucivy-es -p 9200:9200 -e discovery.type=single-node \
  -e xpack.security.enabled=false -e ES_JAVA_OPTS="-Xms8g -Xmx8g" \
  docker.elastic.co/elasticsearch/elasticsearch:8.19.0
```

- **Rejouer Elasticsearch seul** (rapide, index réutilisés) puis régénérer :

```bash
W=~/lucivy_bench/compare-4.1
ES_CORPUS_ID=$(cat $W/corpus.id) python3 benches/compare_elasticsearch.py ~/lucivy_bench/linux-7.2
cp /tmp/es_compare.json $W/elasticsearch.json
python3 benches/compare_engines_report.py $W > $W/compare_engines.md
```

- **Chaque ligne porte les deux temps d'Elasticsearch** : son `took` pour les
  documents, et ce que `highlight` ajoute pour marquer les 200 premiers — sans
  quoi on compare son « documents seuls » à notre « documents et tous les spans ».

### Mesurer à froid

```bash
python3 benches/cold_cache.py        # posix_fadvise(DONTNEED), sans droits root
```

L'éviction a lieu **entre les runs**, donc seule la première requête d'un run est
vraiment à froid. Elasticsearch ne se refroidit pas ainsi (conteneur, JVM) : ne
pas prétendre à une symétrie.

### Autres bancs

```bash
cargo test --release -p lucivy-core --test bench_sharding -- --ignored --nocapture   # 90k docs
cargo test --release -p lucivy-core --test compare_tantivy compare_tantivy -- --ignored --nocapture
```

## 5. Publier

**Le chemin, tel qu'il a servi pour 4.1.0 :**

1. Travailler sur une branche (`v4.1`). Chaque push y lance `ci.yml`.
2. Ouvrir une **PR vers `main`** : `ci.yml` **et** `build.yml` (5 plateformes,
   sdist, WASM) tournent ; rien ne peut publier depuis une PR.
3. Quand tout est vert : **avance rapide** `git push origin v4.1:main` — la tête
   de `main` est alors exactement le commit validé, et la PR se ferme comme
   fusionnée.
4. Dater l'en-tête du CHANGELOG.
5. **Le tag** (décision de Lucie), annoté, sur ce commit :

```bash
gh auth switch -u L-Defraiteur          # compte personnel, jamais celui du travail
git tag -a v4.1.0 <sha> -m "lucivy 4.1.0 — …"
git push origin v4.1.0
```

6. `release.yml` rejoue `ci.yml` et `build.yml`, publie **PyPI → npm → wasm →
   crates.io** (crates en dernier : une version n'y revient jamais), puis attache
   les artefacts à la release.

**Garde-fous** : `PUBLISH_ENABLED=true` (variable de dépôt) et l'environnement
`release`, qui n'a **aucun réviseur requis** — un tag poussé publie seul.
PyPI et npm passent par le *trusted publishing* (OIDC), lié au **nom du fichier**
`release.yml` : ne pas déplacer les jobs de publication ailleurs.

**Vérifier après coup :**

```bash
curl -s https://pypi.org/pypi/lucivy/json | python3 -c "import json,sys;print(json.load(sys.stdin)['info']['version'])"
npm view lucivy version; npm view lucivy-wasm version
for c in ld-lucivy lucivy-core luciole lucistore sparse-vector; do
  curl -s -A x "https://crates.io/api/v1/crates/$c" | python3 -c "import json,sys;print(json.load(sys.stdin)['crate']['max_version'])"
done
```

**Piège de CI** : `cancel-in-progress` fait qu'un push annule le run du
précédent. Sur une rafale de commits, **seul le dernier reçoit un verdict** —
regrouper avant de pousser, ou restreindre l'annulation aux pull requests.

## 6. Suivre la CI sans sonder

```bash
gh api "/repos/L-Defraiteur/lucivy/actions/runs?head_sha=$(git rev-parse HEAD)&per_page=10" \
  --jq '.workflow_runs[] | "\(.name) | \(.event) \(.head_branch) | \(.status) \(.conclusion // "")"'
gh run view <id> --log-failed > /tmp/ci.log 2>&1     # puis grep
```

Préférer `gh api` à `curl` : l'API anonyme est limitée à 60 requêtes par heure,
ce qu'une surveillance longue dépasse.
