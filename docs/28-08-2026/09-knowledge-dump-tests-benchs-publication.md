# Tout ce qui se lance : tests, bancs, publication

Écrit pour être lu seul. Chaque commande a été exécutée telle quelle le
28 août 2026.

**Préalable à toute session** : `export PATH="$HOME/.cargo/bin:$PATH"`.

**Deux règles de sortie, apprises à la dure** :

```bash
cargo test … > /tmp/t.txt 2>&1      # jamais `| tail` : la sortie est énorme
grep -n "^---- \|panicked at\|fatal runtime\|FAILED" /tmp/t.txt | head
```

Le `fatal runtime` compte autant que le `FAILED` : un dépassement de pile
abrège la suite **sans qu'aucun test ne soit marqué en échec**.

---

## 1. Les tests

### Ce que la CI lance, à reproduire avant un push

```bash
cargo test --lib                        # 1435 passés, 0 échec, 16 ignorés
cargo test --lib --no-default-features  # 1401 passés
cargo clippy --lib -- -D warnings
cargo clippy -p lucivy-core -p lucistore -p luciole -p sparse-vector --lib -- -D warnings

V3_CORPUS=. cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth \
    v3_ground_truth_contains -- --nocapture        # le job « ground-truth »
```

`ci.yml` lance aussi un **audit `thread::spawn`** (un appel non gardé hors
test fait échouer la build, il casserait le WASM) et la suite Python.

### La suite complète

```bash
V3_CORPUS=. cargo test -p lucivy-core --no-fail-fast   # 183 passés
cargo test -p sparse-vector                            # 79
cargo test -p luciole --lib                            # 169
cargo test -p lucistore --lib                          # 43
cargo test -p lucivy-cpp                               # 16
```

`cargo test --workspace` lance aussi les doctests : **trois échouent**, et
c'est antérieur (vérifié au tag `v3.0.5`). La CI ne les lance pas.

### Les bindings, comme la CI

```bash
cd bindings/python && bash build.sh && .venv/bin/python -m pytest tests
cd bindings/nodejs && npm run build && node test.mjs
                      node tests/v3_api.mjs && node tests/blob_store.mjs
                      node tests/smoke_warnings.mjs "$PWD/lucivy.node"
```

### Le navigateur

```bash
cd playground && node serve.mjs          # http://localhost:9877
npm install --no-save playwright         # sans télécharger de navigateur :
                                         # PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
node test_delta_sync.mjs                 # synchronisation incrémentale, headless
node test-playground.mjs                 # démarrage + recherche
```

`test_delta_sync.mjs` utilise le **Chrome du système** (`channel: 'chrome'`) ;
`PLAYWRIGHT_BROWSER=chromium` bascule sur celui de playwright. Il échoue si le
delta n'est **pas** plus petit que le snapshot — il ne peut donc pas passer en
livrant tout en douce.

### Ce que chaque test de vérité verrouille

| fichier | ce qu'il prouve |
|---|---|
| `test_sfx_v3_ground_truth.rs` | comptes **et** spans contre un grep octet par octet |
| `fuzzy_finds_an_occurrence_that_straddles_tokens` | le bug de rappel de 3.0.2-3.0.6 ; échoue sous `V3_FUZZY_MODE=pivot` |
| `test_filtered_search_truth.rs` | filtré = non filtré ∩ autorisés, documents *et* highlights |
| `test_federated_search.rs` | union de deux nœuds = index unique, **scores égaux** |
| `test_truncation_flag.rs` | une requête plafonnée le **dit** |
| `test_fuzzy_tiers.rs` | le palier fuzzy = distance d'édition vérifiée |
| `test_segments.rs` (sparse) | N segments = un index compacté |
| `test_filter_truth.rs` (sparse) | filtré = non filtré ∩ autorisés, deux chemins |
| `test_global_dims.rs` (sparse) | la dimension est le token global |
| `test_mmap_durability.rs` (sparse) | tronqué refusé, octet retourné attrapé par le CRC |
| `test_acid_blob_v3.rs` | le blob store est la source de vérité |

---

## 2. Les bancs — tous `#[ignore]`

**Avant de croire un chiffre** : `uptime` (charge basse), et **deux passes**.
Trois chiffres faux ont été publiés faute de cette discipline : ×5,3 sur un
corpus synthétique à poids plats, ×7,8 sur une machine saturée, et
l'explication inventée entre les deux.

### Le panel de démonstration — celui qui vérifie

C'est le seul qui compare ce qu'il chronomètre. Chaque ligne oppose documents
*et* spans à une lecture brute du disque, et affiche le temps de ce scan.

```bash
git clone --depth=1 https://github.com/torvalds/linux /tmp/linux-bench
V3_CORPUS=/tmp/linux-bench cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
```

`V3_MAX_DOCS=n` plafonne, `V3_QUERIES` remplace le panel,
`V3_INDEX_DIR=/chemin` persiste l'index (pour en mesurer la taille).

Format de `V3_QUERIES` : `valeur:mode`, séparés par des virgules. Modes :
`strict`, `relax`, `fz1`-`fz3`, `jw1`/`jw2` (**chronométré, non vérifié**),
`rx`, `sw`/`sws` (début de token), `term`/`terms` (mot entier). `\s` vaut
l'espace. Exemple :

```bash
V3_QUERIES="spin_lock:strict,spinlock:relax,schdule:fz1,return\s-ENOMEM:strict"
```

### Le banc de comparaison, monté le 28 août

Corpus **matérialisé** pour que les trois moteurs voient les mêmes fichiers —
un filtre réimplémenté donnait 467 fichiers d'écart.

```bash
# Elasticsearch
docker run -d --name lucivy-es -p 9200:9200 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e ES_JAVA_OPTS=-Xms8g -Xmx8g \
  docker.elastic.co/elasticsearch/elasticsearch:8.19.0
python3 benches/compare_elasticsearch.py /tmp/lucivy-cmp-90k

# tantivy (amont, dev-dependency, pas le fork — vérifié par checksum)
CMP_CORPUS=/tmp/lucivy-cmp-90k cargo test --release -p lucivy-core \
    --test compare_tantivy -- --ignored --nocapture

# les deux sondes qui documentent leurs limites
cargo test --release -p lucivy-core --test compare_tantivy \
    probe_ngram_positions -- --ignored --nocapture      # positions toujours 0
cargo test --release -p lucivy-core --test compare_tantivy \
    probe_default_tokenizer -- --ignored --nocapture    # le séparateur disparaît
```

### Les autres

```bash
# Sharding, 90 000 fichiers. t01 CONSTRUIT l'index que les autres lisent.
LUCIVY_BENCH_DIR=$HOME/lucivy_bench BENCH_DATASET=/tmp/linux-bench \
  cargo test --release -p lucivy-core --test bench_sharding t01 -- --ignored --nocapture

# Coût d'une recherche filtrée
cargo test --release -p lucivy-core --test test_filtered_search_truth \
    bench_filtered -- --ignored --nocapture

# Parité navigateur / natif
cargo test --release -p lucivy-core --test test_playground_parity -- --ignored --nocapture

# Sparse (dump BGE-M3 dans ~/lucivy_bench/sparse ou $LUCIVY_SPARSE_DUMP)
cargo test --release -p sparse-vector --test bench_commit_cost -- --ignored --nocapture
cargo test --release -p sparse-vector --test bench_segment_search -- --ignored --nocapture
cargo test --release -p sparse-vector --test bench_filter_selectivity -- --ignored --nocapture
```

⚠️ **`bench_sharding` ne vérifie rien.** Ses onze lignes affichent « 20 hits »
parce que 20 est le plafond de résultats. Il chronomètre une réponse que
personne n'a contrôlée — c'est ce qui a laissé passer un bug de rappel pendant
cinq versions. Ne pas en tirer de chiffre publiable.

---

## 3. Les variables d'environnement

| variable | effet |
|---|---|
| `V3_CORPUS`, `V3_MAX_DOCS`, `V3_QUERIES`, `V3_INDEX_DIR` | corpus, plafond, panel, persistance |
| `V3_MERGE=1`, `V3_MERGE_TARGET=n` | compacter l'index (24 segments : −40 % de taille) |
| `V3_SPANS_REPORT_ONLY=1` | revenir au critère « ensemble de documents » |
| `V3_FUZZY_MODE=pieces\|pivot\|auto` | forcer le générateur de candidats |
| `V3_DIAG_FUZZY=1`, `V3_DIAG_FUZZY_MAX=0` | diagnostic fuzzy, tous les rejets |
| `LUCIVY_HIGHLIGHT_SPAN_CAP`, `LUCIVY_MAX_MATCHES_PER_SEGMENT` | bornes mémoire, `0` = illimité |
| `LUCIVY_BENCH_DIR`, `LUCIVY_SPARSE_DUMP`, `CMP_CORPUS` | emplacements des bancs |
| `LUCIVY_SPARSE_VERIFY_CRC=1`, `LUCIVY_SPARSE_MAX_SEGMENTS` | sparse |

---

## 4. Publier une version

**Depuis 3.0.9, un tag suffit.** `release.yml` construit les cinq plateformes
de wheels et d'addons, le wasm, attache tout à la release, puis publie sur
PyPI, npm et crates.io — **trusted publishing partout, aucun jeton, aucun
OTP**.

```bash
# 1. Bumper partout : Cargo.toml racine + 4 crates + bindings/*/Cargo.toml
#    + bindings/nodejs/package.json et npm/*/package.json
#    + bindings/emscripten/package.json + bindings/python/pyproject.toml
#    + les titres des README
cargo update -w --offline

# 2. CHANGELOG : « Unreleased » devient « Lucivy X.Y.Z »

# 3. Reconstruire le wasm (il estampille aussi la version de la page)
bash bindings/emscripten/build.sh

# 4. Tout vert (§1), puis
git push origin main <branche>
git tag -a vX.Y.Z -m "…" && git push origin vX.Y.Z
```

**À configurer une fois par paquet**, sur le site de chaque registre : un
publieur de confiance `L-Defraiteur` / `lucivy` / `release.yml` / environnement
`release` — le **nom de fichier** du workflow, pas son chemin.

- npm, 7 paquets : `lucivy`, les 5 paquets plateforme, `lucivy-wasm`.
  Vérifier : `npx -y npm@latest trust list <paquet> --otp=<code>` (un OTP par
  appel, et le nom du paquet est obligatoire malgré la documentation).
- crates.io, **5 crates seulement** : `luciole`, `lucistore`, `ld-lucivy`,
  `lucivy-core`, `sparse-vector`. Les `ld-lucivy-*` en 0.27.0 n'en ont pas
  besoin.
- PyPI : déjà fait.

**Ce que l'environnement `release` ne fait pas** : il porte une politique de
branche et **aucun réviseur requis**. La publication part seule dès qu'un tag
correspond — il n'y a pas d'étape d'approbation, quoi qu'en disent les vieux
commentaires. `PUBLISH_ENABLED` (variable **de dépôt**, pas d'environnement :
le `if:` d'un job ne voit pas ces dernières) est le seul verrou.

**Les crates en dernier**, et ce n'est pas cosmétique : 3.0.0 les a publiées en
premier, avant deux correctifs du cœur et avant la mise à jour des README, et
3.0.1 a dû suivre le soir même. Une version crates.io ne revient jamais.

### Publier à la main, si nécessaire

```bash
source .vault/load.sh                    # jetons, dossier git-ignoré
(cd bindings/emscripten && npm whoami && npm publish --otp=<code>)
cargo publish -p luciole
cargo publish -p lucistore    # cargo *avertit* au lieu d'échouer si l'index
cargo publish -p ld-lucivy    # tarde — attendre avant le suivant
cargo publish -p lucivy-core
cargo publish -p sparse-vector
```

### Vérifier

```bash
curl -s https://pypi.org/pypi/lucivy/X.Y.Z/json -o /dev/null -w "%{http_code}\n"
curl -s -H "Cache-Control: no-cache" https://registry.npmjs.org/lucivy | \
  python3 -c "import json,sys; print(json.load(sys.stdin)['dist-tags']['latest'])"
curl -s -H "User-Agent: check" https://crates.io/api/v1/crates/lucivy-core | \
  python3 -c "import json,sys; print(json.load(sys.stdin)['crate']['max_version'])"
curl -sI https://l-defraiteur.github.io/lucivy/pkg/lucivy.wasm | grep -i content-length
```

**Pièges de vérification rencontrés** : `npm view` sert un cache CDN périmé —
interroger `registry.npmjs.org` directement ; l'API de crates.io **exige un
`User-Agent`** ; l'index JSON de PyPI retarde alors que l'endpoint de version
répond déjà.

### Le compte GitHub

`gh auth status` liste deux comptes. Le compte **pro ne doit jamais apparaître**
sur les dépôts personnels. Avant tout `gh release/pr/issue/api` sur lucivy :
`gh auth switch -u L-Defraiteur`, et remettre l'autre après. Les `git push`
passent par SSH et ne sont pas concernés.

---

## 5. Les corpus

| chemin | contenu | usage |
|---|---|---|
| `/tmp/linux-bench` | clone du noyau, ~95 700 fichiers | source des corpus |
| `/tmp/lucivy-cmp-90k` | **93 983 fichiers, 857 Mo** matérialisés | le banc de comparaison |
| `/tmp/lucivy-cmp` | 10 000 fichiers, 41,5 Mo | essais rapides |
| `~/lucivy_bench/sparse/` | dump BGE-M3, 2 924 vecteurs | bancs sparse |
| `sparse_vector/tests/fixtures/` | extrait de 500 documents | commité, pour la CI |

Les corpus matérialisés se reconstruisent par un filtre identique à celui du
harnais (≤ 100 000 octets, non vide, sans octet nul, UTF-8 valide, dossiers
`target`, `node_modules`, `.git`, `build`, `__pycache__`, `playground`
exclus), **copie réelle et ordre trié** — pas de lien symbolique, sinon les
moteurs ne voient pas le même ensemble.

---

## 6. Nettoyage

`target/` grossit vite : 114 Go le 27 août, ramené à 15. Cargo ne nettoie
jamais — chaque binaire de test existe en autant d'exemplaires que de
rebuilds.

```bash
du -sh target/*/                    # ce qui pèse
rm -rf target/debug/incremental     # cache de codegen, pur déchet régénérable
rm -rf target/package               # tarballs de cargo publish déjà publiés
```

Les doublons périmés de `target/*/deps` (même nom, hash différent, mtime plus
ancien) sont récupérables sans rien casser : 50 Go la dernière fois.

⚠️ `/home` est en **btrfs** : la place revient de façon asynchrone, et reste
retenue tant qu'un snapshot référence les extents supprimés. Un `df` qui ne
bouge pas tout de suite n'est pas une erreur.

Les index d'essai occupent aussi : `/tmp/lucivy-idx-90k` fait 18 Go,
`/tmp/tv_default` et `/tmp/tv_ngram` environ 1,3 Go, et le conteneur
`lucivy-es` retient 8 Go de RAM (`docker rm -f lucivy-es`).
