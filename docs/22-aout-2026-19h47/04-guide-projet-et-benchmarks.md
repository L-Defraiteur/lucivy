# Guide projet et benchmarks — état au 22 août 2026

> Ce que la prochaine session doit savoir pour reprendre sans redécouvrir.
> Complète `03-recap-session-et-pistes-optim.md`, qui contient le récit et les pistes.

---

## 1. Environnement

### Toolchain

Rust n'était pas installé sur cette machine. Installé via rustup, **hors PATH** :

```bash
~/.cargo/bin/cargo --version    # 1.98.0
```

Utiliser le chemin complet, ou `source ~/.cargo/env`.

### Deux irritants sans rapport avec v3

- `bindings/python` ne compile pas : PyO3 0.24.2 plafonne à Python 3.13, la machine a
  3.14. Contourner avec `--exclude lucivy` ou `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`.
- Un bench de `ld-lucivy-bitpacker` exige nightly (`#![feature]`).

Donc `cargo check --workspace` échoue toujours un peu. Ce n'est pas une régression.

### Corpus

| Corpus | Chemin | Taille | Usage |
|---|---|---|---|
| rag3db | `/tmp/rag3db-bench` | 5 616 fichiers | défaut, ground truth historique |
| Linux kernel | `/tmp/linux-bench` | 95 730 fichiers, 2,1 Go | montée en volume |

```bash
git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench
git clone --depth=1 https://github.com/torvalds/linux /tmp/linux-bench
```

**Attention** : le corpus rag3db a évolué depuis mai. Les comptes grep ont beaucoup bougé
(`functin` 62 → 5, `uint64` 243 → 34). Aucune comparaison avec les chiffres des docs de
mai n'est à périmètre constant.

---

## 2. Les tests qui comptent

| Test | Ce qu'il vérifie | Assertions |
|---|---|---|
| `cargo test --lib` | 1426 tests unitaires | oui — 3 échecs préexistants, voir §6 |
| `v3_ground_truth_contains` | contains strict/relax contre grep | **oui** |
| `baseline_fuzzy_regex` | fuzzy et regex contre brute force | non, rapport seul |
| `regex_v2_vs_v3` | même patterns en v2 et v3 | non, rapport seul |
| `v3_distributed_two_nodes` | multi-machine, union = nœud unique | **oui** |
| `v3_merge_preserves_results` | index fusionné == non fusionné | **oui** |
| `perf_shape_executor` | l'exécuteur multi-thread ne sert à rien ici | non |
| `perf_shape_sharded` | latence par nombre de shards | non |
| `test_sfx_v3_pipeline` | E2E v3 | oui — 1 ignoré |

Toutes les commandes en **`--release`** : le facteur debug est de 7 à 11×, et tous les
chiffres de perf des docs antérieurs à cette session ont été pris en debug sans le dire.

```bash
~/.cargo/bin/cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth -- --nocapture
```

Convention du projet : rediriger, ne jamais `| tail`.

```bash
... > /tmp/run.txt 2>&1
```

---

## 3. Piloter les benchmarks

Tout se pilote par variables d'environnement, sans toucher au code.

| Variable | Défaut | Effet |
|---|---|---|
| `V3_CORPUS` | `/tmp/rag3db-bench` | racine du corpus |
| `V3_MAX_DOCS` | 5000 (contains), 500 (baseline) | nombre de documents |
| `V3_COMMIT_EVERY` | 500 | documents par commit → **contrôle le nombre de segments** |
| `V3_MERGE` | absent | active la fusion par paliers |
| `V3_MERGE_TARGET` | 1 | nombre de segments visé |
| `V3_MERGE_GROUP` | 8 | segments fusionnés par palier |
| `V3_QUERIES` | jeu rag3db | `valeur:strict`, `valeur:relax`, séparés par des virgules |
| `V3_LIMIT` | taille du corpus | plafond de résultats (aucun par défaut) |
| `V3_THREADS` | 1 | threads de l'exécuteur de recherche |
| `V3_SHARDS` | `1,4,8` | nombre de shards comparés (`perf_shape_sharded`) |
| `RECOMPUTE_GT` | absent | recalcule le cache de ground truth |
| `V3_INDEX_DIR` | absent | persiste l'index (construit en RAM, copié sans fsync) et le rouvre en mmap aux runs suivants ; clefé sur la forme, invalidé par `.v3_shape` |
| `V3_MERGE_AT_END` | absent | désactive le merge progressif pendant l'indexation |
| `V3_PROFILE` | absent | compteurs par étage (`briques/profile.rs`), timings `[prescan]`, `[merge]`, `[fst]` |
| `V3_DIAG_LITERAL=<query>` | absent | dump des chaînes et des matchs (`[lit]`, `[match]`) pour cette requête |
| `V3_DIAG_BYTE=<n>` | absent | restreint `[match]` au `byte_from` donné |
| `LUCIVY_VERBOSE` | absent | résumés de DAG par nœud, timings `[finalize]` — fonctionne depuis `83d9695` |
| `QUERY`, `MODE` | absent | filtre le baseline fuzzy/regex |

Le cache de ground truth est **clefé par corpus et par taille** : réutiliser celui d'un
autre arbre ferait passer un run au vert pour rien.

Depuis le 23 août, la vérité terrain de `v3_ground_truth_contains` lit chaque fichier
**depuis le disque** et compare **tous les spans** (strict et relaxed). La ligne de
sortie est `(search, +fetch, grep) spans gt=… v3=… miss=… extra=…` ; les documents
restent le critère de passage, les spans sont rapportés. Mettre une requête **sans
résultat** dans chaque panel : c'est la mesure du plancher (29 ms à 50k).

### Diagnostics

| Variable | Ce qu'elle imprime |
|---|---|
| `V3_DIAG_LITERAL=<query>` | par étage : matches single, chaînes chunk, textes des tokens de chaque chaîne |
| `V3_DIAG_REGEX=1` | littéraux extraits, gaps analysés, matches par littéral, intersection, ventilation des rejets |
| `V3_DIAG_FUZZY=1` | candidats / gardés / rejetés, avec fenêtre et aiguille des premiers rejets |
| `V3_DIAG=1` | explain DAG par segment + forensics de document |
| `V3_DIAG_COLLECTOR=<mot>` | chaque intern/posting/ordinal contenant le mot |
| `V3_DEBUG_QUERY=<query>` | trace détaillée du prescan |

**C'est l'instrumentation qui a résolu tous les bugs de la session, jamais le
raisonnement.** Cinq hypothèses fausses ont été éliminées par les compteurs. En cas de
doute, instrumenter avant de supposer.

### Recettes

Reproduire l'état de référence :

```bash
~/.cargo/bin/cargo test --release -p lucivy-core \
  --test test_sfx_v3_ground_truth -- --nocapture > /tmp/gt.txt 2>&1
# attendu : 15 pass 0 fail (contains), 9/11 (baseline)
```

Monter en volume sur le kernel, avec des requêtes qui y existent vraiment :

```bash
V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 \
V3_QUERIES='spin_lock:strict,kmalloc:strict,EXPORT_SYMBOL:strict,struct file:strict,net_device:strict,mutex_unlock:strict' \
~/.cargo/bin/cargo test --release -p lucivy-core \
  --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/big.txt 2>&1
```

Le jeu de requêtes rag3db par défaut (`rag3db`, `std::unique_ptr`, `ku_dynamic_cast`)
rend **0 hit** sur le kernel et ne mesure rien.

Tracer la courbe segments / latence — la mesure la plus rentable qui reste :

```bash
for seg in 250 500 1000 2000 4000; do
  echo "=== commit_every=$seg ==="
  V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=20000 V3_COMMIT_EVERY=$seg \
  V3_QUERIES='spin_lock:strict,kmalloc:strict,net_device:strict' \
  ~/.cargo/bin/cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture 2>&1 \
    | grep -E "index shape|^(spin_lock|kmalloc|net_device) "
done
```

Ou par fusion, à indexation constante :

```bash
for target in 1 8 24 64; do
  V3_MERGE=1 V3_MERGE_TARGET=$target V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=20000 \
  V3_QUERIES='spin_lock:strict,kmalloc:strict' \
  ~/.cargo/bin/cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture 2>&1 \
    | grep -E "merge \(tiered|^(spin_lock|kmalloc) "
done
```

Sharding :

```bash
V3_SHARDS=1,2,4,8,16 ~/.cargo/bin/cargo test --release -p lucivy-core \
  --test test_sfx_v3_ground_truth perf_shape_sharded -- --nocapture
```

---

## 4. Points mesurés à connaître avant d'optimiser

### Moins de segments = plus lent

Le segment est l'unité de parallélisme du prescan. Mesuré sur 20 000 documents du kernel :

| Segments | `spin_lock` strict | `kmalloc` strict |
|---|---|---|
| 320 | 159 ms | 136 ms |
| 32 | — | 39 ms |
| 8 | — | 105 ms |
| 1 | 7 376 ms | 400 ms |

Fusionner à 1 coûte 40 à 49× sur les requêtes sélectives. **Le merge sert à borner le
nombre de segments, pas à le minimiser.**

### L'exécuteur multi-thread ne sert à rien

`ContainsQueryV3::weight()` appelle `prescan_segments`, qui fait tout le travail, et
`weight()` s'exécute **avant** `executor.map` dans `Searcher::search_with_executor`. Un
exécuteur multi-thread parallélise donc une phase qui ne coûte rien : mesuré **1.0×** sur
80 segments et 24 threads. Le parallélisme utile est celui que `prescan_segments` fait
lui-même, via `luciole::scatter::build_scatter_dag`.

### Aucun merge ne se déclenche automatiquement

`segment_updater_actor.rs:135` : « Merges are deferred — they run when drain_merges() or
start_merge() is called explicitly. » Et `drain_merges()` se contente d'attendre ceux déjà
en vol. Un index construit via `LucivyHandle` ne fusionne **jamais** tout seul.

### Le fan-out luciole dégrade proprement

`execute_dag` exécute tous les nœuds inline quand il détecte un thread scheduler, un
handler d'actor ou un cooperative wait (`runtime.rs:310-315`). Le prescan shardé, qui
tourne déjà dans un actor, ne peut donc pas imbriquer de pool. C'est pour ça que le
fan-out a été **aplati** en un nœud par (shard, segment) : les deux niveaux de
parallélisme composent au lieu de s'annuler.

**Ne jamais introduire de `thread::spawn`** — tout doit passer par luciole, sous peine de
casser le build WASM.

---

## 5. Architecture v3, l'essentiel

### Trois partitions FST

| Partition | Contenu | Postings | Coordonnées |
|---|---|---|---|
| `0x00` | SI=0, début de token | `.sfxpost` | chunk-level |
| `0x01` | SI>0, suffixes | `.sfxpost` | chunk-level |
| `0x02` | word-stripped | `.word_sfxpost` | word-level |

**L'invariant central** : chunk et word-stripped peuvent partager un texte alors que
leurs postings vivent dans des fichiers différents avec des sémantiques différentes.
Toute structure clefée sur le texte seul fusionne les deux — c'est la « fuite de
partition » que ce projet paie depuis mai.

Le tag `is_word_stripped` est désormais **persisté dans `TTX3`** (octet `reserved`, coût
zéro). C'est ce qui rend le merge par remap possible.

### Sémantique des champs de `MatchV3`

| Champ | Signification |
|---|---|
| `byte_from` | début du **match** |
| `byte_to` | fin du **match** |
| `token_end` | fin du **token conteneur** — lu uniquement par `exact_match` |
| `span` | nombre de **positions de tokens**, toujours `last_position - position + 1` |

Ces quatre-là portaient chacun deux ou quatre sémantiques avant cette session.

### Le principe qui a tout débloqué : vérifier, pas filtrer

Regex, fuzzy et contains acceptaient sans preuve. Les trois utilisent maintenant
`posmap` + `termtexts` pour reconstruire le texte et vérifier, **sans toucher au
docstore** :

- contains : le texte contient-il le littéral
- fuzzy : DP semi-globale de distance d'édition, fenêtre bornée
- regex : le vrai pattern, uniquement sur les chemins approximatifs (`AcceptAnything`,
  `ByteRangeCheck`)

Corollaire : la récupération peut être **lâche**. Le seuil pigeonhole et les tolérances
de gap peuvent être relâchés — la vérification tranche.

### Casse

Le FST est construit en minuscules (`builder_v3.rs:248`) mais `termtexts` **conserve la
casse d'origine** (`collector_v3.rs:255`). Donc : récupération insensible à la casse
(sur-ensemble), vérification sensible si on le veut. Le regex a choisi la sémantique
exacte, le fuzzy reste insensible (contrat de la ground truth).

---

## 6. Les 3 tests unitaires rouges

| Test | Cause |
|---|---|
| `diag_false_positive_uint64t` | fixture morte depuis le 19 mai — appelle le pipeline avec `None` pour les maps |
| `test_resolve_chain_sep_skip` | idem — `resolve_chains_v3` en direct sans pipeline word |
| `test_into_data_sorted` | casse connue du WIP de juin — `tokens` passé de `BTreeSet` à `Vec` |

Aucun n'est une régression de cette session. Documentés dans
`docs/19-mai-2026/05-recap-session-5-complete.md:46-48` pour les deux premiers.

Plus `v3_term_is_whole_token_not_prefix`, marqué `#[ignore]` : régression située par
bisect à `8aeb093`, `term` n'étant pas sur le chemin critique du code RAG.

---

## 7. Pièges à ne pas retomber dedans

1. **Ne jamais planifier sur un score cité dans un doc.** Trois documents de mai se
   contredisaient ; seule la mesure a tranché.
2. **Toujours vérifier la forme de l'index** avant de lire un timing : nombre de
   segments, shards, exécuteur. Le harnais l'imprime (`index shape:`).
3. **Toujours mesurer en release.** Facteur 7 à 11×.
4. **Vérifier que le corpus contient les requêtes.** Un test vert sur 5 hits ne prouve
   rien.
5. **Vérifier qu'un merge a réellement eu lieu** avant de conclure sur un index fusionné.
   Une première version du test passait au vert sur 64 → 64 segments.
6. **Instrumenter avant de supposer.** Cinq hypothèses fausses dans la session, dont deux
   justes mais qui n'avaient d'effet que combinées à une autre.
7. **Ne pas conclure d'une lecture statique qu'un code est inoffensif.** L'analyse du
   matin l'avait fait pour le code des siblings en strict ; 50 000 documents l'ont
   réfutée.
