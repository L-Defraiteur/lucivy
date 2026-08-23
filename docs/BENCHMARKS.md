# Benchmarks et vérité terrain — mode d'emploi

Document principal pour lancer les mesures du moteur SFX v3. Tout ce qui est écrit
ici a été utilisé tel quel le 23 août 2026 ; si une commande ne marche plus, c'est le
code qui a changé, pas le document : le corriger ici en même temps.

Règle d'or des sessions précédentes, trois fois vérifiée : **mesurer avant
d'expliquer, reproduire en secondes avant de théoriser, et comparer des spans
(octets) contre le disque, jamais des documents**.

## 1. Corpus

| Corpus | Chemin attendu | Taille | Usage |
|---|---|---|---|
| rag3db (défaut) | `/tmp/rag3db-bench` | ~4 600 fichiers | panel rapide, 5-45 s |
| Linux kernel | `/tmp/linux-bench` | 95 730 fichiers | montée en volume (5k, 50k) |

```bash
git clone --depth=1 https://github.com/L-Defraiteur/rag3db.git /tmp/rag3db-bench
git clone --depth=1 https://github.com/torvalds/linux /tmp/linux-bench
```

`V3_CORPUS=/chemin` change le corpus, `V3_MAX_DOCS=n` le nombre de fichiers (pris
dans l'ordre du parcours, fichiers ≤ 100 Ko).

## 2. Le harnais : `v3_ground_truth_contains`

Un seul test fait contains strict/relaxed, fuzzy et regex, avec la **vérité terrain
par spans lue depuis le disque** pour chaque requête :

- strict : toutes les occurrences (chevauchantes) en minuscules ASCII ;
- relaxed : idem sur le texte sans séparateurs, remappé aux octets source ;
- fuzzy : `fuzzy_spans` (la définition partagée moteur/harnais, `briques/fuzzy_spans.rs`)
  sur le texte sans séparateurs ;
- regex : `regex::Regex` insensible à la casse, `find_iter` (leftmost-first,
  non recouvrant).

Les spans sont **assertés** : un manquant ou un en-trop fait échouer le test.
`V3_SPANS_REPORT_ONLY=1` repasse au critère « ensemble de documents » (pour
diagnostiquer sans bloquer).

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/gt.txt 2>&1
grep -E "strict|relax|fz|rx|test result" /tmp/gt.txt
```

Toujours rediriger vers un fichier puis `grep` : jamais `| tail` (le test écrit des
milliers de lignes, et la sortie luciole en cas d'attente est verbeuse).

### Requêtes : `V3_QUERIES`

```
V3_QUERIES='valeur:mode,valeur:mode,...'
```

| mode | sens |
|---|---|
| `strict` | contains, séparateurs respectés |
| `relax` | contains, séparateurs ignorés |
| `fz1` `fz2` `fz3` | fuzzy (Levenshtein ≤ d), toujours relaxed |
| `rx` | regex (syntaxe du crate `regex`) |
| `sw` / `sws` | startsWith (le match commence un mot), relaxed / strict |
| `term` / `terms` | mots entiers (startsWith + fin de mot), relaxed / strict |

Le mode est ce qui suit le **dernier** `:`, donc `std::[a-z_]+_ptr:rx` va bien.
Les blancs sont avalés par le trim : écrire `\s` (espace), `\t`, `\n`. Exemple de
panel complet :

```bash
V3_QUERIES='zzqqxxyyww:strict,kmalloc:strict,spin_lock:strict,include:strict,__init:strict,uint64_t:relax,__init:relax,kmallc:fz1,inclde:fz1,kmalloc:fz2,/\*[^*]*\*/:rx,[0-9]{8}:rx,\t\t:strict'
```

**Toujours une requête sans résultat** (`zzqqxxyyww`) dans un panel : c'est le
plancher (ouverture des segments, prescan). Si ce chiffre bouge, aucun compteur
interne ne le dira.

### Lecture de la sortie

```
include   strict   36824   36824   OK (58.1ms search, 240.2ms +fetch, 333.5ms grep) spans 214692 exact
```

- colonnes : requête, mode, docs grep, docs v3, statut ;
- `search` = le moteur seul ; `+fetch` = récupération des documents par le harnais
  (hors moteur) ; `grep` = la vérité terrain depuis le disque, même tâche ;
- `spans N exact` ou `spans gt=… v3=… miss=… extra=…` suivi des 3 premiers spans
  manquants / en trop **avec contexte et chemin de fichier**.

Sous `V3_PROFILE=1`, chaque requête ajoute :

```
[prescan] 800 segments, scatter DAG wall 47.0ms, per-segment CPU sum 670.1ms, max 3.5ms, peak concurrency 24 | ...
  stage totals ... (contains : single / chunk walk / sibling / resolve / word ...)
  fuzzy: resolve … chains … window … dp … | hits= regions= windows= rejected= window_postings= derive_miss= spans=
```

`peak concurrency` doit être ≈ nombre de cœurs ; `max` ≈ `wall` signifie qu'un
segment domine ; `derive_miss` doit rester 0. Le regex réutilise les compteurs
fuzzy (`dp` = temps regex).

## 3. Forme de l'index : commits, merges, cache

| variable | effet |
|---|---|
| `V3_COMMIT_EVERY=500` | docs par commit (défaut 500 ; un commit = un segment par thread d'indexation) |
| (rien) | `NoMergePolicy` : index « naturel », beaucoup de petits segments — **le plus rapide mesuré** |
| `V3_POLICY=1` | la policy du writer fusionne au commit, plafond 10 000 docs/segment (`LucivyHandle`) — l'index « réel » |
| `V3_MERGE=1 V3_MERGE_TARGET=32 V3_MERGE_GROUP=8` | le harnais pilote lui-même les merges (ancienne méthode ; a produit un segment de 48 078 docs + miettes, à éviter) |
| `V3_INDEX_DIR=/tmp/v3idx_x` | cache de l'index sur disque ; réutilisé si corpus, taille, commits, policy et version de format (`v=N` dans la clé) sont identiques, sinon reconstruit. **Incrémenter `v=` dans `index_shape_key` à chaque changement de format** |
| `V3_SHARDS`, `V3_THREADS`, `V3_LIMIT` | sharding, threads, plafond de résultats |

Construction 50k : ~65 s (en RAM puis copie sans fsync — btrfs+zstd fait 65 ms par
fsync, 25 fichiers par segment). Réutilisation : 0 s.

Un segment de 50 000 docs kernel consomme 83 % des 16,7 M d'ordinaux adressables
et coûte 13× plus sur `include` que 800 petits segments : ne pas fusionner vers un
segment unique (le builder refuse au-delà de la limite, proprement).

## 4. Les trois panels de référence

```bash
# rag3db, 45 s, panel par défaut (15 requêtes contains)
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/gt_rag.txt 2>&1

# kernel 50k naturel (cache), 70 s la première fois puis ~10 s
V3_INDEX_DIR=/tmp/v3idx_50k_nat V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 V3_COMMIT_EVERY=500 V3_PROFILE=1 \
V3_QUERIES='zzqqxxyyww:strict,kmalloc:strict,spin_lock:strict,net_device:strict,include:strict,__init:strict,kmalloc:relax,uint64_t:relax,__init:relax' \
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/gt_50k.txt 2>&1

# kernel 50k sous policy (l'index réel), ~75 s la première fois
V3_INDEX_DIR=/tmp/v3idx_50k_pol V3_POLICY=1 V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 V3_COMMIT_EVERY=500 V3_PROFILE=1 \
V3_QUERIES='…' cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture > /tmp/gt_50k_pol.txt 2>&1
```

Chiffres de référence (23 août 2026, 24 cœurs, spans exacts partout) :

| kernel 50k naturel | search |
|---|---|
| requête vide | 29 ms |
| `kmalloc` / `spin_lock` / `__init` strict | 28-32 ms |
| `include` strict (36 824 docs, 214 692 spans) | 55 ms |
| `uint64_t` / `__init` relax | 40 / 63 ms |
| `kmallc` / `inclde` / `uint64` fz1 | 71 / 142 / 67 ms |
| `kmalloc` fz2 | 201 ms |
| `/\*[^*]*\*/` rx (421 036 spans) | 191 ms |
| `[0-9]{8}` rx (sans littéral, balayage complet) | 190 ms |

Policy 10k : `include` 79 ms, `__init` relax 85 ms.

## 5. Modes de recherche comparables

| variable | valeurs | sert à |
|---|---|---|
| `V3_FUZZY_MODE` | `auto` (défaut), `pieces`, `pivot`, `ngram` | générateur de candidats fuzzy — mêmes spans attendus dans les quatre |

## 6. Tests rapides (secondes) à lancer avant tout commit moteur

```bash
cargo test --release --lib                                  # 3 échecs pré-existants connus (fixtures mortes)
cargo test --release -p lucivy-core --test test_sfx_v3_pipeline   # 25 tests, 3 s
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth  # rag3db, 45 s
#   dont v3_ground_truth_coherence : panel fixe « requêtes de RAG » (littéraux longs à
#   séparateurs, sw/term, typos dedans, accents, CJK, emoji/ZWJ), 32 lignes, ~10 s
cargo test --release -p lucivy-core --test test_fuzzy_ground_truth --test test_fuzzy_monotonicity
cargo test --release -p luciole --lib
```

Dans `test_sfx_v3_pipeline.rs`, les tests qui ont trouvé les bugs de la journée :

- `v3_merge_equals_fresh_by_spans` — A∪B frais contre merge(A,B) à deux niveaux,
  spans par document (`V3_MERGE_DOCS`, `V3_CORPUS`) ;
- `v3_policy_merges_preserve_everything` — fusions par la policy pendant
  l'indexation ;
- `v3_word_shapes_share_key_not_ordinal`, `v3_span_non_ascii_neighbours`,
  `v3_fuzzy_span_inside_long_token`, `v3_relaxed_sku_corpus_matches_grep` ;
- outils `#[ignore]` : `v3_merge_bisect` (delta-debugging : `V3_BISECT_TARGET`,
  `V3_BISECT_QUERY`, `V3_BISECT_GREP`), `v3_merge_repro_files` (`V3_REPRO_FILES`),
  `v3_a2_probe`, `v3_a2_chunks`.

## 7. Diagnostics

| variable | sortie |
|---|---|
| `V3_PROFILE=1` | compteurs par étape, lignes `[prescan]`, `[fst]`, `[merge]` |
| `LUCIVY_VERBOSE=1` | commits, policy (`[segment_updater] policy: …`), finalize |
| `V3_DIAG_LITERAL=mot` (+ `V3_DIAG_BYTE=n`) | chaînes et matchs du contains pour cette requête |
| `V3_DIAG_FUZZY=1` | fenêtres, rejets, pièces choisies (très verbeux sur 800 segments) |
| `V3_DIAG_REGEX=1` | littéraux, fenêtres, documents entiers |
| `V3_DIAG_RESOLVE=1` | résolution posmap |

## 8. Méthode qui a marché

1. Requête vide d'abord (plancher).
2. Spans contre le disque, assertés ; `V3_SPANS_REPORT_ONLY=1` pour explorer.
3. Un écart → réduire : `v3_merge_bisect` ramène 600 fichiers à 1-3 en secondes ;
   `v3_a2_probe` coupe un texte caractère par caractère ; `v3_a2_chunks` dumpe le
   tokenizer. Théoriser seulement après.
4. Un correctif → les quatre modes (strict, relax, fz, rx) sur rag3db, puis 50k,
   puis le test fusionné = frais.
5. Commit intermédiaire avec les chiffres avant/après dans le message.
