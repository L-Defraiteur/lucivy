# Rapport — ce qui a été fait, comment le vérifier, ce qui reste

27 août 2026. Autonome : écrit pour être lu sans l'historique de la session.
Couvre les 23 commits depuis le tag `v3.0.5` (tous sur `main`, non publiés).

Compagnons : `01-design-sparse-segments-dimension-globale.md` (le design
sparse et ses mesures), `docs/26-08-2026/02-…` (la page de présentation et
la release 3.0.5), `RELEASE.md` (la procédure de publication).

---

## 1. Ce qui a été fait depuis 3.0.5

### 1.1 Le mode fédéré passe par le DAG

`search_with_global_stats` collectait **tous** les documents appariés de
tous les shards dans un `Vec` sans plafond avant de trier : pas de
parallélisme, pas de top-k borné, pas d'`allowed_ids`, pas de découpage
mémoire. Il appelle maintenant le même `search_internal` que `search()` ;
les statistiques fusionnées descendent par `DagOpts::global_stats` jusqu'à
`BuildWeightNode`, où elles remplacent l'agrégat local et écrasent les
`doc_freq` du prescan local (le prescan tourne quand même : c'est lui qui
remplit le cache rejoué par les scorers).

`search_filtered_with_global_stats` est venu avec — le pré-filtre sous les
statistiques de la fédération —, et il est exposé dans **tous les bindings** :
`allowed_ids` / `allowedIds` en argument de `search_with_global_stats` en
Python et Node, une méthode dédiée en C++, le tableau d'ids déjà pris par
l'autre recherche filtrée côté wasm.

Nuance publiée dans la doc de la page : avec des statistiques fédérées, les
ids disent **ce qui est visité**, les statistiques disent **comment ça
note** — un document garde le rang qu'il aurait pour n'importe qui.
`search_filtered` sur un index seul fait l'inverse : il note comme si
l'index était le sous-ensemble.

### 1.2 Le sparse : atomique, segmenté, fusionnable

Dans l'ordre où ça s'est fait :

1. **Écriture atomique** — `sparse.mmap`, `vectors.bin` et `dims.bin`
   étaient écrits directement sur leur destination. Temporaire + `flush` +
   `sync` + `rename`, plus un `sync` du répertoire ; pied CRC-32 (format v2)
   et contrôle de longueur à l'ouverture. `_sparse_config.json` versionné et
   tolérant aux champs inconnus.
2. **La dimension est le token id global** (format v3) — le mot de
   bourrage de `DimHeader` devient le `token_id`, la table est triée, la
   recherche d'une dimension est une recherche binaire. `sparse_dims.bin`
   n'est plus nécessaire. C'est ce qui rend deux segments fusionnables sans
   remappage.
3. **Segments** — un commit n'écrit plus que son delta : **320 ms → 35 ms**
   à 200 000 vecteurs, et plat au lieu de linéaire. `meta.json` nomme les
   segments actifs, une suppression est un tombstone, `seg_<id>.ids` (8
   octets par document) dit quel segment porte un id.
4. **Merge** — `segments::merge_segments(&[&Segment], …)` marche les tables
   de tokens ensemble et concatène. Il prend une tranche de segments *de
   n'importe où* : fusionner deux index sera le même appel. Un commit
   compacte au-delà de huit segments (`LUCIVY_SPARSE_MAX_SEGMENTS`).
5. **Le filtre** — sur un index segmenté, `search_filtered` testait un
   prédicat par document : le chemin `seek` était devenu inatteignable
   (régression de l'étape 3). L'ensemble descend maintenant dans chaque
   segment. Au passage, la copie + tri + dédup par requête et la
   construction d'un `HashSet` par requête ont disparu : **un ensemble trié
   et sans doublon est lu sur place**. 540 000 ids : 6,0 ms → **0,22 ms**.

Un index d'avant les segments s'ouvre toujours et se convertit à son commit
suivant.

### 1.3 Les mesures, et deux chiffres retirés

Trois corpus ont été nécessaires pour un seul chiffre, et les deux premiers
étaient faux **de façon reproductible** :

- dimensions uniformes, tous poids à 1.0 → ×5,3 à vingt segments. Le WAND
  n'a rien avec quoi élaguer ; on mesurait le générateur.
- vrais vecteurs BGE-M3, mais **machine occupée à les produire** → ×7,8 à
  cent segments.
- au repos, trois runs, les deux corpus : **le nombre de segments ne change
  pas le temps de recherche** (×1,0 à ×1,5, sans tendance).

Une explication avait été écrite entre les deux (« un modèle a un
vocabulaire partagé, donc de longues listes que le découpage casse ») : elle
expliquait du bruit, elle a été retirée aussi. Le seuil de huit segments est
maintenant justifié par ce qui se **compte** — fichiers, mappings, chemin
d'écriture, octets supprimés — et non par ce qui se chronomètre.

**Règle à retenir : un bench sur données synthétiques mesure le générateur ;
sur machine chargée, la charge. Aucun des deux ne s'annonce.**

### 1.4 Clippy sur nos crates, en CI

La CI ne lintait que `cargo clippy --lib`, c'est-à-dire **le moteur seul** —
`lucivy_core`, `lucistore`, `luciole` et `sparse_vector` n'avaient jamais
été passés à clippy. Quinze lints corrigés, et le job en lint désormais
quatre :

```yaml
cargo clippy -p lucivy-core -p lucistore -p luciole -p sparse-vector --lib -- -D warnings
```

Les forks vendorisés (`lucivy-fst`, `bitpacker`, `columnar`, `common`)
restent dehors : leur dette est celle de tantivy.

### 1.5 La page de présentation

Le playground s'ouvre sur une page qui documente : pitch, terminal qui
clone et indexe pour de vrai, puis **neuf entrées** d'usage (query, filter,
shard, distribute, storage, une transaction, snapshot & sync, navigateur,
sparse), chacune avec son « how it works » **sous** le code. Un sélecteur de
langage unique bascule les sept entrées qui existent en Python, Node et
Rust ; les deux autres disent pourquoi elles n'ont qu'un langage.

La démo indexe désormais le tarball que le déploiement Pages fabrique du
même commit (même origine, pas de quota GitHub) ; le proxy CORS ne relaie
plus que `/repos/<owner>/<repo>/tarball`, pour les origines du playground,
en streaming.

---

## 2. Les tests : quoi lancer, et quand

### 2.1 Ce que la CI lance (à reproduire avant un push)

```bash
export PATH="$HOME/.cargo/bin:$PATH"

cargo test --lib                       # moteur, ~1 435 tests
cargo test --lib --no-default-features # et la matrice de features de la CI
cargo clippy --lib -- -D warnings
cargo clippy -p lucivy-core -p lucistore -p luciole -p sparse-vector --lib -- -D warnings

V3_CORPUS=. cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth \
    v3_ground_truth_contains -- --nocapture      # le job « ground-truth »
```

Bindings, comme la CI :

```bash
cd bindings/python && bash build.sh && .venv/bin/python -m pytest tests   # 108 + 4 skips
cd bindings/nodejs && npm run build && node test.mjs
                     node tests/v3_api.mjs && node tests/blob_store.mjs
                     node tests/smoke_warnings.mjs "$PWD/lucivy.node"     # ← veut le chemin de l'addon
cargo test -p lucivy-cpp                                                  # 16 tests
```

**Toujours rediriger dans un fichier puis grepper — jamais `| tail`** :

```bash
cargo test -p lucivy-core --no-fail-fast > /tmp/t.txt 2>&1
grep -c "test result: ok" /tmp/t.txt
grep -n "^---- \|panicked at\|fatal runtime" /tmp/t.txt | head
```

Le `fatal runtime` compte : un dépassement de pile abrège la suite sans
qu'aucun test soit marqué `FAILED` (arrivé le 26 août avec le look-ahead).

### 2.2 La suite complète

```bash
V3_CORPUS=. cargo test -p lucivy-core --no-fail-fast   # 183 passés, 27 ignorés
cargo test -p sparse-vector                            # 79 passés
cargo test -p luciole --lib                            # 43
cargo test -p lucistore --lib                          # 169
```

`cargo test --workspace` fait passer aussi les doctests : **trois échouent**
et c'est antérieur à cette session (vérifié au tag `v3.0.5`) — un import
inutilisé dans un doctest de `src/tokenizer/equal_chunk.rs`, deux dans
`columnar` et `common`. La CI ne lance pas les doctests.

### 2.3 Ce que chaque test de vérité verrouille

| fichier | ce qu'il prouve |
|---|---|
| `test_sfx_v3_ground_truth.rs` | comptes **et** spans comparés à un grep octet par octet, sur un vrai corpus |
| `test_filtered_search_truth.rs` | filtré = non filtré ∩ autorisés, documents *et* highlights, 11 requêtes × 4 jeux, avec suppressions |
| `test_federated_search.rs` | l'union de deux nœuds = un index unique, **scores égaux** ; le filtré fédéré ; le top-k borné |
| `test_truncation_flag.rs` | une requête plafonnée le **dit** (`last_search_truncated`) |
| `test_fuzzy_tiers.rs` | le palier fuzzy = distance d'édition vérifiée |
| `test_segments.rs` (sparse) | N segments = un index compacté ; update, suppression, merge, conversion d'un ancien format ; filtré identique avant/après merge |
| `test_filter_truth.rs` (sparse) | filtré = non filtré ∩ autorisés sur **les deux chemins**, ids non triés / dupliqués / inconnus |
| `test_global_dims.rs` (sparse) | la dimension est le token global ; un v1/v2 se lit ; recharger par position corromprait tout |
| `test_mmap_durability.rs` (sparse) | tronqué refusé, octet retourné attrapé par le CRC, aucun `.tmp` laissé |
| `test_acid_blob_v3.rs` | le blob store est la source de vérité ; `close()` = plus aucun appel |

---

## 3. Les benchs : tous `#[ignore]`, avec leur ligne de commande

Aucun bench ne tourne en CI (des runners partagés ne mesurent rien).

### 3.1 Plein texte

```bash
# Sharding sur 90 000 fichiers du kernel. t01 CONSTRUIT l'index que les
# autres lisent : le lancer en premier, en --release, sinon des heures.
LUCIVY_BENCH_DIR=$HOME/lucivy_bench \
  cargo test --release -p lucivy-core --bench bench_sharding t01 -- --ignored --nocapture

# Coût d'une recherche filtrée sur l'index 10 000 fichiers que
# test_playground_parity laisse dans /tmp/lucivy_parity_native
cargo test --release -p lucivy-core --test test_filtered_search_truth \
  bench_filtered -- --ignored --nocapture

# Parité navigateur / natif (construit l'index de 10 000 fichiers)
cargo test --release -p lucivy-core --test test_playground_parity -- --ignored --nocapture
```

### 3.2 Sparse

Les vecteurs viennent du dump BGE-M3 de la session rag3weaver, cherché dans
`$LUCIVY_SPARSE_DUMP`, puis `$LUCIVY_BENCH_DIR/sparse`, puis
`~/lucivy_bench/sparse` — et à défaut de l'extrait de 500 documents commité
dans `sparse_vector/tests/fixtures/`. `BENCH_DOCS=n` change la taille,
`BENCH_CORPUS=text` compare contre des vecteurs tirés du texte du dépôt.

```bash
cargo test --release -p sparse-vector --test bench_commit_cost -- --ignored --nocapture
#   ce que coûte un commit quand l'index grossit (doit rester plat)

cargo test --release -p sparse-vector --test bench_segment_search -- --ignored --nocapture
#   search_cost_per_segment  : le nombre de segments ne doit pas changer le temps
#   update_cost_per_segment  : lui, croît (2,5 → 4 µs de 1 à 100 segments)

cargo test --release -p sparse-vector --test bench_filter_selectivity -- --ignored --nocapture
#   filter_cost_by_selectivity     : ×0,15 à 0,1 % du corpus, ×1,3 au pire
#   cost_of_a_very_large_allowed_set : 540 000 ids en 0,22 ms

cargo test --release -p sparse-vector --test bench_wand_compare -- --ignored --nocapture
```

**Avant de croire un chiffre** : `uptime` (la charge doit être basse), et le
lancer **deux fois**. Deux chiffres faux ont été publiés cette nuit faute de
cette discipline.

### 3.3 Le playground

```bash
cd playground && node serve.mjs          # http://localhost:9877
#   /               la page de présentation, la démo démarre seule
#   /#playground    le playground seul
#   ?proxy          forcer le clone GitHub au lieu du tarball local
#   ?desktop        ignorer les réglages « petit appareil »
#   ?maxmatches=N   plafond de matches par segment (0 = illimité)
#   ?verbose        diagnostics moteur

node test-playground.mjs                 # playwright, si installé
python3 test_playground.py
```

---

## 4. Les variables d'environnement qui comptent

| variable | effet |
|---|---|
| `V3_CORPUS` | racine du corpus des tests de vérité (`.` = le dépôt) |
| `LUCIVY_BENCH_DIR` | où vivent les gros index de bench (défaut `~/lucivy_bench`) |
| `LUCIVY_SPARSE_DUMP` | dossier du dump BGE-M3 |
| `LUCIVY_VERBOSE` | traces moteur (recherche, batchs, troncature) |
| `LUCIVY_MAX_MATCHES_PER_SEGMENT` | plafond de matches par segment ; `0` = illimité |
| `LUCIVY_HIGHLIGHT_SPAN_CAP` | plafond de spans ; `0` = illimité |
| `LUCIVY_SHARD_BATCH_BYTES` | budget mémoire d'un batch de shards |
| `LUCIVY_SPARSE_MAX_SEGMENTS` | compactage sparse (défaut 8, `0` = jamais) |
| `LUCIVY_SPARSE_VERIFY_CRC` | vérifier le CRC de chaque `sparse.mmap` à l'ouverture |
| `POSTGRES_URL` | tests ACID contre un vrai Postgres (`#[ignore]`) |

---

## 5. Ce qui reste à faire

### 5.1 Publier 3.0.6 — quand tu veux

Tout ce qui précède est en `Unreleased` dans `CHANGELOG.md`. La procédure a
changé depuis 3.0.5 : elle est dans `RELEASE.md`, mais le résumé est

```bash
# 1. Bumper la version partout (une seule pour tout le workspace)
#    Cargo.toml racine + les 4 crates + bindings/*/Cargo.toml
#    + bindings/nodejs/package.json (et npm/*/package.json)
#    + bindings/emscripten/package.json + bindings/python/pyproject.toml
#    + les titres des README
cargo update -w --offline     # rafraîchit Cargo.lock

# 2. Tout vert (§2), puis
git tag -a v3.0.6 -m "…" && git push origin v3.0.6
#    → le workflow release.yml construit 5 wheels + 5 addons, les attache à
#      la release, puis ATTEND ton approbation sur la page du run
#    → approuver ⇒ PyPI (trusted publishing) et npm

# 3. À la main, après :
cd bindings/emscripten && npm publish --otp=<code>   # npm whoami d'abord
cargo publish -p luciole && … lucistore, ld-lucivy, lucivy-core, sparse-vector
```

Deux verrous protègent la publication : l'environnement `release` (ton
approbation) **et** la variable de dépôt `PUBLISH_ENABLED=true`.

**Attention `sparse-vector` 3.0.6** : le format `sparse.mmap` passe en v3 et
l'index devient segmenté. Les anciens index s'ouvrent et se convertissent au
commit suivant, mais un binaire 3.0.5 **ne lira pas** un index écrit par
3.0.6 (« unsupported version: 3 »). À dire dans les notes de version.

### 5.2 npm en trusted publishing (supprime le dernier secret)

Les six paquets existent maintenant, donc c'est possible : sur npmjs.com,
pour `lucivy`, `lucivy-wasm`, `lucivy-linux-x64-gnu`,
`lucivy-linux-arm64-gnu`, `lucivy-darwin-x64`, `lucivy-darwin-arm64`,
`lucivy-windows-x64` → Settings → Trusted Publisher → GitHub
`L-Defraiteur/lucivy`, `release.yml`, environnement `release`. Puis
supprimer le secret `NPM_TOKEN` et révoquer le token : le workflow bascule
seul sur `--provenance`. Plus aucun OTP.

### 5.3 Dialogue rag3weaver

Deux réponses écrites, **non commitées dans leur dépôt** (c'est le leur) :

- `26-aout-2026-20h29/07-reponse-lucivy-cahier-des-charges.md` — la réponse
  au cahier des charges (L3 et L5 faits, deux corrections à leur lecture,
  l'ordre défendu), plus la demande de dump.
- `26-aout-2026-20h29/09-reponse-lucivy-prefiltre-sparse.md` — les réponses
  au pré-filtre sparse, la courbe de sélectivité, le bug que leur question a
  trouvé.

En attente de leur côté : combien de segments un domaine monte en pratique
(ça fixerait le seuil de compactage sur autre chose qu'un raisonnement), et
la confirmation que leurs domaines arrivent **triés**.

### 5.4 Le reste, par ordre décroissant d'intérêt

- **Un GIF de la démo** pour le README et les posts de lancement.
- **Tester la page de présentation sur un téléphone** — la démo réduit à
  quatre recherches en dessous de 720 px, jamais vérifié en vrai.
- **L2 du cahier des charges** : exposer une recherche restreinte à un
  sous-ensemble de shards (`ShardFilter` est `pub(crate)` et n'est produit
  que par le batcheur mémoire) **et** cadrer les statistiques dessus — avec
  deux variantes distinctes, sinon on casse l'invariance des scores au
  batching.
- **L1** : monter/démonter un shard déjà construit. `IndexWriter::add_segment`
  et `merge_indices` existent dans le fork et ne sont appelés par personne.
- **Un index dense** (HNSW/flat) si on veut couvrir l'hybride entièrement —
  c'est là qu'un GPU servirait, pas sur le sparse.
- **Trois doctests rouges** dans les crates vendorisés (§2.2) : la CI ne les
  lance pas, mais `cargo test --workspace` non plus n'est pas vert.
- **`bindings/nodejs/node_modules/@napi-rs/cli` est suivi par git** — un
  vestige, à sortir comme l'a été `lucivy.node` le 26 août.
