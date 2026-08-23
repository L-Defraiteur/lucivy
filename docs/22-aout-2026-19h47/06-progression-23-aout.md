# Progression du 23 août 2026 — ce qui a été fait, mesuré, commité

Chaque ligne est une mesure, pas une estimation. Les commits sont sur `v3-recovery`.

## Le résultat en une table

50 000 fichiers du kernel Linux, 800 segments naturels, index mmap, moteur seul
(hors désérialisation des documents), spans vérifiés un à un contre le disque.

| Query | hier soir | ce matin | 14h | **16h** | grep (même tâche, disque) |
|---|---|---|---|---|---|
| requête sans résultat | — | — | 190 ms | **29 ms** | 172 ms |
| `kmalloc` strict | 143 ms* | 104 ms* | 177 ms | **29 ms** | 199 ms |
| `spin_lock` strict | 3 526 ms** | 297 ms | 183 ms | **28 ms** | 318 ms |
| `net_device` strict | 14 001 ms** | 220 ms | 207 ms | **34 ms** | 292 ms |
| `include` strict (36 824 docs) | 34 891 ms** | 560 ms | 205 ms | **58 ms** | 342 ms |
| `__init` strict | 49 000 ms | 6 296 ms | 328 ms | **42 ms** | 323 ms |
| `kmalloc` relax | — | — | 184 ms | 175 ms | 1 081 ms |
| `uint64_t` relax | 1 353 ms | — | 216 ms | 170 ms | 1 126 ms |

(*) à 20k documents. (**) sur index fusionné à 32 segments.

Le « grep » de droite lit chaque fichier depuis le disque et trouve **toutes** les
occurrences en spans d'octets — le travail exact du moteur. Hier, la colonne grep
était un `contains` sur un `Vec` préchargé en RAM (90 ms), ce qui donnait « on est plus
lent que grep ». C'était une comparaison truquée dans les deux sens.

## Chronologie

### Matin — mesurer ce que le harnais mesurait vraiment

- `476a724` — Les cinq sidecars étaient copiés (`.to_vec()`) par segment et par
  requête. Zéro-copie via `OwnedBytes`. Strict −21/−27 %, relax inchangé.
- `1f4d19e` — **Le chronomètre du harnais englobait le grep de référence.** Le
  « pipeline word ×15 plus lent » n'a jamais existé : c'était `grep_docs_relaxed` qui
  appliquait `strip_seps` à tout le corpus. Six hypothèses de la §5 d'hier répondaient
  à une question inventée. Séparé en `(search, +fetch, grep)`.
  Ajout de `briques/profile.rs` (compteurs opt-in sous `V3_PROFILE`).
  Puis mémoïsation des remainders dans `build_chains_from_splits` : redondance 15×
  à 78× mesurée ; `include` 2 675 → 213 ms CPU.
- `4eaf367` — À nombre de segments **égal**, un index fusionné était 5 à 29× plus lent
  qu'un index par commits. La conclusion d'hier « moins de segments = plus lent, 46× »
  était fausse : c'était un quadratique en taille de segment dans
  `resolve_chains_impl` (appariement `active × entries`). Trois correctifs : index par
  document, `resolve_filtered` sur les docs actifs seulement (264 M → 289 k entrées),
  mémo de la position 0 (39 M → 365 k postings). `spin_lock` fusionné 3 546 → 324 ms.
- `720951b` — Docs corrigées (la §3 d'hier).
- `83d9695` — **Cache d'index** `V3_INDEX_DIR` (53,9 s → 0 s), timings de merge,
  `LUCIVY_VERBOSE` qui était documenté et jamais lu.

### Midi — trois audits en parallèle (agents), puis le merge

- Audit contrats : cinq de mes soupçons faux, et P5 (collision de clé 0x02) annoncée
  « impact moyen-faible » — elle se révélera réelle l'après-midi.
- Audit quadratiques du merge : a **mesuré** (pas estimé) — `merge_segments_v3` n'est
  pas le coût ; le tri FST estimé à 300-700 ms en fait 30-50 (mesuré après) ; le vrai
  quadratique est dans `word_map.rs` (`contains` linéaire, Θ(D^1.9)).
- Audit requêtes : trois sidecars morts (`word_pos_map`, `chunk_word_map`,
  `next_word_map`), `.sfxpost` encore copié, `entries_filtered` qui promettait une
  dichotomie et balayait, et **l'inversion posmap** comme vrai gisement.
- `1b92465` — Gaspillage supprimé (copies de `.sfx` jetées, port DAG mort, quadratique
  des word maps, `contains` du collector). Indexation −18 %. Merge total **inchangé**
  (18,9 s) — dit franchement dans le commit.
- `5425490` — **`__init` 49 s → 976 ms** : résolution stricte par `posmap` (la question
  dans le bon sens : « quel ordinal à pos+1 ? »), 0 collision / 0 mismatch sur
  27,8 M de lookups ; et `Arc` sur les listes d'alternatives (3,4 M de chaînes clonaient
  la même liste : chunk walk 50 s → 0,87 s).
- `40af7c7` — Vérité terrain relaxed : **ses trois « faux positifs » étaient des bugs
  du harnais** (fenêtre glissante coupée trop tôt). v3 avait raison.
- `803a174` — **`word_pos_map` réaffecté** : même forme que posmap, contenu inutile
  (compteur que rien ne lisait). Stocke désormais l'ordinal word-stripped + span,
  construit depuis les mêmes `word_postings` que `word_sfxpost`. `word_pairs`
  57 M → 0, `uint64_t` relax word resolve 6,7 s → 3 ms CPU. Format `WMP2`.
- `cdd577d` — Indexation sur disque 464 s → 6,7 s : **btrfs+zstd, un fdatasync = 65 ms**,
  25 fsyncs par segment, 8 finalizes en parallèle sérialisés par le FS. Construire en
  RAM, copier sans fsync, tampon `.v3_shape` écrit en dernier.
- `2eb6426` — **Merge parallèle** : le DAG avait N nœuds `merge_i` depuis toujours, mais
  `execute_dag` force l'inline dans un acteur. Fusions en tâches luciole, réponse par
  continuation (`collect_replies_to`), `IndexWriter::merge_many`. 10k : 18,9 → 5,6 s.
- `76dcbc1` — Index fusionné au niveau du naturel : `include` 34,9 s → 1,0 s (chaînes
  groupées par tête commune : 459 M → 780 k lookups ; `OrdinalHeader` emprunte au lieu
  d'allouer trois `Vec`, 40 Go de trafic d'allocation supprimés).
- `75577be` — Le chronomètre « v3 » comptait aussi `searcher.doc()` pour chaque
  résultat (36 824 fetches sur `include`). Séparé.

### Après-midi — la vérité terrain fait le même travail que le moteur

- `456bd58` — **Les documents étaient exacts, les highlights faux.** Une seule
  occurrence par document sur les chaînes (pré-existant : `position` d'émission écrasée
  par le dedup), fins tronquées (clamp sur le contenu propre), relaxed arrêté avant le
  token suivant (`overlap_overflow` placé via posmap).
- `4779915` — Plus aucun span en trop : milieu de chaîne à sti > 0 (sautait des octets
  en silence), fuite de partition 0x02 dans le DFS chunk, et la **collision de clé
  0x02** (`"0ui"` = `"0"+"ui"` ou `"0u"+"i"`, même ordinal) — fin de contenu exacte via le
  premier chunk du mot portant un séparateur.
- `4f3e7a9` — **Le plancher.** Une requête sans résultat coûtait 190 ms. 3 803 ms de
  CPU par requête dans `SfxFileReaderV3::open` : copie du FST entier + table des
  parents + désérialisation de word maps mortes, **par segment, par requête, depuis le
  début du projet**. `open_owned` emprunte le slice mmap. 3 803 → 2 ms. Tout le strict
  passe sous 60 ms.

## Ce que la journée enseigne (trois fois chacune)

1. **Vérifier qu'un écart existe avant de l'expliquer.** Le « ×15 du relaxed », le
   « 46× des segments », le « 300-700 ms du tri FST » : trois chiffres non vérifiés,
   trois explications argumentées, trois fois faux.
2. **Le coût est souvent hors des compteurs.** Le fetch des documents dans le timer,
   le grep dans le timer, l'ouverture du FST hors de `find_literal_v3`. Une requête
   **vide** est la mesure du plancher et doit être la première du panel.
3. **Cohérent ≠ correct.** Les tests existants vérifiaient la cohérence interne ; la
   vérité terrain par spans, depuis le disque, a trouvé six bugs en une heure, dont un
   qui perdait deux occurrences sur trois de `std::unique_ptr`.

## Harnais, état final

```bash
V3_INDEX_DIR=/tmp/v3idx_50k V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 \
V3_COMMIT_EVERY=500 V3_PROFILE=1 \
V3_QUERIES='zzqqxxyyww:strict,spin_lock:strict,__init:strict,uint64_t:relax' \
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture
```

Sortie par requête : `(search, +fetch, grep) spans gt=… v3=… miss=… extra=…`, puis les
trois premiers spans manquants/en trop avec contexte et chemin de fichier.
Ajouts du jour : `V3_INDEX_DIR`, `V3_PROFILE`, `V3_MERGE_AT_END`, `V3_DIAG_LITERAL`,
`V3_DIAG_BYTE`, `V3_DIAG_RESOLVE`, `LUCIVY_VERBOSE` (fonctionne maintenant).

## Suite, 23 août après-midi — fusionné = frais = disque

Point de départ : `07-suggestions-et-chantiers.md`, A1 en tête. Le test demandé par A1
(`v3_merge_equals_fresh_by_spans`) a été écrit d'abord ; tout le reste en découle.

| Query | naturel 800 seg. | fusionné 32 seg. | spans (les deux) |
|---|---|---|---|
| requête sans résultat | 29 ms | 2 ms | 0, exact |
| `kmalloc` strict | 30 | 86 | 2 417 exact |
| `spin_lock` strict | 32 | 35 | 11 893 exact (1 manquant avant) |
| `net_device` strict | 33 | 82 | 854 exact |
| `include` strict | 55 | 410 | 214 692 exact (3 / 11 manquants avant) |
| `__init` strict | 28 | 41 | 16 746 exact (1 manquant avant) |
| `kmalloc` relax | 27 | 70 | 2 420 exact |
| `uint64_t` relax | 40 | 211 | 31 194 exact |
| `__init` relax | 63 | 297 | 214 121 exact (161 / 7 manquants, 1 doc perdu avant) |

rag3db (15 requêtes) : 15/15 exacts, `rag3db` 15 128 / 15 128 (144 manquants hier).
zh_CN 600 docs : fusionné = frais = grep sur 11 requêtes.

### Ce qui a été trouvé, dans l'ordre, par des reproductions de 3 s

- `43fb110` — **Quatre causes, un commit.**
  1. Une clé 0x02 couvre plusieurs *formes* de mot (`init` = mot `init`, ou `in` +
     overlap `it`, ou `in` + overlap `i` + …) ; une clé chunk aussi (`spinlock` entier,
     ou `spinlo` + overlap `ck`). Un seul ordinal portait les métas de la première
     occurrence internée, et l'ordre des segments changeait laquelle. C'était « A1 » :
     pas un bug du merge, un bug d'internement que le merge rend visible. Internement
     par (texte, forme) ; la fabrique FST prenait déjà plusieurs parents par clé.
  2. `word_sfxpost` WSP2 : `byte_to` = fin de contenu du posting, lue par tout le
     monde ; le contournement `word_content_end` d'hier disparaît.
  3. **`equal_chunks` émettait un chunk vide** sur les textes multi-octets (le snap UTF-8
     prend de l'avance sur le plan de découpe). C'était « A2 » : pas l'overlap, pas
     l'EOF — un chunk sans texte à `position - 1`, que le chemin ancré rejetait.
  4. Dedup des matchs par (doc, position, **byte_from**) : `INIT2INIT` pour `init`.
- Harnais : spans **assertés** (C2), `V3_SPANS_REPORT_ONLY=1` pour revenir au critère
  documents.

### Outils ajoutés (tous dans `test_sfx_v3_pipeline.rs`)

- `v3_merge_equals_fresh_by_spans` — A∪B frais contre merge(A,B) à deux niveaux, spans
  par document, strict + relaxed + grep. `V3_MERGE_DOCS`, `V3_CORPUS`.
- `v3_merge_bisect` (`#[ignore]`) — delta-debugging : réduit le corpus au minimum qui
  fait diverger fusionné/frais (`V3_BISECT_TARGET`) ou frais/grep (`V3_BISECT_GREP`).
  332 docs → 3 en 6 s, 600 → 1 en 0,7 s.
- `v3_merge_repro_files`, `v3_a2_probe`, `v3_a2_chunks` (`#[ignore]`) — rejouer une
  liste de fichiers, couper un texte caractère par caractère, dumper le tokenizer.

### La leçon du jour

Hier : « estimer avant de mesurer, trois fois faux ». Aujourd'hui : trois fois de
suite, l'explication plausible était fausse (A1 « le merge », A2 « l'overlap UTF-8 »,
« le vocabulaire des autres segments ») et la reproduction minimale a dit autre chose
en moins d'une minute. Le bisect a coûté 40 lignes ; il a remplacé trois heures de
théorie.

## Soir — la policy de merge au commit, et ce qu'elle a révélé

| index 50k | segments | construction | `include` | `__init` relax | `uint64_t` relax | spans |
|---|---|---|---|---|---|---|
| naturel (`NoMergePolicy`) | 800 × 62 | 64 s | 55 ms | 63 | 40 | 9/9 exacts |
| « fusionné 32 » du harnais | **48 078** + 31 × 62 | 64 s + 660 s | 410 | 297 | 211 | 9/9 exacts |
| fusionné 1 | 1 × 50 000 | 64 s + 212 s | 718 | 348 | — | exacts |
| **policy au commit, plafond 10k** | 78 (10000, 8500, 7500, 4000…) | **72 s** | **79** | **85** | **84** | 9/9 exacts |

- **A4** : en fusionnant vers un segment, le compteur de parents d'une clé a atteint
  63 242 puis 64 461 — limite u16 65 535. Garde posé (refus propre), en-tête passé en
  u32, fusion complète refaite : une clé à 3 248 834 parents, 82,7 % des ordinaux
  24 bits consommés par 50k docs. Les gros segments sont mauvais sur tous les axes.
- L'index « 32 segments » d'hier était un segment de 48 078 docs plus des miettes : le
  merge par paliers du harnais avait tout avalé. B2 (« un gros segment = un thread »)
  mesurait ça, pas un défaut de parallélisme.
- **B4** : `handle_commit` consulte la policy, qui cascade en fin de fusion ; segments
  en vol suivis, merge explicite recouvrant refusé (sinon 400 docs → 269, mesuré) ;
  `max_merged_docs` plafonne la **sortie** des fusions ; `LucivyHandle` pose 10k.
- Deux bugs que la policy a fait sortir en une heure, parce que pour la première fois
  des fusions tournaient **pendant** l'indexation :
  1. `persist` pendant une fusion en vol → fichier tronqué → **SIGSEGV** en mmap.
     `drain_merges()` avant de persister ; le drapeau `pending` ne tombe qu'après la
     cascade.
  2. **Le GC supprimait les `.sfx` des segments en cours d'écriture** : `sfx_field_ids`
     n'est posé sur le meta qu'après l'écriture, donc `list_files` ne les nommait pas.
     `include` 36 824 → 14 247 documents, trous alignés sur les threads d'indexation.
     Tout fichier d'un segment encore dans l'inventaire est vivant, quel que soit son
     meta — lu depuis `.managed.json` sans verrou (le premier essai a déverrouillé un
     deadlock lecteur/écrivain sur `meta_informations`).

## Nuit — fuzzy : les mêmes leçons, appliquées

Point de départ : C4 du doc 07. `baseline_fuzzy_regex` comparait des documents sur
500 fichiers, et `test_fuzzy_ground_truth` tournait sur le moteur **v2** (le défaut
de `sfx_version` est 2 ; il jugeait un highlight « OK » à distance ≤ d, pas à sa
place).

**Mesuré avant de corriger** (rag3db, vérité terrain par spans depuis le disque,
définition partagée `fuzzy_spans`) : documents quasi exacts (1 doc d'écart sur
`functin`, `inclde`, `uint64`), **spans 0/8 exacts** — les highlights étaient les
étendues des chaînes de trigrammes (26-40 octets pour 10) parce que
`verify_candidates` vérifiait le document, jamais le span. `inclde` 1 254 ms,
`uint64` 1 503 ms sur 4 600 fichiers.

**Après** (`e96dc11`) :

| rag3db (4 600 fichiers) | avant | après |
|---|---|---|
| spans exacts | 0 / 8 | **11 / 11** |
| documents exacts | 5 / 8 | 11 / 11 |
| `inclde` fz1 | 1 254 ms | 305 ms |
| `uint64` fz1 | 1 503 ms | 314 ms |

| kernel 50k naturel | docs | spans | search |
|---|---|---|---|
| requête sans résultat fz1 | 0 | exact | 17 ms |
| `kmallc` fz1 | 1 494 | 3 053 exact | 1 855 ms |
| `spinlock` fz1 | 4 000 | 21 205 exact | 670 ms |
| `net_devce` fz1 | 656 | 2 448 exact | 456 ms |
| `inclde` fz1 | 37 115 | 216 996 exact | 7 343 ms |
| `__init` fz1 | 44 579 | 1 815 246, **1 manquant** | 11 175 ms |
| `uint64` fz1 | 1 316 | 32 708 exact | 2 913 ms |
| `mutex_unlok` fz1 | 2 276 | 10 622 exact | 189 ms |
| `kmalloc` fz2 | 13 613 | 77 050 exact | 5 807 ms |

(grep fuzzy depuis le disque : 4 à 7 s par requête — la DP sur chaque fichier.)

Ce qui a été trouvé, par reproductions de 10 ms (`v3_fuzzy_span_inside_long_token`) :
- `MAX_CHAINS_PER_DOC = 8` : un plafond silencieux, 280 occurrences de `rag3weaver`
  perdues sur 1 107. Remplacé par des régions (hits proches), sans plafond.
- Un hit de la partition word porte la position du **premier** chunk de son mot :
  « dernier hit par octet » n'était pas « dernière position », la fenêtre s'arrêtait
  avant l'occurrence (`rePrun|ing` pour `retrun`). Positions min/max.
- Marge de fenêtre en positions : une suite de séparateurs est plusieurs chunks.
  Marge en octets de contenu.
- Un span tronqué au bord d'une fenêtre coupée (`uint6|`) : laissé à la fenêtre de
  sa propre région, qui le voit entier.
- **`LucivyHandle::search` ne marchait pas sur un index v3** (« invalid .sfx magic
  bytes ») : prescan v2 inconditionnel. C'est l'API des bindings.

Reste : la perf fuzzy à grande échelle (`__init` fz1 11 s, `inclde` 7 s — à profiler,
probablement `rebuild_window_mapped` et les hits des bigrammes fréquents), et le
défaut `sfx_version = 2` à trancher.

### Fuzzy, suite : l'audit agent, et trois générateurs de candidats

Un agent en lecture seule a comparé les pipelines contains et fuzzy avec les
compteurs — **mesuré**, pas estimé. Enseignements non appliqués au fuzzy : prescan
séquentiel (wall = CPU, contains à 24 de concurrence), FST parcouru deux fois par
n-gramme, 96 % des hits word en écho des hits chunk, `resolve_doc` par position de
fenêtre (49 postings décodés pour 1), pas de compteurs. Corrigés (`4fbf6dd`) :
kernel 50k `inclde` fz1 7 343 → 297 ms, `__init` 11 175 → 476, `kmalloc` fz2 5 807 →
233, spans identiques.

Puis trois générateurs de candidats derrière `V3_FUZZY_MODE`, même vérification,
spans identiques dans les trois (kernel 50k, wall ms) :

| requête | ngram | pivot | pieces | **auto** |
|---|---|---|---|---|
| `kmallc` fz1 | 114 | 120 | 66 | 71 |
| `spinlock` fz1 | 50 | 59 | 78 | 79 |
| `net_devce` fz1 | 43 | 43 | 41 | 42 |
| `inclde` fz1 | 295 | 198 | 129 | 142 |
| `__init` fz1 (→ `init`) | 485 | 480 | 575 | 499 |
| `uint64` fz1 | 143 | 64 | 63 | 67 |
| `mutex_unlok` fz1 | 26 | 26 | 31 | 32 |
| `kmalloc` fz2 | 228 | 261 | 189 | 201 |

- `ngram` : tous les n-grammes résolus, seuil pigeonhole sur la région.
- `pivot` : seuls les `N − t + 1` n-grammes les plus rares (toute occurrence en
  contient un), seuil 1.
- `pieces` : requête coupée en d+1 pièces, chaque pièce résolue exactement par le
  pipeline contains, partition choisie par coût FST minimal. Ce n'est pas « gratuit » :
  le contains sur un littéral de 3 octets coûte 120 ms CPU sur rag3db (chaînes à
  travers les séparateurs en relaxed, légitimes). Les pièces gagnent quand elles
  sont rares, perdent quand l'une est `in`.
- `auto` (défaut) : pièces si leur coût FST × 2 ≤ coût pivot, sinon pivot.

Au passage : un split dont la clé contient toute la requête (contenu + overlap) est
un match simple, la chaîne était redondante (`fst_walk`, −700 chaînes sur `inc`).

Contains sur le même index : 30-80 ms. Le fuzzy est à ×1,5-3 (hors `__init`, qui
est une question de sémantique : `init` d=1 admet `int`, `unit`, `inet`).
