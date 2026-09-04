# ld-lucivy — Contexte projet

## Architecture

Moteur full-text search Rust avec substring matching via Suffix FST. Trois couches :

- **ld-lucivy** : moteur core (index, query, scoring, merger, segments, SFX engine)
- **lucivy_core** : handle unifié (`ShardedHandle`), query builder, tokenizers, snapshot/delta, blob store
- **luciole** : framework actor/DAG (crate séparé, WASM-safe)
- **lucistore** : persistance partagée (BlobStore, ShardStorage, snapshot/delta, sync)
- **sparse_vector** : index sparse (postings + WAND, `src/wand/`) sur lucistore, shardé via luciole
  (`ShardedSparseHandle`) — crate ami, MIT, code original (design inspiré de Qdrant, aucun code
  dérivé : audit ligne à ligne, voir `docs/24-08-2026/05-wand-comparaison.md`).
  Commit atomique depuis le 26 août au soir : temporaire + `rename` + `sync`,
  pied CRC-32, contrôle de longueur à l'ouverture,
  `LUCIVY_SPARSE_VERIFY_CRC=1` pour vérifier le CRC ; `_sparse_config.json`
  versionné et tolérant. **Segmenté depuis le 27 août** : un index est
  `meta.json` + N `seg_<id>.mmap` (format v3 : la dimension **est** le
  token id global, table triée) + `seg_<id>.ids` ; un commit n'écrit que le
  delta (28-33 ms au lieu de 320 à 200 k vecteurs), une suppression est un
  tombstone, un merge marche les tables triées ensemble sans rien remapper
  (`segments::merge_segments`, `&[&Segment]` — donc fusionner deux index
  est le même appel), et un commit compacte au-delà de 8 segments
  (`LUCIVY_SPARSE_MAX_SEGMENTS`) — pour le nombre de fichiers et le chemin
  d'écriture, **pas** pour la vitesse de recherche : au repos, trois runs,
  elle ne bouge pas de 1 à 100 segments. Les benchs sparse lisent le dump
  rag3weaver (`~/lucivy_bench/sparse/*.jsonl`, extrait de 500 docs commité
  dans `tests/fixtures/`). Deux chiffres faux ont été publiés avant :
  ×5,3 (corpus uniforme, poids plats → le WAND n'élague rien) et ×7,8
  (machine occupée à produire le dump). **Un bench sur données synthétiques
  mesure le générateur ; sur machine chargée, la charge.** Les v1/v2 s'ouvrent et se convertissent
  au commit suivant. Design : `docs/27-08-2026/01-…`. Vérité :
  `test_global_dims.rs`, `test_segments.rs`, `test_mmap_durability.rs`,
  benchs `bench_commit_cost.rs` et `bench_segment_search.rs`.
  **Filtre (27 août)** : donner des ids **triés et sans doublon** — ils sont
  lus sur place, l'appartenance est une recherche binaire, et le filtre coûte
  ×0,15 (il *gagne*) à 0,1 % du corpus et ×1,3 au pire jusqu'à 100 % ;
  540 000 ids répondent en 0,22 ms (6,0 ms avant). Un ensemble non trié est
  copié et trié à **chaque** requête. Un filtré = un non filtré intersecté,
  mêmes documents et même ordre, scores à quelques ULP près (les deux chemins
  somment les lanes dans un ordre différent). Vérité : `test_filter_truth.rs`,
  bench `bench_filter_selectivity.rs`
- **Bindings** (5 crates) :
  - CXX bridge rag3db : `lucivy_fts/rust/src/bridge.rs`
  - WASM emscripten : `bindings/emscripten/src/lib.rs` (extern "C" + SharedArrayBuffer + pthreads)
  - Node.js napi : `bindings/nodejs/src/lib.rs` (napi-rs)
  - Python PyO3 : `bindings/python/src/lib.rs` (pyo3)
  - C++ standalone : `bindings/cpp/src/lib.rs` (cxx bridge namespace lucivy)

Note : wasm-bindgen (single-threaded) a été retiré — emscripten est le seul binding WASM.

## Query types — v2 compat layer

Toutes les queries texte passent par le SFX engine quand sfx_enabled=true.
Les anciens types sont routés automatiquement via `build_query()` dans `lucivy_core/src/query.rs`.

| Type | Route vers | Paramètres |
|------|-----------|------------|
| `contains` | natif SFX | `field, value, distance, anchor_start, exact_match, regex, strict_separators` |
| `contains_split` | natif SFX | split whitespace → boolean should de contains |
| `term` | → contains + anchor_start + exact_match | cross-token exact match |
| `fuzzy` | → contains + distance | cross-token fuzzy via trigram pigeonhole ; `fuzzy_metric: "jaro_winkler"` + `min_similarity` (0.9) valide les candidats par Jaro-Winkler au lieu de Levenshtein |
| `regex` | → contains + regex=true | cross-token regex via literal extraction |
| `phrase` | → contains | multi-token adjacency |
| `startsWith` | → contains + anchor_start | SI=0 only |
| `startsWith_split` | → contains_split + anchor_start | |
| `parse` | value simple → OR de contains par mot×champ ; syntaxe booléenne (AND/OR/NOT, guillemets, +/-, parenthèses autonomes) → `boolean` de contains (NOT > AND > OR, mots côte à côte = OR) ; highlights dans les deux cas, multi-`fields` | `query_warnings` dit laquelle |
| `phrase_prefix` | → contains | prefix match dernier token |
| `boolean` | composite | must/should/must_not |
| `disjunction_max` | composite | max score sub-queries |
| `more_like_this` | TF-IDF natif | pas SFX (recommandation, pas substring) |

### Paramètres contains (QueryConfig)

- `anchor_start: bool` — SI=0 only (match au début du token)
- `exact_match: bool` — match couvre le(s) token(s) entier(s)
- `distance: u8` — Levenshtein (0=exact, >0=fuzzy via RegexContinuationQuery)
- `regex: bool` — pattern regex cross-token
- `strict_separators: bool` — valider les séparateurs entre tokens

### Bornes mémoire côté requête (3.0.2)

- `LUCIVY_HIGHLIGHT_SPAN_CAP` (4 M natif / 1 M wasm) : le sink d'highlights
  s'arrête là ; `ShardedHandle` relance alors la recherche filtrée aux ids du
  top-k pour ne remplir que leurs spans (scores/ordre de la 1ʳᵉ passe).
- `LUCIVY_MAX_MATCHES_PER_SEGMENT` (4 M natif / 20 k wasm ; `0` = illimité) :
  plafond de `MatchV3` par segment et par requête ; au-delà la requête est
  tronquée sur ce segment, jamais d'abort, et **la recherche le dit** :
  `ShardedHandle::last_search_truncated()` (Python `last_search_truncated`,
  Node `lastSearchTruncated()`, wasm `memory_status.last_search_truncated`).
  Chemin : thread-local dans `resolve.rs` → `Query::prescan_truncated` →
  métrique `truncated` du nœud `build_weight` → handle. Le panel de 21
  requêtes ne l'atteint pas ; « t » sur 10k fichiers kernel l'atteint.
  `LUCIVY_HIGHLIGHT_SPAN_CAP=0` = illimité aussi.

### Recherche filtrée (`allowed_ids`) — pré-filtre réel depuis le 26 août

Le jeu d'ids voyage jusqu'au prescan v3 : `filtered_segment_reader`
(`sharded_handle.rs`) pose sur un clone du lecteur le bitset alive (collecteur,
comme avant) **et** `set_doc_filter` (canal séparé, lu par les trois prescans
→ `BriquesContext.filter_docs: Option<&dyn DocFilter>` → `resolve_filtered`).
`BuildWeightNode` ne prescanne que les shards actifs. Une recherche filtrée
score **comme si l'index était le sous-ensemble** : `doc_freq` compté par le
prescan filtré, `N` = taille du sous-ensemble (`AggregatedBm25StatsOwned::
with_subset_docs`, tokens mis à l'échelle → longueur moyenne du corpus
conservée) ; ordre d'une requête mono-terme inchangé, scores égaux au non
filtré seulement si tout est autorisé ; non filtré inchangé (canal séparé du
bitset de suppression). Vérité : `test_filtered_search_truth.rs` ; bench
`bench_filtered_search_cost`.

## SFX Engine

Suffix FST avec partitionnement SI=0/SI>0 pour le substring matching.

- **SI=0** : début de token (pour anchor_start/startsWith)
- **SI>0** : suffixes (pour contains anywhere)
- **Cross-token** : `falling_walk` + `sibling_table` pour matcher à travers les frontières de tokens
- **Fuzzy** : trigram pigeonhole via RegexContinuationQuery
- **Regex** : extraction de littéraux, validation regex sur candidats

Fichiers par segment (v3, par champ) : `.sfx`, `.sfxpost`, `.termtexts`, `.posmap`,
`.word_sfxpost`, `.word_pos_map`, `.sibling_v3`. Référence 10 000 fichiers
au soir du 4 septembre : 508 Mo (1 152 le matin), `.sfx` 41 %. (`.bytemap` : jusqu'au 4 septembre 2026,
ignoré depuis ; `.gapmap`, `.sepmap` : v2.)

**`sfx_version` par défaut = 3** depuis le 23 août 2026. Un `meta.json` sans le champ
est un index v2 (le champ est maintenant toujours écrit). Les tests du moteur v2
utilisent `Index::create_in_ram_sfx2`.

## Sharding

- `ShardedHandle` : N shards, routing configurable
- `balance_weight=1.0` : round-robin, indexation rapide — c'est le défaut du
  `ShardRouter` (`DEFAULT_BALANCE_WEIGHT`, `shard_router.rs:36`), **pas celui
  d'un index** : `ShardedHandle` applique `unwrap_or(0.2)` quand la config ne
  le dit pas (`sharded_handle.rs:1687`, `:1741`). Les deux commentaires
  divergent ; c'est 0,2 qui s'applique.
- `balance_weight=0.2` (défaut effectif) : token-aware, co-localise les
  documents similaires
- BM25 cross-shard : `ExportableStats` sérialisable, `merge()`, `search_with_global_stats()`
- Distributed ready : export_stats → merge → search_with_global_stats
  (+ `search_filtered_with_global_stats` : le même, restreint à des ids).
  Depuis le 26 août au soir ce mode **passe par le DAG** comme `search()` :
  shards en parallèle, top-k borné, batching mémoire, réparation des
  highlights. Les stats fusionnées voyagent via `DagOpts::global_stats`
  jusqu'à `BuildWeightNode`, où elles remplacent l'agrégat local et écrasent
  les `doc_freq` du prescan local (le prescan tourne quand même : c'est lui
  qui remplit le cache rejoué par les scorers). Vérité :
  `test_federated_search.rs` — union des nœuds = index unique, **et scores
  égaux**. Avant : boucle séquentielle, tous les hits dans un `Vec` sans
  plafond.

## Formats d'échange

- **LUCE** : snapshot complet (tous les shards)
- **LUCID** : delta incrémental (1 shard)
- **LUCIDS** : delta incrémental sharded (N shards, seulement les shards modifiés)

## Persistence — Directories

| Type | Usage | I/O pattern |
|------|-------|-------------|
| StdFsDirectory | Natif + WASM/OPFS | Deferred I/O : tout en RAM jusqu'au terminate() |
| RamDirectory | Tests | Pure RAM |
| BlobDirectory | ACID (mmap + DB blob) | Extensible (Postgres, S3, etc.) |

**WASM crucial** : `FsWriter` bufferise en RAM, I/O au `terminate()` seulement.
Jamais d'I/O dans un actor handler.

## WASM — Règles critiques

- **JAMAIS de `thread::spawn`** en WASM — tout via le scheduler (actors/tasks)
- `docstore_compress_dedicated_thread: false` en WASM
- Watch callbacks inline en WASM (pas de thread)
- GC thread skip en WASM
- `WRITER_HEAP_SIZE = 15MB` en WASM (50MB natif)
- `MAXIMUM_MEMORY = 4GB` (limit 32-bit WASM)

## luciole — framework Actor/DAG

Crate séparé dans `luciole/`. WASM-safe.

- **Actor** : trait avec priorités (Idle→Critical), GenericActor avec handlers typés
- **Scheduler** : pool threads persistants, WASM compatible
- **DAG** : construction + exécution topologique, undo, checkpoint
- **StreamDag** : pipeline streaming avec drain topologique
- **pipe_to / collect_replies_to / task_pipe_to** : request-reply non-bloquant
- **execute_dag_async** : DagExecutor actor (DAG level-by-level)
- **WaitGraph** : tracking dépendances, dump mermaid/text
- **ActorActivity** : labels dynamiques (String) dans les dumps scheduler
- **BranchNode** : FONCTION pas struct (`BranchNode(|| cond)`)

## Bindings — état 3.0.0 (25 août 2026)

| Binding | Snapshot | Delta | 3.0.0 : `query_warnings`, `compact`, `wait_merges_quiet`, `index_bytes`, `drop_index`, `open_snapshot(_from)` | Filtré (`allowed_ids`) |
|---------|----------|-------|------|------|
| Python | export+import+**servi en place** | export+apply (sharded) | oui — tests `tests/test_v3_api.py` (93 verts, 4 skip documentés) | oui |
| Node.js | export+import+**servi en place** | export+apply (sharded) | oui — `tests/v3_api.mjs` | oui |
| C++ (cxx) | export+import+**servi en place** | export+apply (sharded) | oui — tests Rust dans `lib.rs` ; `rollback` = erreur honnête | oui |
| Emscripten | import only | manquant | `memory_status`, `preload`, drapeaux (`--scheduler-threads`, `--max-merged-docs`, `--max-builds`, `--ram-index-max-mb`…) | non |

Stockage blob ACID (`BlobStore`, `BlobShardStorage`, lazy) : **exposé dans
les trois bindings natifs** depuis le 25 août au soir — Python
(`Index.create_with_blob_store` / `open_with_blob_store`, objet duck-typé,
GIL relâché sur tout appel), Node (`BlobIndex`, classe asynchrone, callbacks
via `ThreadsafeFunction`), C++ (`lucivy::BlobBackend`, classe abstraite dans
`include/lucivy/blob_backend.h`). Règle : les méthodes du store tournent sur
les threads du scheduler ; thread-safe, jamais de réentrance dans l'index,
et le thread appelant ne doit pas tenir GIL / boucle d'événements.

Emscripten manque : export_snapshot, export_sharded_delta, apply_sharded_delta.

**Dictionnaire partagé exposé partout** (5 septembre au soir, branche `v4`) :
`shared_dictionary` dans `SchemaConfig` (alias de `sfx_version` 4,
`effective_sfx_version()`, contradiction refusée) — Python
`Index.create(..., shared_dictionary=True)` et `create_with_blob_store`,
Node `Index.create(path, fields, shards, sharedDictionary)` et
`BlobIndexOptions.sharedDictionary`, C++ `lucivy_create` accepte un objet
schéma complet (comme le chemin blob), emscripten `IndexConfig.shared_dictionary`,
bridge rag3db : le JSON de schéma tel quel. Décrit dans chaque README, le
CHANGELOG (« Unreleased ») et `lucivy_core/README.md`. **Pas le défaut**
(décision du 5 septembre). **Compaction du dictionnaire en fusion de
flux** (fin de session du 5, `suffix_fst/dictionary_compact.rs`) : au-delà
de 8 générations, les plus petites fusionnent (le compte revient à 4),
union des FST en ordre de clés, records copiés tels quels ou parents
fusionnés, sortie en flux, `.termtexts` par tas en trois passes ; noyau
19 s et 229 Mo au lieu de 48 s et 12,8 Go, fichiers identiques octet
pour octet (`01` §13).

## Extension rag3db (lucivy_fts)

- `lucivy_fts/rust/src/bridge.rs` — bridge CXX Rust (dans ce repo)
- `../../lucivy_fts/` — code C++ de l'extension (repo séparé)

## Scoring

- BM25 standard, correct cross-shard (diff=0.0000 single vs 4-shard)
- Fuzzy : tiers (`tier * 1000 + bm25`), scores négatifs voulus. Levenshtein :
  tier = **distance d'édition vérifiée** (0 exact, -1 une édition…) — plus le
  compte de trigrammes manquants, résidu du pigeonhole sans vérification qui
  donnait « 16 misses » sur une substitution (26 août). Jaro-Winkler : tier
  `-(1 - sim) * 10`. En v3 le tier transite par `CachedPrescan.coverage` →
  `SfxWeight` → `SfxScorer::with_coverage` (raccordé le 25 août au soir ;
  avant, jeté par `FuzzyQueryV3`). Test : `test_fuzzy_tiers.rs`.
- `ExportableStats` : sérialisable (Serialize/Deserialize) pour distributed search

## Tests

- `cargo test --lib` : 1431 passed, 0 failed, 16 ignored (les 3 anciens rouges
  réparés/retirés le 23 août : invariants de l'ancien design)
- `cargo test -p lucivy-core` : tout vert. `bench_sharding` t01 est `#[ignore]` :
  c'est un bench (clone du kernel, 90 000 docs × 3, des heures en debug) — le
  lancer en `--release -- --ignored`, sous `$LUCIVY_BENCH_DIR` ou
  `$HOME/lucivy_bench` (chemins codés en dur sur une autre machine jusqu'au
  26 août → « Permission denied ») ; t04 (`sfx:false`) supprimé, ce mode n'existe plus
- Vérité terrain : `docs/BENCHMARKS.md`
- Bench sharding : `bench_sharding.rs` (90K docs Linux kernel)
- Bench vs tantivy : `bench_vs_tantivy.rs`
- IMPORTANT : toujours `> /tmp/fichier.txt 2>&1`, JAMAIS `| tail`

## Build

```bash
# Tests ld-lucivy
cargo test --lib

# Tests luciole
cargo test -p luciole --lib

# Build WASM emscripten
bash bindings/emscripten/build.sh

# Playground
cd playground && node serve.mjs
```

## Docs

**Dossier courant : `docs/05-09-2026/` — pour repartir, lire dans l'ordre
`01-journal-session-5-septembre.md` (la journée du 5 : le plan par shard,
les alternatives par préfixe, la coupe en galop, le `.gmap` GMP2, les A/B
30 000 et noyau entier, l'option `shared_dictionary`, la vérité terrain du
noyau, ce qui reste, et §13 la compaction du dictionnaire en fusion
de flux), `02-architecture.md` (l'architecture complète avec
le mode dictionnaire §3 : fichiers, générations, plan puis exécution,
l'option) et `03-knowledge-dump-baselines-tests-outils.md` (corpus,
harnais, baselines de taille et de temps, A/B, profil, tests, piège RAM,
scratchpad, protocole, pièges) — tous trois autonomes. Le détail brut est
dans `docs/04-09-2026/` : `11-journal-chantier-plan-fst.md` (le journal du
5, mesures intermédiaires et fausses pistes), `10-…rapport.md` (le rapport
d'avant le chantier, note d'état en tête), `09-journal-chantier-dictionnaire.md`
(le chantier dictionnaire de la nuit du 4 au 5), `07` et `08` (état du 5 au
matin), `06-chantier-dictionnaire-partage-rapport.md` (le plan du
dictionnaire tel qu'écrit avant de coder). Puis
`04-recap-journee-et-a-faire.md` (−36 % d'index en une journée),
`05-piste-dictionnaire-partage-par-shard.md` (la décision, mesurée ×2,2), `01-recap-findings-et-plan-d-action.md`
(le plan, avec l'état et le gain mesuré de chaque étape), `03-journal-des-etapes.md`
(chaque étape : changement, taille, justesse, A/B de temps, commandes) et
`02-audit-taille-index-sfx-v3.md` (l'audit du format v3 : le `.sfx` était aux
trois quarts une table de parents ; script `benches/scan_index_size.py`).
**Le travail v4 est sur la branche `v4`, pas sur `main`**, et un binaire 3.0.x
ne lit pas un index v4 : conteneur `.sfx` **version 8** (tous les parents en
table, valeur FST = offset ; clés coupées à la frontière du token, plus de
marqueur, overlap dans le record, plat ≤ 32 parents / groupé par overlap
au-delà, `own_len` dérivé de la clé — journal
`09-journal-chantier-dictionnaire.md` ; la version 7, intermédiaire, est
refusée), ordinaux sur 28 bits (`.word_pos_map` `WMP3`), tables d'offsets
par blocs (`block_offsets.rs` : `SFP4`, `WSP4`, `SIB4`, `.termtexts`
layout 3) ; **`sfx_version` 4 = dictionnaire partagé par shard**
(`dict-<g>.<champ>.sfx/.termtexts`, `.gmap` par segment — `GMP2` depuis le
5 au soir : têtes de blocs de 64 + statistique « mots longs » du segment,
`GMAP` encore lu —, une génération
par commit avec ses seuls nouveaux textes, compaction **en fusion de
flux** des plus petites générations au-delà de
`LUCIVY_DICT_MAX_GENERATIONS` = 8 ; référence 10 000 : 390 Mo, −66 %
depuis le 4 au matin, 30 000 : −20 %, noyau entier **7,3 → 5,6 Go à
format égal** (−23 % ; le « 11,06 → 5,98 » d'avant comparait un v3 du
matin en conteneur 5), ×6,7 le texte ; à la requête, **un plan par shard** (`briques/plan.rs`,
depuis `prescan_segments_more`) remplit en parallèle les cellules FST de
la mémo du lecteur partagé avant le scatter par segment, un reste avalé
est un `Alts::Prefix` testé sur `.termtexts` au lieu d'une liste, et la
coupe des listes au `.gmap` galope ; à froid sur 30 000 fichiers, ×2-22 le
5 au matin → **×0,8-1,9** le soir (`11` §4 : exactes 2,5-5,3 ms contre
1,7-3,3 en v3, fuzzy plus rapide ; noyau entier idem, `11` §4 bis ; avec
le `.gmap` GMP2, `11` §6.1 : **×0,8-1,6, le ×1,5 tenu sur neuf requêtes
sur dix**, la regex à ×1,6) ; les tests fédéré, filtré et roundtrip LUCE
ont une variante `sfx_version 4` ; mode **optionnel, pas le défaut** —
décision à prendre, la règle du ×1,5 n'est manquée que par la regex),
`.posmap` `PMP3` (3 octets), `.sibling_v3` `SIB3` (sans gap), `.termtexts`
layout 2 (méta dans la table d'offsets), plus de `.bytemap` en v3. Chaque
lecteur ouvre encore les layouts précédents. Règle du 4 septembre : la taille
disque/RAM d'abord, l'exactitude est ce qu'on vend, le temps acceptable tant
qu'aucune requête n'approche ×1,5. Protocole par étape : index de référence
10 000 fichiers (`/tmp/lucivy-cmp`, `V3_INDEX_DIR`), panel
`v3_ground_truth_demo` identique (comptes et spans), A/B de temps sur
30 000 fichiers (`/tmp/lucivy-cmp-90k`, `V3_MAX_DOCS=30000`,
`V3_COMMIT_EVERY=2000`), réouverture des index de référence antérieurs.
Veille : `docs/28-08-2026/` — lire d'abord
`07-rapport-progression-et-taille-index.md` (ce qui a été fait, et pourquoi la
promotion attend une réduction de la taille d'index), `08-architecture.md` et
`09-knowledge-dump-tests-benchs-publication.md` (autonomes : tous les tests,
tous les bancs, la publication). Puis `06-comparaison-moteurs-mesures.md`
(lucivy contre Elasticsearch et tantivy, mesuré), `02-fuzzy-perdait-des-documents.md`
(le bug de rappel de 3.0.2-3.0.6), `04-strategie-diffusion.md`,
`05-reponses-issues.md`, et les brouillons de post `01`/`03`.
Veille : `docs/27-08-2026/02-rapport-outils-tests-et-a-faire.md`, puis
`01-design-sparse-segments-dimension-globale.md`. Avant : `docs/26-08-2026/`
et `docs/25-08-2026/` (`05` à `08`).

Les docs sont dans `docs/` organisés par dossier daté. Convention depuis le
24 août 2026 : `JJ-MM-AAAA` (triable). Dossier courant : `24-08-2026/` —
**lire d'abord `06-recap-progression-et-a-faire.md`, `07-architecture.md`,
`08-knowledge-dump-tests-benchs.md`** (état de fin de journée, autonomes) ;
01-05 sont le détail. Dialogue avec la session rag3weaver :
`../rag3db/extension/rag3weaver/docs/23-aout-2026-20h33/`.
- `9-mai-2026-11h14/` — session courante (deadlock fix, compat layer, feature inventory)
- `24-mars-2026-20h35/` — knowledge dump complet
- `3-mai-2026-15h00/` — design pipe_to, execute_dag_async

## Style

- Ne pas mentionner Claude dans les docs ou le code
- Docs en français
- Code et commentaires en anglais

## Packages publiés

| Registre | Package | Publié | Date |
|----------|---------|---------|---------|
| PyPI | `lucivy` | **3.0.7** (5 wheels `cp39-abi3` : manylinux_2_28 x86_64 + aarch64, macOS x86_64 + arm64, win_amd64 ; + sdist) | 28 août 2026 (nuit) |
| npm | `lucivy` + `lucivy-linux-x64-gnu`, `lucivy-linux-arm64-gnu`, `lucivy-darwin-x64`, `lucivy-darwin-arm64`, `lucivy-windows-x64` | **3.0.7** | 28 août 2026 (nuit) |
| npm | `lucivy-wasm` | **3.0.7** (worker + pkg WASM, `bindings/emscripten`) | 28 août 2026 (nuit) |
| crates.io | `ld-lucivy`, `lucivy-core`, `luciole`, `lucistore`, `sparse-vector` | **3.0.7** | 28 août 2026 (nuit) |

3.0.7 dans la nuit du 27 au 28, juste après 3.0.6 : **le fuzzy relâché
perdait des documents** depuis le 23 août (donc 3.0.2 à 3.0.6). `auto`
choisissait le générateur `pivot`, qui tire ses candidats des postings de
trigrammes — lesquels n'existent qu'à l'intérieur des chunks d'un token — donc
une occurrence dont les trigrammes partagés enjambent tous un séparateur n'a
aucun posting et **son document n'est pas rendu du tout**. Corrigé : séparateurs
relâchés ⇒ `pivot` exclu, condition connue d'avance. Trouvé par un nouveau
panel `v3_ground_truth_demo` (93 605 fichiers du kernel, comptes **et** spans
comparés à une lecture du disque) ; `bench_sharding` ne pouvait pas le voir,
toutes ses lignes affichent « 20 hits » parce que 20 est le plafond de
résultats. Détail : `docs/28-08-2026/02-fuzzy-perdait-des-documents.md`.
**L'environnement `release` n'a aucun réviseur requis** — les publications
partent seules dès qu'un tag correspond.

3.0.6 juste avant : **npm est passé au trusted publishing**
(OIDC) — les 6 paquets que publie `release.yml` ont un publieur de confiance
`L-Defraiteur/lucivy` / `release.yml` / environnement `release` /
permission `publish`, le secret `NPM_TOKEN` a été supprimé, et la CI a tout
publié **sans un seul OTP**, avec attestation de provenance signée. Vérifier
la configuration : `npx -y npm@latest trust list <paquet> --otp=<code>` (un
OTP par appel, et le nom du paquet est obligatoire malgré la doc).
`lucivy-wasm` reste publié **à la main** : aucun workflow ne construit le
WASM (`build.sh` demande emsdk + nightly `-Z build-std`), donc son trusted
publisher ne sert à rien tant qu'un job emscripten n'existe pas — à ajouter,
`build.sh` marche déjà tel quel avec `mymindstorm/setup-emsdk` puisqu'il
retombe sur `emcc` dans le `PATH` si `$HOME/emsdk` est absent.
**Attention compat** : `sparse.mmap` passe en format v3, un binaire 3.0.5 ne
lira pas un index écrit par 3.0.6.

3.0.5 le 26 au soir : **binaires pour cinq plateformes** par
`.github/workflows/release.yml` (matrice maturin + cargo, Linux dans
manylinux_2_28, Intel macOS cross-compilé depuis `macos-14` — `macos-13`
n'a plus de runner), GitHub Release `v3.0.5` avec les 11 artefacts, PyPI
par **trusted publishing** (OIDC, plus de token), npm à la main cette fois
(le token « bypass 2FA » ne contournait rien → `EOTP` ; et npm a refusé le
nom `lucivy-win32-x64-msvc` pour « spam », d'où `lucivy-windows-x64`).
Prochaine version : configurer le trusted publisher npm sur les 6 paquets
(ils existent maintenant) et supprimer `NPM_TOKEN`. Les jobs de
publication attendent l'approbation de l'environnement `release` **et**
la variable **de dépôt** `PUBLISH_ENABLED=true` (pas d'environnement : le
`if:` d'un job ne voit pas celles-là). Page de présentation + troncature
signalée dans la même version (voir `CHANGELOG.md`, `RELEASE.md`).
3.0.4 le 26 à midi : recherche filtrée = vrai pré-filtre (regex sur 10 ids
126 → 4 ms), stack overflow du look-ahead corrigé. 3.0.3 la même nuit : palier fuzzy = distance vérifiée, playground mobile, index
à moitié écrit reconstruit, montage OPFS. 3.0.2 juste avant : CI verte, bornes
mémoire côté requête, paliers fuzzy raccordés (voir `CHANGELOG.md`). Avant : 3.0.0 puis 3.0.1 le même soir : les crates 3.0.0 étaient partis avant deux
correctifs du cœur (interblocage lazy sans `blob_len`, message de
finalisation perdu) et avec leurs README 2.x. Leçon : **publier les crates en
dernier**, après les bindings et les README, pas en premier.

Précédent : `ld-lucivy` / `lucivy-core` 2.0.0, `luciole` / `lucistore` 0.1.0,
PyPI 2.0.1, npm 2.0.2. Tokens de publication : `.vault/` (ignoré par git),
`source .vault/load.sh` ; npm demande un OTP en direct.

Règle : **tout le workspace porte le même numéro** (décision du 25 août) —
une seule version à retenir pour les utilisateurs.

Ordre de publication crates.io : `luciole` → `lucistore` → `ld-lucivy` →
`lucivy-core` (→ `sparse-vector`). Jamais de `cargo publish` sans le feu vert
explicite de Lucie.
