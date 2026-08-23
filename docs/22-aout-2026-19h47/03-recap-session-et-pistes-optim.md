# Récapitulatif de session — 22 août 2026

> Branche `v3-recovery`. Reprise après changement de machine.
> Point de départ : toolchain absent, corpus absent, dernière mesure datant du 2 juin.

---

## 1. Ce qui a été fait, dans l'ordre

### Remise en route

Rust n'était pas installé sur la machine (`~/.cargo` absent, rien dans pacman), et le
corpus `rag3db` non plus. Installé rustup 1.98.0, recloné le corpus. **Le WIP du 2 juin
n'avait jamais été compilé** — il l'est, et sans erreur : les seuls échecs du workspace
sont `bindings/python` (PyO3 0.24.2 plafonne à Python 3.13, la machine a 3.14) et un
bench `bitpacker` qui exige nightly.

### Mesures d'entrée, qui contredisaient les docs

| | Docs de mai | Mesure réelle |
|---|---|---|
| Contains | 13/15, « 2 fails restants » | **15/15** |
| Fuzzy | 0/6 | 2/6 |
| Regex | jamais mesuré | 2/5 |

Le rapport de session 7 disait 13/15, le findings du 30 mai décrivait 2 fails, le doc
fuzzy du 1er juin affirmait « validée à 15/15 ». Les trois ne pouvaient pas être vrais
ensemble. **On a raisonné trois mois sur un score périmé.**

### Les dix vérités dichotomiques (doc 02) — toutes traitées

| # | Correction | Commit |
|---|---|---|
| 1 | Garde de merge `sfx_version >= 3` | `36d7cae` |
| 2 | `has_word_pipeline()` exige `num_ordinals > 0` | `36d7cae` |
| 3 | `merge_segments_v3` renommée et documentée honnêtement | `36d7cae` |
| 4 | `query_content_len` en octets et non en chars | `fcf21c0` |
| 6 | `byte_to` = fin du match, `token_end` = fin du conteneur | `fcf21c0` |
| 10 | Doc-comments menteurs | `fcf21c0` |
| 7 | Unité de `span` unifiée, post-filtre mort retiré | `8aeb093` |
| 8 | `resolve_single_word_v3` câblé dans `find_literal_v3` | `8aeb093` |
| 9 | Bloc explain mort supprimé (174 lignes) | `c464ddc` |
| 5 | Tag de partition persisté dans `TTX3` | `35bc2d6` |

### Perf du contains

| Étape | Effet |
|---|---|
| `prescan_segments` parallélisé via luciole (`665a516`) | `include` 1273 → 191 ms |
| Fan-out aplati par (shard, segment) (`3bc978f`) | `include` 191 → 166 ms, `function` 36 ms |

Avant : `uint64_t` relax à 1717 ms. Après : 110 ms. Sans sharding.

### Trois angles morts de couverture

- **v3 + sharding n'avait jamais été câblé** (`1281d70`). Les deux benchs shardés ne
  fixent pas `sfx_version`, donc tournent en v2. Le DAG shardé ouvrait tout `.sfx` avec
  le reader v1/v2 : échec immédiat sur `invalid .sfx magic bytes`.
- **v3 + distribué n'avait jamais tourné** (`6bc89ef`). Seul `acid_postgres.rs` teste ce
  chemin, `#[ignore]` et en v2. `ContainsQueryV3::make_weight` ignorait le fournisseur
  de stats globales — le distribué v3 était donc cassé.
- **Le regex n'avait jamais été mesuré sur v3.** `test_regex_ground_truth.rs` construit
  son index avec `SchemaConfig { ..Default::default() }`, donc v2.

### Correction : la même réponse structurelle trois fois

Regex, fuzzy et contains avaient **la même racine** : le pipeline accepte sans preuve.

| Moteur | Ce qui a été fait | Résultat |
|---|---|---|
| Regex | leftmost match (`031b498`), `ByteRangeCheck` implémenté + vérification du pattern réel (`6aa30bf`) | 2/5 → 5/10 exacts, 535 FP éliminés |
| Fuzzy | vérification Levenshtein (`d21775f`), slack séparateur + chaînes multiples (`48576e9`) | 2/6 → **6/6 exact** |
| Contains | vérification du littéral (`fc79372`) | 6/10 → 9/10 sur 50k docs kernel |

Les trois vérifications utilisent `posmap` + `termtexts`, **sans jamais toucher au
docstore**.

### Passage à l'échelle

Corpus kernel Linux cloné (`/tmp/linux-bench`, 95 730 fichiers, 2,1 Go). Trois bugs que
5 000 documents ne montraient pas :

- **`TableFunction` strict** matchait `migra|table function|` — le supplément de chaînes
  par siblings tournait sans condition sur `strict_separators` (`c04ff19`).
- **`__init` strict** rendait 13 275 documents pour 4 742 — la chaîne enjambait un token
  intermédiaire entier. Réglé par la vérification.
- **Le plafond de 10 000 résultats** transformait toute requête à fort rappel en échec.

### Merge v3

Bloqué le matin (corruption silencieuse via le DAG v2), puis débloqué en deux temps :
ré-indexation (`26c80b0`), puis remap (`2a4c375`) parce que la ré-indexation coûtait
18 Go pour fusionner 50 000 documents.

---

## 2. État mesuré en fin de session

| Axe | Valeur | Corpus |
|---|---|---|
| Contains | 15/15 | rag3db 5k |
| Contains | 9/10 | kernel 50k, 800 segments |
| Fuzzy | 6/6 exact | rag3db 500 |
| Regex | 5/10 exacts | rag3db 2k |
| Baseline globale | 9/11 | rag3db 500 |
| `cargo test --lib` | 1426 passed, 3 failed, 16 ignored | |

Les 3 échecs unitaires : deux fixtures mortes depuis le 19 mai (elles passent `None`
pour les maps), une casse connue du WIP (`tokens` passé de `BTreeSet` à `Vec`).

---

## 3. Le nombre de segments — ce que j'avais conclu, et ce qui était vrai

**La conclusion écrite ici était fausse.** Elle disait : « moins de segments rend les
requêtes plus lentes », le segment étant l'unité de parallélisme du prescan.

Les mesures d'origine, conservées telles quelles :

| Query | 320 segments | 1 segment | Écart |
|---|---|---|---|
| `spin_lock` strict | 159 ms | 7 376 ms | 46× |
| `struct file` strict | 211 ms | 8 380 ms | 40× |
| `net_device` strict | 161 ms | 7 942 ms | 49× |
| `kmalloc` strict | 136 ms | 400 ms | 3× |
| `__init` strict | 9 264 ms | 28 114 ms | 3× |

Les chiffres sont exacts. L'explication ne l'était pas. Les index à peu de segments
avaient tous été obtenus **par fusion**, et la lenteur ne venait pas de la perte de
fan-out : elle venait d'un coût quadratique en taille de segment dans la résolution de
chaînes, que 320 petits segments masquaient et que la fusion exposait.

La mesure qui l'établit — même corpus, **même nombre de segments**, obtenus des deux
façons (20 000 documents kernel, 32 segments) :

| Query | 32 seg par commits | 32 seg par fusion | |
|---|---|---|---|
| `kmalloc` strict | 58 ms | 308 ms | 5× |
| `include` strict | 157 ms | 2 716 ms | 17× |
| `spin_lock` strict | 123 ms | 3 546 ms | **29×** |

À forme d'index identique, l'écart est entier. Ce n'était donc pas le nombre de segments.

Les trois causes, trouvées par le profilage et corrigées (`4eaf367`) :

1. `resolve_chains_impl` appariait l'ensemble actif avec la **totalité** des postings de
   chaque ordinal. Les deux listes croissent avec le segment → quadratique en taille de
   segment. Corrigé en indexant les postings par document.
2. Les postings étaient matérialisés sans élagage : 264 173 480 entrées sur `spin_lock`
   pour 1,8 million réellement appariées. Corrigé en ne demandant au resolver que les
   documents encore actifs.
3. La position 0, qui n'a aucun ensemble actif pour s'élaguer, était résolue par chaîne
   alors que les chaînes partagent massivement leurs ordinaux de départ : 39 122 783
   postings sur `include`. Corrigé par mémoïsation.

Après correction, sur index fusionné 32 segments / 20k : `spin_lock` 3 546 → 324 ms,
`include` 2 716 → 1 264 ms, `kmalloc` 308 → 148 ms. Et un index fusionné de 1 667
documents par segment tient 333 ms, contre 269 ms pour un index naturel de 568 documents
par segment — l'écart structurel a disparu.

**Ce qui reste ouvert** : à 50 000 documents, un index fusionné à 32 segments met encore
3 526 ms sur `spin_lock`, là où 72 segments naturels du même corpus tiennent 233 ms. Ni
la taille de segment ni le nombre de passes de fusion ne l'expliquent — les deux ont été
testés et écartés. Cause non identifiée.

**La leçon** : trois formes d'index avaient été comparées sans que la variable « fusionné
ou non » soit isolée. Le corollaire qu'on en avait tiré — « le merge sert à borner le
nombre de segments, pas à le minimiser » — reposait donc sur rien.

Et un fait à connaître, celui-là vérifié : **aucun merge ne se déclenche
automatiquement.** `segment_updater_actor.rs:137` diffère les merges à un
`drain_merges()`/`start_merge()` explicite « pour éviter la famine de threads pendant le
commit », et `drain_merges` se contente d'attendre ceux déjà en vol. Un index construit
via `LucivyHandle` ne fusionne jamais tout seul.

---

## 4. Pistes d'optimisation pour la prochaine session

### 4.1 Le résidu de lenteur sur index fusionné à 50k — le plus rentable

Voir la fin de la §3. À corpus et requête identiques, 32 segments fusionnés donnent
3 526 ms contre 233 ms pour 72 segments naturels. Deux explications ont été testées et
écartées : la taille de segment (1 667 documents par segment fusionné tiennent 333 ms à
20k) et le nombre de passes de fusion (l'index rapide en a subi davantage).

La méthode qui a marché trois fois de suite : profiler la forme lente avec `V3_PROFILE=1`
et chercher le compteur qui explose. Les compteurs de `chunk resolve`
(`first-position postings`, `entries`, `pair iterations`) ont désigné les trois causes
précédentes sans ambiguïté.

Coût : ~19 min d'indexation par itération avec merge progressif, ~5 min avec
`V3_MERGE_AT_END=1`. Chercher d'abord une reproduction moins chère.

### 4.2 `__init` — pathologie de requête

Non re-mesuré depuis les correctifs de `1f4d19e` et `4eaf367` : le run 50k qui devait le
faire a été interrompu. Le chiffre ci-dessous est antérieur.

9,3 s à 320 segments contre 160 ms pour les autres requêtes du même corpus. La requête
commence par deux séparateurs, ce qui fait exploser la construction de chaînes : le diag
montrait 308 puis 504 chaînes candidates par segment, contre une poignée ailleurs.

Piste : une requête dont le préfixe est un séparateur ne devrait pas ancrer sur des
tokens pure-séparateur, qui sont légion. Instrumenter `V3_DIAG_LITERAL=__init` et
regarder d'où viennent les splits.

### 4.3 Le pipeline word — voir §5

### 4.4 Le prescan copiait les sidecars par segment et par requête — CORRIGÉ [mesuré]

**Diagnostic.** `prescan_segment_v3` faisait `load("posmap")`, `load("bytemap")`,
`load("word_sfxpost")`, `load("sibling_v3")`, `load("termtexts")` — cinq lectures
terminées par `.to_vec()`.

Il n'y avait là **aucune I/O** : `SegmentReader` détient déjà les `FileSlice` dans
`registry_files` (`segment_reader.rs:101`), et `read_bytes()` sur un handle adossé
à la RAM ou à un mmap ne fait que découper un `Arc` (`ram_directory.rs:84`,
`file_slice.rs:332`). Le `.to_vec()` était donc le coût réel et le seul : une copie
intégrale de chaque sidecar, à chaque segment, à chaque requête.

`MmapDirectory` existe déjà (`src/directory/mmap_directory/`, avec cache de mmap en
weak-ref), mais il n'était pas en cause — le harnais de bench tourne sur
`RamDirectory`. Rien à écrire côté directory.

**Correctif.** Ne plus copier : garder l'`OwnedBytes`. Il implémente
`Deref<Target = [u8]>`, donc les `open(b)` et les `.as_deref()` en aval compilent
inchangés. Appliqué aux trois prescans v3 — contains, fuzzy, regex.

**Mesure** (20 000 fichiers kernel, 320 segments, moyenne sur 2–3 runs ; l'écart
run-à-run est de ~4 ms, très en dessous des gains) :

| Query | avant | après | |
|---|---|---|---|
| `spin_lock` strict | ~161 ms | ~128 ms | **−21 %** |
| `kmalloc` strict | ~143 ms | ~104 ms | **−27 %** |
| `include` strict | ~360 ms | ~329 ms | −9 % |
| `kmalloc` relax | ~1600 ms | ~1590 ms | ≈ 0 |
| `uint64_t` relax | ~1698 ms | ~1674 ms | ≈ 0 |

**La prédiction de cette section était fausse**, et à l'envers. Elle annonçait un gain
« probablement significatif sur le relax, qui charge les cinq fichiers, contre trois
pour le strict ». C'est le **strict** qui gagne 21–27 % ; le relax ne bouge pas d'un
pouce.

Le raisonnement comptait les fichiers chargés au lieu de comparer le coût du
chargement au coût du reste. Sur le strict, la copie pesait un cinquième du temps
total. Sur le relax, les mêmes copies sont noyées dans un travail dix fois plus long
— celui du pipeline word (§5), désormais seul suspect restant. Le résultat resserre
donc §5 : le coût du relax n'est ni l'I/O ni le chargement, il est bien dans le walk.

### 4.5 Le seuil pigeonhole peut être baissé

Passé de `.max(2)` à `.max(1)`, sans effet mesuré parce que `ngrams.len() - n·d` vaut
déjà 3+ sur les requêtes testées. Il ne joue que sur les requêtes très courtes. Depuis
que la vérification est exacte, baisser le seuil ne peut que gagner du rappel.

### 4.6 Le O(n²) de `build_trigram_chains`

Double boucle `start`/`j` par document, et on émet maintenant jusqu'à
`MAX_CHAINS_PER_DOC = 8` chaînes au lieu d'une. Coût mesuré : +40 % sur `inclde` et
`retrun`. Une DP à balayage unique donnerait `O(H·T)` et serait *optimale*, donc
supprimerait aussi le FN dû au premier-arrivé.

Trois gaspillages purs identifiés et non corrigés :
- `fst_candidates_v3` appelé **deux fois par n-gramme** (sélectivité puis résolution)
- hits chunk et word en doublon dans la même `Vec` (×4 sur le quadratique)
- le tri par sélectivité ne sert plus à rien depuis que le `doc_filter` a disparu

### 4.7 `best_bt` — highlights fuzzy faux

`composite.rs` prend la première occurrence globale du dernier trigramme après
`best_bf`, pas celle de la chaîne retenue. `test_fuzzy_ground_truth` est rouge avant
comme après cette session (vérifié par `git stash` sur HEAD).

### 4.8 Le remap de merge charge un segment entier à la fois

Acceptable pour des paliers de 8, à revoir si on fusionne plus large. La première version
chargeait **tous** les fichiers de **tous** les segments avant de commencer : 30 Go.

---

## 5. Le pipeline word n'est pas lent — la mesure l'était [mesuré]

**Cette section disait le contraire, et se trompait entièrement.** Elle annonçait un
relax à ~15× le strict, listait six hypothèses classées par suspicion, et désignait
`intermediates_are_pure_sep` comme suspect principal. Rien de tout cela ne tient.

### La cause : le grep de référence était dans le chronomètre

Le harnais de bench n'avait qu'un seul `Instant`, démarré **avant** le calcul de la
vérité terrain :

```rust
let t = std::time::Instant::now();
let grep_set = if q.strict_sep { grep_docs_strict(..) } else { grep_docs_relaxed(..) };
let v3_result = search_v3(..);
let ms = t.elapsed()...;   // grep + moteur
```

Toutes les latences publiées dans les rapports de session portaient donc un scan complet
du corpus. Et l'écart entre modes n'était pas neutre : `grep_docs_relaxed` applique
`strip_seps` à chaque fichier avant de chercher, là où `grep_docs_strict` ne fait qu'un
`contains`. Le prétendu « ×15 du relax » était cette différence-là.

Séparés (20 000 fichiers kernel, 320 segments) :

| Query | v3 | grep | ce qu'on lisait |
|---|---|---|---|
| `kmalloc` strict | 63 ms | 34 ms | ~150 ms |
| `kmalloc` relax | **60 ms** | **1500 ms** | ~1600 ms |
| `spin_lock` strict | 84 ms | 30 ms | ~160 ms |
| `spin_lock` relax | **72 ms** | 1691 ms | — |

En relax, le moteur est à 60 ms. Sur `spin_lock` il est même **plus rapide** que le
strict. Les deux chronomètres sont désormais distincts et le rapport affiche
`(… ms v3, … ms grep)`.

### Ce que le profilage a réellement montré

Répartition CPU par étage (`V3_PROFILE=1`, cumul sur les segments) :

| | kmalloc str | kmalloc rlx | include str | uint64_t rlx |
|---|---|---|---|---|
| **chunk walk** | **86,0 %** | **78,0 %** | **93,3 %** | **96,6 %** |
| chunk resolve | 6,6 % | 6,5 % | 5,3 % | 1,1 % |
| chunk sibling DFS | 5,2 % | 4,4 % | 1,2 % | 0,4 % |
| single | 2,1 % | 2,6 % | 0,2 % | 0,1 % |
| tout le pipeline word | 0 % | 8,4 % | 0 % | 1,8 % |

Sort de H1 à H6 :

- **H2 (suspect principal) — mort.** `intermediates_are_pure_sep` : 409 appels pour 409
  positions balayées, 11 780 pour 11 780. Exactement une position par appel : la boucle
  ne boucle jamais. Coût nul.
- **H1 — vrai mais négligeable.** Le relax ajoute bien le pipeline word au pipeline
  chunk, mais cet ajout pèse 1,8 à 8,4 %, pas un facteur 2.
- **H3, H5, H6 — sans objet.** Le pipeline word entier tient sous 10 %.
- **H4 — déjà réfutée** en §4.4, pour une raison distincte.

### Le vrai point chaud, et son correctif

`cross_chunk_chain_v3`, via `build_chains_from_splits`, pesait 78 à 96 % du temps moteur
**dans les deux modes**. Le pipeline word n'y était pour rien.

La cause est une redondance. Tout remainder walké par cette boucle est un **suffixe de la
requête** : le premier est `query_lower[safe_start..]` et chaque étape ne rogne que par
l'avant. Il y en a donc au plus `query_lower.len()`, quel que soit le nombre de splits —
mais le walk FST était refait pour chacun des dizaines de milliers de splits :

| query | appels `fst_candidates` | remainders distincts | redondance |
|---|---|---|---|
| `kmalloc` | 11 679 | 756 | 15× |
| `uint64_t` | 56 609 | 2 228 | 25× |
| `include` | 94 414 | 1 212 | **78×** |

Correctif : mémo sur l'offset de début, qui identifie un suffixe de façon unique.

| | chunk walk avant | après | mural avant | après |
|---|---|---|---|---|
| `include` strict | 2675 ms | 213 ms (−92 %) | 303 ms | 147 ms |
| `uint64_t` relax | 2690 ms | 277 ms (−90 %) | 169 ms | 71 ms |
| `spin_lock` strict | 586 ms | 143 ms (−76 %) | 84 ms | 65 ms |
| `kmalloc` strict | 178 ms | 60 ms (−66 %) | 63 ms | 62 ms |

(Les totaux CPU dépassent le temps mural : le prescan est parallélisé par segment via
luciole, ce sont des temps cumulés sur les threads. À lire comme des parts.)

### La leçon

Six hypothèses ont été formulées, classées et argumentées sur un chiffre que personne
n'avait vérifié. Le suspect principal ne consommait rien ; le vrai coupable n'était dans
aucune des six, et n'était même pas dans le pipeline incriminé.

**Avant d'expliquer un écart, vérifier qu'il existe.** Un chronomètre qui englobe la
vérité terrain ne mesure pas le moteur.

---

## 5 bis. Fan-out dans un acteur : la règle exacte [vérifié le 23 août]

`execute_dag` exécute tout inline dès qu'il tourne dans un acteur ou sur un
thread du pool (`luciole/src/runtime.rs`, « avoid thread pool starvation »). C'est ce
qui rendait le merge séquentiel : le DAG de commit a un nœud `merge_i` par opération,
mais il tourne dans `segment_updater`, donc les nœuds s'enchaînent.

La règle protège emscripten, qui n'a que quelques pthreads. Mais ce qu'elle interdit
est plus large que ce qui est dangereux :

- **Dangereux** : fan-out **puis attente bloquante** dans l'acteur. Un thread du pool
  immobilisé à attendre des tâches qui ont besoin du pool → interblocage dès que le
  pool est petit.
- **Sûr** : fan-out **puis continuation**. L'acteur soumet les tâches et rend la main ;
  `collect_replies_to` / `pipe_to` lui renvoie un message quand tout est fini. Aucun
  thread n'attend.

Le merge parallèle (`2eb6426`) est du second type : `handle_start_merges` soumet N
tâches et retourne, `SuMergesDoneMsg` fait la comptabilité. C'est le motif que
l'indexer applique déjà à ses finalizes. Il est donc sûr en emscripten **sur le
papier** — non testé dessus.

**La vraie correction**, plutôt qu'un chemin spécial par DAG : faire passer le DAG de
commit par `execute_dag_async` (le `DagExecutor` niveau par niveau existe), qui donne le
fan-out par continuation à tout DAG exécuté depuis un acteur. Le chemin ajouté pour le
merge en deviendrait une instance générique.

---

## 5 ter. État au 23 août, 14h — mesuré, commité

### Perf, 50k fichiers kernel, 800 segments naturels, index mmap en cache

| Query | moteur | grep depuis le disque |
|---|---|---|
| `spin_lock` strict | 210 ms | 312 ms |
| `include` strict (36 824 docs) | 212 ms | 346 ms |
| `net_device` strict | 190 ms | 296 ms |
| `EXPORT_SYMBOL` strict | 198 ms | 413 ms |
| `__init` strict | 328 ms | 329 ms |
| `kmalloc` relax | 179 ms | 1 080 ms |
| `uint64_t` relax | 208 ms | 1 113 ms |

Ce matin : `__init` 49 s, index fusionné 3,5 s par requête. Le « grep » de la
colonne de droite fait désormais **le même travail** que le moteur — lire chaque
fichier depuis le disque, trouver toutes les occurrences en spans d'octets — et non
un `contains` sur un `Vec` préchargé, qui était la comparaison précédente (et qui
donnait 90 ms, d'où l'impression d'être « plus lent que grep »).

Index fusionné à 32 segments : `include` 999 ms, `spin_lock` 256, `__init` 340,
`net_device` 212 — au niveau du naturel sauf `uint64_t` relax (1 119 contre 251).

### Correction : les highlights étaient faux (`456bd58`)

Les documents étaient exacts ; les spans ne l'étaient pas, et rien ne les vérifiait.
Trois bugs, dont un ancien : une seule occurrence émise par document sur les chaînes
(`position` d'émission écrasée par le dedup), fins tronquées à la frontière de chunk
(clamp sur le contenu propre alors que l'overlap est du texte), relaxed s'arrêtant
avant le token suivant (l'overlap 0x02 est après des séparateurs → `overlap_overflow`
placé via posmap).

Panel rag3db : 9 requêtes sur 15 exactes au span près. 50k kernel : `include`
214 689 / 214 692, `net_device` et `kmalloc` exacts.

**Résidus connus, non traités** :
- Occurrences manquantes en fin de fichier ou devant un caractère non-ASCII
  (`rag3db` 144 / 15 128 ; `function`, `return`, `struct` 1-2). Hypothèse : l'overlap
  de 2 octets coupe un caractère UTF-8 de 3 octets.
- Spans qui **démarrent trop tôt**, sur un séparateur antérieur, quand la tête de
  chaîne est un token finissant par un séparateur : `__init` 1 404 / 18 149
  (`>>_vapor __init<<`) ; relaxed `uint64_t` 72 (`>>;\n    uint64<<`). C'est
  `byte_from_first = e.byte_from + first_sti` qui ne pointe pas sur le dernier
  séparateur du token.

### Harnais

- `V3_INDEX_DIR` : index construit en RAM, copié sans fsync, rouvert en mmap
  (`cdd577d`). 50k : 64 s à construire, 0 s à rouvrir. Sur ce disque (btrfs+zstd) un
  fsync coûte 65 ms, et MmapDirectory en fait un par fichier, 25 par segment.
- Merge parallèle (`2eb6426`) : `IndexWriter::merge_many`, fusions en tâches
  luciole, réponse par continuation. 10k : 18,9 → 5,6 s.
- Le merge **progressif** du harnais reste mauvais : 660 s sur 886 s du run 50k
  fusionné, parce qu'il re-fusionne sans cesse des segments moyens. La
  `LogMergePolicy` existe et n'est jamais consultée (`handle_commit` diffère tout).
- Le rapport affiche `(search, +fetch, grep)` et `spans gt=… v3=… miss=… extra=…`.

### Pivot sur la position rare : toujours pas fait — voir `05-pivot-position-rare.md`.

---

## 6. Ce qu'il ne faut pas refaire

**Cinq hypothèses fausses dans cette session**, dont deux justes mais incomplètes :

| Hypothèse | Verdict |
|---|---|
| Double-feed d'overlap dans le DFA regex | testée **deux fois**, négative les deux fois |
| Un seul span testé par doc (regex) | réfutée par les compteurs |
| Classes de caractères comme cause racine | c'était un symptôme |
| Câblage des siblings dans le regex | mesure strictement neutre |
| Chaînes multiples par doc (fuzzy), seules | sans effet — n'a marché qu'avec le slack |

Ce qui a débloqué à chaque fois, c'est **l'instrumentation**, jamais le raisonnement :
`first_token_dfa=240 sur 347` a localisé le bug regex, et
`window="falseAllowShortFunctionsOnASingle"` a rendu le problème de casse évident en une
ligne. Deux paires de correctifs n'avaient d'effet que **combinées** (leftmost +
troncature côté regex, slack + chaînes multiples côté fuzzy) : tester isolément conduisait
à conclure « inutile » ou « nuisible ».

**Corollaire de méthode** : ne jamais conclure d'une analyse statique qu'un code est
« inoffensif ». L'analyse du matin avait vu le code des siblings en strict et conclu
exactement ça. Le corpus de 50 000 documents l'a réfutée.

---

## 7. Reste ouvert

- 1 FP sur `spin_lock relax` à 50k
- 2 patterns regex totalement cassés **en v2 comme en v3** : `#include\s*[<"]`,
  `[A-Z][a-z]+Error` — extraction de littéraux, pas validation
- `v3_term_is_whole_token_not_prefix` marqué `#[ignore]`, régression située à `8aeb093`
- 3 tests unitaires rouges (2 fixtures de mai, 1 casse connue)
- Build emscripten jamais lancé — la contrainte WASM est respectée par construction
  (tout passe par `build_scatter_dag`, aucun `thread::spawn`) mais non prouvée
