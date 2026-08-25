# Journal de nuit — 25 août 2026

Suite directe du 24 (`docs/24-08-2026/06-recap-progression-et-a-faire.md`,
doc 41 rag3weaver). Branche `wip/publication-3.0.0`. Priorités données avant
de dormir : (1) finir le débogage WASM/parité, (2) la fusion v3 en arènes.
Entrées horodatées (heures approximatives, recalées sur le journal du
playground), les plus récentes en bas.

## 00:05 — état au départ

- Navigateur : 15 440 fichiers indexés en ~15 min (build debug), 413
  segments, 24 fusions une à une, tas au plus haut 2,3 Go, 5,5 Go de
  sidecars écrits dans OPFS. Commit `1fb67ec` (sans Asyncify, permis de
  fusion, lectures paresseuses).
- Panel de parité (21 requêtes) : échecs `read sfx: No such file (os error
  44)` — un searcher tient des segments fusionnés puis supprimés ; en natif
  c'est masqué par mmap. Corrigé dans l'arbre (pas encore commité) : les
  handles paresseux **épinglent** les octets d'un fichier supprimé tant
  qu'ils vivent (sémantique unlink), test natif
  `lucivy_core/tests/test_lazy_directory.rs` (3 verts).
- Rejouer le panel sans réindexer : `?open=user_index` (ouverture directe
  de l'index OPFS). Bloqué : au rechargement, `OPFS mount failed (ret=-20)`
  (ENOTDIR de `wasmfs_create_directory("/opfs")`), reproductible deux fois,
  alors que le montage réussissait au chargement précédent. En cours.

## ≈00:20 — parité navigateur / natif : acquise

- Le montage OPFS a réussi au 3e chargement (`?nodemo`) ; le panel a tourné
  **depuis le worker** (`playground/parity_worker.js` : `lucivy_open` sur
  l'index OPFS + `lucivy_search`), sans dépendre de l'état de la page.
- **19/21 requêtes identiques** au natif (comptes, top-10 ordonné, scores à
  1e-4, nombre de spans) ; les 2 autres : mêmes docs, mêmes scores, mêmes
  spans, ordre des ex æquo différent (disposition des segments différente).
  Tous les modes couverts. Résultats : `/tmp/parity_native.json`,
  `/tmp/parity_wasm.json`, diff `/tmp/parity_diff.txt`.
- Temps en build debug : 4-14 s par requête (natif 35-670 ms) ; un « sans
  résultat » coûte 4,6 s → coût fixe par requête (ouverture des readers de
  ~100 segments × 4 shards, asserts). À remesurer en release.
- Commit `5190663` (épinglage unlink, `?open=`, runners).
- Le montage OPFS qui échoue (`ret=-20`) juste après un rechargement reste
  à comprendre : deux échecs consécutifs puis un succès, sans changement
  de code. Hypothèse : access handles OPFS de l'ancien worker pas encore
  libérés (course), à confirmer avec un délai/retry au montage.

## ≈00:25 — fusion v3 en arènes : écrite, en test

`merge_segments_v3` : arène de textes `(start, len)`, table d'intern en
adressage ouvert (FxHash de la forme + texte, comparaison contre l'arène,
zéro allocation sur un hit, dimensionnée 2× la somme des termes), postings
chunk et mot dans deux vecteurs plats étiquetés par ordinal, un seul tri
puis découpe par ordinal ; lecteurs `for_each_entry` sans allocation dans
`sfxpost_v2` et `word_sfxpost`. Type de sortie inchangé
(`SfxCollectorDataV3`) — l'étape suivante serait de le passer en arène
aussi. Tests de merge et bench 2 000 docs en cours.

## ≈00:35 — build release : même parité, mêmes temps → l'I/O, pas le CPU

- Build release (5,2 Mo) sur l'index OPFS : 20/21 identiques + 1 ex æquo
  (filtre `path`, 382 docs au même score, fenêtre top-10 arbitraire).
- **Temps identiques au debug** : 4-14 s par requête, 4,3 s pour une
  requête sans résultat. Donc pas les asserts : une requête `content`
  matérialise tous les sidecars de tous les segments (~100 segments
  vivants, ~4 Go) et le cache de fichiers (768 Mo) se vide à chaque
  requête → ~4 Go relus depuis OPFS par requête. Le natif vit sur mmap +
  page cache. Test en cours : `?cache=3000` (nouveau `--file-cache-mb`).
- Conséquence structurelle : pour 15-20 k docs dans le navigateur, il faut
  des lectures **par plage** dans les sidecars côté requête (FST résident,
  postings/termtexts lus à l'ordinal), et/ou des sidecars plus petits.
- Fusion en arènes (natif i686, 14 segments, 650 k tokens) : boucle
  436 → 195-223 ms ; assemblage 172 → 250-290 ms à cause d'un tri global,
  remplacé par un bucketing stable par ordinal (les buckets arrivent triés
  par construction, vérifié en debug) — bench en cours.

## ≈00:40 — la taille de l'index est le vrai plafond du navigateur

Mesuré dans OPFS après le run complet (15 440 fichiers, ~400 Mo de texte) :
**5 904 Mo** vivants — par shard ~1 450 Mo dont `.sfx` 610-680 Mo,
`word_sfxpost` ~190, `bytemap` ~165, `sfxpost` ~155, `termtexts` ~90,
`sibling_v3` ~80 ; 1 500 à 2 500 fichiers par shard (≈ 100-150 segments,
les fusions n'ont consolidé que 24 groupes de 14). Une requête `content`
touche le `.sfx` de chaque segment : 2,5 Go, hors de portée d'un tas wasm
de 4 Go → le cache de 768 Mo se vide à chaque requête, d'où les 4-14 s.
Un `?cache=3000` ne suffira pas non plus (2,5 Go rien que les FST).

Leviers, par ordre de rendement probable :
1. **Compaction** : fusionner beaucoup plus large en fin d'indexation (le
   FST partage les suffixes entre documents ; 400 petits segments = 400 FST
   redondants). À mesurer en natif : même corpus, fusion forcée jusqu'à
   ~10 k docs/segment, taille des sidecars avant/après.
2. **Format** : 15× le texte est le problème de fond (natif compris : 50 k
   fichiers kernel → ~20 Go). Pistes : `bytemap` et `word_sfxpost` sont
   des dérivés recalculables, `sibling_v3`/`sfxpost` compressibles.
3. **Lectures par plage côté requête** : postings/termtexts lus à
   l'ordinal ; le FST lui-même a besoin d'un `&[u8]` contigu (fork
   BurntSushi), donc résident — d'où l'importance de 1 et 2.

## ≈00:42 — fusion en arènes : mesurée, commitée

Natif i686 release, fusion `content` de 14 segments (~650 k tokens,
~10 M postings), moyenne sur 3 shards :

| version | boucle segments (intern+postings) | assemblage | total |
|---|---|---|---|
| avant (String par clé, Vec par ordinal) | 471 ms (436) | 172 ms | 643 ms |
| arène + table d'intern + postings plats + tri global | 249 (223) | 262 | 511 |
| + remap de docs en table dense | 216 (195) | 249 | 465 |
| + bucketing stable par ordinal (plus de tri) | **180 (145)** | **226** | **406** |

−37 % ; l'assemblage reste dominé par les 650 k `String` de sortie
(`token_texts` + `tokens`) et les 650 k `Vec` de `content_postings` que
le type `SfxCollectorDataV3` impose — prochaine étape si on continue :
passer ce type en arène (touche le collector et les nœuds du DAG).
Tests : lib merge 71, pipeline v3, merge_contains, deux champs — verts.

Ajouté `ShardedHandle::compact(max_docs)` : regroupe les segments de
chaque shard sous `max_docs` et les fusionne (`merge_many`), commit avant
et après. Mesure en cours sur les 15 440 fichiers (natif) : tailles des
sidecars avant/après compaction à 10 k docs/segment.

## ≈00:45 — compaction : deux courses corrigées, mesure en cours

- Index natif complet (15 440 docs) avant compaction : **5 553 Mo, 264
  `.sfx`** (~88 segments par champ) — `sfx` 2 168, `word_sfxpost` 749,
  `sfxpost` 611, `bytemap` 567, `termtexts` 308, `sibling_v3` 291,
  `offsets` 209, `posmap` 167. Même ordre que le navigateur (5 904 Mo).
- `compact()` a d'abord fait paniquer luciole : `start_merges` refusait un
  segment déjà pris par une fusion de la policy et lâchait le `Reply` sans
  répondre (« actor died without replying »). Corrigé : l'acteur répond
  l'erreur ; `IndexWriter::wait_pending_merges()` ; `compact` planifie sur
  un index calme et **re-planifie** si une fusion en cascade s'est
  glissée entre-temps (20 essais, 100 ms).
- Navigateur : `lucivy_compact_async` (thread + statut SAB), cas worker
  `compact`, `index.compact(maxDocs)`, `?compact=N` en fin d'import.
- Mesure « avant/après compaction à 10 k docs/segment » relancée.

## ≈00:50 — compaction mesurée ; la course est réglée à la source

- `compact` planifié **dans l'acteur** `segment_updater` (`SuCompactMsg`) :
  il seul sait quels segments une fusion tient, donc son plan ne peut pas
  être refusé ; `IndexWriter::compact(max_docs)` envoie et attend
  `MergesDone` ; `ShardedHandle::compact` = par shard + commit. Au passage :
  la préparation d'un lot de fusions est atomique (tout valider avant de
  marquer — une 2e op qui échouait laissait la 1re « en fusion » pour
  toujours) et un refus répond au demandeur au lieu de le faire paniquer.
- **Natif, 15 440 fichiers, compaction à 10 k docs/segment : 4 fusions,
  16,2 s, 294 → 21 `.sfx`, 5 642 → 4 449 Mo (−21 %)** : `sfx` 2 215 →
  1 613 (le FST partage les suffixes), `bytemap` 568 → 425, `termtexts` 306
  → 230 ; `word_sfxpost` (750 → 731) et `sfxpost` (614 → 559) ne bougent
  pas — ce sont des postings, rien à partager. Mêmes comptes de hits,
  requêtes 0,5-660 ms.
- Conclusion pour le navigateur : même compacté, l'index fait **11× le
  texte** (4,4 Go pour 400 Mo) ; un tas wasm de 4 Go ne le tiendra pas.
  Les 2,5 Go de fichiers indexés par ordinal/doc (`word_sfxpost`,
  `sfxpost`, `bytemap`, `sibling_v3`, `termtexts`, `posmap`,
  `word_pos_map`) sont lisibles par plage ; le `.sfx` (1,6 Go) doit rester
  résident tel quel (`&[u8]` contigu pour le FST). C'est un chantier de
  format + lecteurs, pas un réglage. Pour 15-20 k docs en navigateur, la
  cible réaliste immédiate est un corpus plus petit ou un index côté
  serveur + deltas ; à décider au réveil.

## ≈00:52 — suites complètes vertes, commit

lib **1416** (le test des permis de fusion en plus), lucivy-core 23
binaires verts hors `bench_sharding` t01/t04 (pré-existants), WASM release
rebâti avec la compaction. Commit + push ci-dessous. Lancé ensuite : le
corpus complet dans le navigateur avec `?compact=10000` pour mesurer les
requêtes sur 21 `.sfx` au lieu de 300.

## ≈00:55 — navigateur : indexation 809 s en release, compaction sans effet

- Run complet `?corpus&compact=10000` en release : **809 s** (13,5 min),
  parité 20/21 + ex æquo, temps de requête inchangés (5-17 s).
- La compaction a « tourné » 24 s sans rien fusionner : 69-90 segments par
  shard après. Cause : en WASM (une fusion à la fois), les lots de la policy
  s'empilent en attente du permis (`start_merges` en attente 80-296 s dans
  le graphe) et **tous leurs segments sont marqués en fusion** → le plan de
  compaction ne voit aucun candidat. Correction : `IndexWriter::compact`
  attend un index calme, `ShardedHandle::compact` refait des tours tant que
  le nombre de segments baisse.
- À reconsidérer au réveil : le permis unique + cascades = files
  d'attente de tâches cooperatives ; une policy plus large en fin de lot
  (ou pas de cascade pendant un chargement) serait plus sain que de
  compacter après coup.

## ≈01:05 — compaction itérative : natif 285 → 15 `.sfx`

`IndexWriter::compact` attend un index calme avant de planifier ;
`ShardedHandle::compact` refait des tours par shard tant que le nombre de
segments baisse (deux tours sans progrès consécutifs = fini). Natif,
15 440 docs, 10 k docs/segment : **5 fusions, 43 s, 285 → 15 `.sfx`,
5 580 → 4 339 Mo** (`sfx` 2 150 → 1 544). Rejeu dans le navigateur après
rebuild.

## ≈01:30 — navigateur : compaction lente et partielle, requêtes inchangées

Run complet `?corpus&compact=10000` avec la boucle patiente : indexation +
compaction **1 274 s**, dont **524 s de compaction** pour passer de ~75 à
35-67 segments par shard seulement (natif : 15 `.sfx` en 43 s). Parité
toujours 20/21 + ex æquo. **Temps de requête inchangés** (4-13 s) : le
nombre de segments n'est pas la variable, ce sont les octets relus par
requête. Deux leçons :
- en wasm, une fusion à la fois + cascades de la policy = files d'attente
  de tâches coopératives et compaction qui tourne en rond ; il faudrait
  suspendre la policy pendant un chargement en masse et fusionner large une
  fois à la fin (ou remonter le permis à 2 avec le cache lazy en place) ;
- la compaction n'est pas le levier des requêtes navigateur ; seuls le
  format (11× le texte) et des lecteurs par plage le sont.

## Bilan de la nuit (pour le réveil)

Fait : parité navigateur/natif sur 15 440 fichiers (20/21 + 1 ex æquo,
tous modes) ; Asyncify retiré (cause du blocage OPFS) ; commit et
compaction sur pthread + SAB ; lectures paresseuses + cache borné +
sémantique unlink dans `StdFsDirectory` ; permis de fusion ; fusion v3 en
arènes (643 → 406 ms, sortie identique) ; `ShardedHandle::compact` planifié
par l'acteur (natif 285 → 15 `.sfx`) ; préparation de lot atomique et refus
qui répond ; harnais de parité et d'analyse de tailles ; docs 41-42
rag3weaver ; journal ; suites vertes (lib 1416, lucivy-core 23 binaires).
Commits `a3693ff` → `8b58881`, tout poussé sur `wip/publication-3.0.0`.

À décider : (1) pour 15-20 k docs dans le navigateur, le vrai chantier est
format + lecteurs par plage (2,5 Go de fichiers indexés par ordinal
lisibles par plage, `.sfx` résident) — ou un corpus plus petit / un index
serveur + deltas ; (2) policy de fusion pendant un chargement en masse ;
(3) `SfxCollectorDataV3` en arène (suite naturelle du merge) ; (4) le
montage OPFS qui échoue parfois juste après un rechargement (retry ajouté,
à observer) ; (5) publication 3.0.0 (dry-run vert, inchangé).

## ≈01:50 — non-régression natif (réponses aux questions du matin)

- Ingestion native 15 440 docs (x86_64 release, fusions incluses) : 38,1 s
  hier soir → 24,5-29,3 s cette nuit (5 runs), −33 % ; un seul point
  « avant », à prendre comme un ordre de grandeur (A/B propre possible sur
  `ab441ad^`).
- Requêtes natives 15 440 docs : mêmes 21 comptes, temps de même ordre dans
  les deux sens selon la forme des segments (`kmalloc` 55 → 77 ms sur des
  segments plus gros, regex 299 → 224, startsWith 118 → 91).
- **Panel 50 k à chaud : identique au 23 août**, spans exacts — plancher
  26 ms, `kmalloc` 27, `spin_lock` 28, `include` 46, `__init` 29,
  relaxed `kmalloc` 26 / `uint64_t` 47 / `__init` 47.
- luciole : cette nuit seulement `set_task_label` ; les consolidations sont
  dans ses utilisateurs (segment_updater : refus qui répond, lot atomique,
  compaction dans l'acteur ; permis de fusion coopératifs). Ouvert dans
  luciole : détection d'un thread mort (une tâche qui trap ne répond
  jamais), famine par files coopératives, graphe d'attente aveugle aux
  mutex/canaux/syscalls.

## ≈02:05 — audit de l'index : `.pos` et `.offsets` retirés des index v3

Audit fichier par fichier (tableau dans la conversation, à reporter dans
le doc d'architecture) : sur 4,34 Go compactés, `.sfx` 36 %, postings
mots 17 % (non compressés, 20 o/entrée), postings chunks 13 %, `bytemap`
9 % et `word_pos_map` 4 % (dérivables), `sibling_v3` 6 % (non compressé),
`termtexts` 5 %, `posmap` 4 %, **`.offsets` 3 % lu par personne**, **`.pos`
2 % lu par les scorers v2 seulement**, docstore 2 %, index inversé 0,3 %.

Fait : champs texte en `IndexRecordOption::WithFreqs` quand
`sfx_version ≥ 3` (BM25 garde les fréquences ; positions/offsets tantivy ne
servent qu'aux scorers v2). Mesuré : suites vertes, parité identique,
**4 339 → 4 162 Mo** après compaction (−177 Mo), ingestion 26,6 s.
Un index v2 (`sfx_version: 2`) garde positions + offsets.

## ≈02:40 — recherche par lots de shards (idée de Lucie)

Plutôt que le LRU fichier par fichier : les shards passent par lots
dimensionnés par un budget mémoire (`LUCIVY_SHARD_BATCH_BYTES`, défaut
1 Go en wasm, illimité en natif = un seul lot = chemin strictement
identique). Un lot est lu une fois, cherché en parallèle tout en RAM,
fusionné dans le top-k courant (même ordre déterministe de `ScoredEntry`,
stats BM25 globales inchangées car calculées sur tous les shards), puis
ses fichiers sont libérés du cache (`evict_cached_files_named`). Nouveau
`ShardFilter::Subset`. Taille d'un shard = somme des longueurs de ses
fichiers (un `stat` par fichier avec les handles paresseux).
Ne réduit pas les octets lus par requête (ça, c'est le saut de shard par
bloom et la pagination) mais supprime le thrash et borne la mémoire.
Validation : parité native avec un budget forcé (4 lots), puis navigateur.

## ≈03:10 — recherche par lots : deux passes comme le distribué, bug en cours

Lucie a redressé la structure : comme dans le distribué, **passe 1** =
prescan de chaque lot (lecture des sidecars, puis libération), **fusion
globale** des prescans (fréquences pour l'IDF), **un seul poids**, **passe
2** = recherche lot par lot avec ce poids. Fait : `prescan_segments_more`
(accumulatif, forwardé par `Box` et `BooleanQuery`), `BuildWeightNode
::prescanned`, `build_search_dag_with_query`, deux boucles dans
`search_internal`. Premier reset trouvé (`global_doc_freq = 0` en tête du
corps accumulatif). Symptôme restant : comptes identiques mais chaque hit
avec 1 span et tf = 1 (top doc 9869 à 0,75 au lieu de 330 à 5,49). Un
agent frais est sur la cause (repro 3 000 docs, 40 s). Natif : un seul
lot par défaut, chemin strictement inchangé (suites vertes).

Pistes de Lucie notées pour la suite : shards fins (~500 docs) + file
d'admission threads × mémoire pour borner l'indexation ; bloom de
trigrammes par shard pour sauter des lots entiers ; matérialiser/libérer
par lot plutôt que par fichier (c'est ce que fait la passe 1).

## ≈03:35 — le « reset » n'en était pas un

Un agent frais a instrumenté le repro (3 000 docs, 4 lots) : après la
passe 1 chaque lot voit 52 segments en cache et `global_doc_freq` 494,
aucun cache miss du scorer, `take_prescan_cache` sans appelant, prescan du
DAG qui saute les v3. Hits et scores justes ; c'était le **tri final** de
la fusion des lots : `ScoredEntry::cmp` est déjà inversé sur le score
(min-tas), `sort_by(|a, b| b.cmp(a))` rendait donc la liste par score
croissant — le « top 10 » du JSON était les 10 pires hits (tf = 1, 1 span).
Corrigé par `into_sorted_vec()`, comme `MergeResultsNode`. Preuve 3 000
docs : top-10, scores et spans identiques. Validation 15 440 docs + suites
+ build WASM en cours.

## ≈09:55 — profil navigateur : 99 % chargement, 1 % recherche

Panel par lots (4 lots) sur l'index OPFS, `V3_PROFILE` : par lot de 45
segments, marche FST `contains_v3` **26 ms**, chargement des sidecars
1 577 ms, ouverture du résolveur (`.sfxpost`) 452 ms, lecture du `.sfx`
avant la marche ~1 200 ms (somme CPU sur 4 threads, ~0,8-1 s de mur). La
passe 2 coûtait encore 0,8-1,2 s par lot : la **détection de version**
faisait `read_bytes()` du `.sfx` entier de chaque segment pour lire 4
octets (DAG, poids, mon filtre) → `detect_sfx_version_of` lit 4 octets.
Résultats par lots : mêmes 20 OK + ex æquo. Le montage OPFS a mis 9
essais (~4 s) à réussir après le rechargement : le réessai à la demande
(`ensure_opfs_mounted` dans open/create) est en place.
Conclusion inchangée : le volume lu par requête est le levier (saut de
shard, pagination), pas le CPU.

## ≈10:10 — trace des chargements : chaque `.sfx` était lu 5 fois par requête

Trace `[fs] load` (nom, octets, ms) sur chaque matérialisation de fichier
entier du répertoire paresseux (`LUCIVY_VERBOSE`). Une requête `kmalloc`,
117 segments : **572 chargements de `.sfx`, 11,6 Go, 28 s CPU** ; les
autres sidecars 117 fois chacun (normal). La recherche dans le shard
elle-même : 5-18 ms.

Coupable : `PrescanShardNode` (prescan v2 du DAG) faisait `read_bytes()`
du `.sfx` **entier** de chaque segment pour lire sa version, puis sautait
le segment (v3). Un DAG par lot → tous les `.sfx` de tous les shards
rechargés à chaque lot (1 + 4). Même motif dans `LucivyHandle::search`.
Corrigé : en-tête de 4 octets (`detect_sfx_version_of`), et les nœuds de
prescan d'un DAG « déjà prescanné » ne couvrent que les shards du lot.

Après : 117 chargements de `.sfx` (2,3 Go), passe 2 à 50-350 ms par lot.
Panel 21 requêtes : 9-16 s → **5,3-12,9 s**, mêmes résultats (20 OK +
ex æquo ; le seul DIFF, `path contains`, est un ordre d'ex æquo).

Débit OPFS mesuré sur les 23 105 chargements du panel (fixe par
ouverture + débit asymptotique) :

| taille | chargements | moyenne | débit |
|---|---|---|---|
| < 1 Mo | 7 518 | 3,8 ms | 143 Mo/s |
| 1-4 Mo | 9 142 | 5,5 ms | 321 Mo/s |
| 4-16 Mo | 4 447 | 10,2 ms | 806 Mo/s |
| 16-64 Mo | 1 698 | 26,3 ms | 1 109 Mo/s |
| > 64 Mo | 300 | 78,6 ms | 1 155 Mo/s |

Soit ~3 ms de fixe par ouverture et ~1,15 Go/s. 72 % des chargements font
moins de 4 Mo : à débit constant de 1,15 Go/s le panel lirait en 116 s de
CPU au lieu de 192. **Un paquet par shard** (un fichier, un manifeste,
tranches sans copie) vaut donc ~40 % du temps de chargement — l'idée de
Lucie du matin ; à faire après le cache d'en-tête.

Reste inexpliqué sur la requête unitaire : 7,1 s côté page pour 3,7 s de
passes 1+2. Piste : les lectures « petites » (en-tête de version, 4
octets) font open+seek+read à chaque appel — ~3 ms sur OPFS × 117
segments × (filtre + poids par lot + prescan) ≈ 2-3 s. Cache des 4 premiers
Ko sur le handle (`head`), une ouverture par handle. Mesure en cours.

## ≈11:30 — combien d'octets une requête touche vraiment (mmap + mincore)

`test_touched_bytes` (ignoré par défaut) : ouvrir l'index, chauffer avec un
terme **différent**, `posix_fadvise(DONTNEED)` sur les 208 fichiers, mesurer
les pages résidentes (`mincore`) avant/après la requête. L'éviction laisse
17 Mo, donc le delta est bien le coût marginal d'une requête. Index kernel
15 440 docs compacté, 4 378 Mo :

| requête | touché | sfx | word_sfxpost | sibling_v3 | posmap | word_pos_map | termtexts |
|---|---|---|---|---|---|---|---|
| `kmalloc` (1 216 hits) | **1 330 Mo (30 %)** | 341 (21 %) | 598 (78 %) | 177 (66 %) | 73 (43 %) | 60 (35 %) | 4 (1,7 %) |
| `spin_lock_init` (1 112) | **1 298 Mo (30 %)** | 354 (22 %) | 628 (82 %) | 180 (67 %) | 75 (44 %) | 59 (35 %) | 2 (0,8 %) |
| `zzqqxxwwvv` (0 hit) | **67 Mo (1,5 %)** | 46 (2,8 %) | 0 | 20 (7,6 %) | 0 | 0 | 0,3 |

Trois conclusions chiffrées :

1. **La pagination vaut 3,3× sur un terme fréquent et ~70× sur un terme
   rare.** Le navigateur paie aujourd'hui les 5,4 Go dans tous les cas ;
   `zzqqxxwwvv` n'a besoin que de 67 Mo. C'est le levier principal.
2. **`word_sfxpost` est le vrai poids du jeu de travail** : 765 Mo (17 % de
   l'index) dont 78-82 % touchés par une requête courante. C'est le fichier
   non compressé à 20 o/entrée repéré à l'audit ; le compresser réduit le
   coût natif *et* navigateur, indépendamment de la pagination.
3. `termtexts` (235 Mo) n'est touché qu'à 1-2 % : il se pagine idéalement.

À froid, la même requête met 381 ms en natif (contre 21-55 ms à chaud) :
l'I/O domine aussi en natif, la différence est que mmap ne lit que 30 %.

## ≈12:00 — « compresser word_sfxpost, mais il faut le décompresser » (Lucie)

Objection juste, et elle sépare deux choses :

- **Compression par blocs** (zstd/lz4) : douteux. En WASM il faudrait
  décompresser à 1-2 Go/s pour économiser des lectures à 1,15 Go/s (débit
  OPFS mesuré) — à peu près nul ; et en natif, où mmap ne lit que les pages
  utiles, c'est du CPU ajouté pour rien : régression.
- **Encodage plus dense** (delta + varint) : pas de passe de
  décompression. Les entrées sont déjà décodées champ par champ pendant la
  marche ; lire un varint au lieu d'un `u32` coûte quelques ns.

Mesure sur de vraies données (`test_wsp_density`, 4 fichiers, 8,87 M
entrées) : les cinq `u32` fixes (20 o) tombent à **7,22 o/entrée, soit
2,77× plus petit** en delta-varint (`doc_id` delta, `first_position` delta
dans le doc, `last-first`, `byte_from`, `byte_to-byte_from`).

| entrées/ordinal | ordinaux | part des entrées | aujourd'hui | varint |
|---|---|---|---|---|
| 1 | 51,0 % | 8,2 % | 20 o | 7 o |
| 2-4 | 29,6 % | 12,4 % | 52 o | 19 o |
| 5-16 | 13,8 % | 18,5 % | 166 o | 60 o |
| 17-64 | 4,4 % | 21,6 % | 603 o | 218 o |
| 65-256 | 1,0 % | 18,3 % | 2 293 o | 828 o |
| > 256 | 0,2 % | 21,0 % | 14 253 o | 5 143 o |

Aussi : 56,4 % des ordinaux sont vides et la table d'offsets pèse 6,9 % du
fichier (13,1 Mo pour 3,3 M ordinaux) — deuxième cible, séparée.

Donc le gain est réel mais il faut le dire honnêtement : 2,77× de lecture
en moins sur 17 % de l'index (et le sidecar le plus touché : 78-82 %),
contre ~5 varints à décoder par entrée. Gain net certain en navigateur (on
lit moins, on ne décompresse rien), gain à froid en natif, coût CPU
marginal à chaud — à vérifier au panel 50 k avant de garder.

Mise en œuvre additive : le writer émet `WSP3`, le lecteur accepte `WSP2`
et `WSP3`. Les index existants continuent de se lire.

## ≈13:00 — WSP3 : `.word_sfxpost` en delta-varint

Écrit. Le writer émet `WSP3`, le lecteur accepte `WSP2` **et** `WSP3` : les
index existants continuent de se lire, rien à migrer.

Format d'un bloc : `n` en varint, puis `c = (n-1)/32` points de reprise de
16 octets, puis les entrées. Une entrée = `d_doc`, `d_first`, `last-first`,
`d_from`, `to-from`, en varints ; `d_first` et `d_from` sont des deltas
quand le document est le même que l'entrée précédente (signalé par
`d_doc == 0`), absolus sinon. Les deltas se calculent en `wrapping_sub` et
s'appliquent en `wrapping_add` : un champ non monotone fait un aller-retour
exact, il coûte seulement un varint plus large.

Le point délicat était `entry_at`, une **recherche binaire** sur des
enregistrements de taille fixe que le varint casse. Le point de reprise `k`
garde l'état du décodeur après l'entrée `k*32-1` — `(doc, first, from)` — et
le décalage de l'entrée `k*32`. Une recherche binaire sur les points de
reprise réduit à une plage de 32 entrées : la recherche reste
logarithmique, pour 1,5 Mo sur un fichier de 177 Mo.

Résultat sur l'index kernel 15 440 docs compacté :

| | WSP2 | WSP3 |
|---|---|---|
| `.word_sfxpost` | 738 Mo | **292 Mo** (2,53×) |
| index total | 4 339 Mo | **3 708 Mo** (−14,5 %) |
| panel 21 requêtes | 2 738 ms | **2 494 ms** (−8,9 %) |

Les 21 requêtes rendent les **mêmes comptes** ; le seul DIFF est celui qui
existait déjà (ordre d'ex æquo sur `path contains`). Le panel est plus
rapide, pas plus lent : le décodage varint coûte moins que les octets qu'on
ne lit plus, même à chaud. Une seule requête est plus lente (`kmalloc`,
54,9 → 105,8 ms) et c'est la première du panel — chauffe, pas régression :
la même requête relancée à froid est mesurée séparément.

Tests : 1 419 (moteur) + suites `lucivy-core`, dont un test qui compare
WSP3 et WSP2 entrée par entrée sur 9 tailles (1, 2, 31, 32, 33, 64, 65,
200, 1 000), vérifie `entry_at` sur toutes les clés présentes *et*
absentes, et qu'un fichier tronqué ne boucle ni ne panique.

## ≈13:10 — les autres sidecars : où le varint mord encore

Relevé des formats croisé avec « octets touchés par requête ».

| fichier | taille | touché | verdict |
|---|---|---|---|
| `.sibling_v3` | 254 Mo | 66 % | ✅ meilleure cible suivante |
| `.sfxpost` | 555 Mo | — | ✅ bon, points de reprise obligatoires |
| `.termtexts` | 221 Mo | 1,7 % | ✅ gain disque seul |
| `.posmap` / `.word_pos_map` | 322 Mo | 43 / 35 % | ❌ rien à mordre |
| `.bytemap` | 408 Mo | — | ❌ déjà un bitmap |

- `.sibling_v3` est **plus simple** que `word_sfxpost` : entrées de 6 o
  fixes, `next_ordinal` croissant et dédupliqué dans chaque ordinal, et
  lecture **strictement séquentielle** (`siblings()` boucle `pos += 6`, pas
  de recherche binaire) — donc **pas de points de reprise à payer**. En
  prime `gap_len` occupe 2 octets pour une valeur qui vaut 0 ou 1 presque
  toujours : `(delta_next << 1) | (gap != 0)` puis `gap` seulement s'il est
  non nul.
- `.sfxpost` a la structure de `word_sfxpost` d'avant : `doc_ids`
  strictement croissants, `payload_offsets` cumulatifs (redondants : c'est
  le préfixe des longueurs), payload déjà varint mais **absolu**. Mêmes
  points de reprise que WSP3, `find_doc` étant une recherche binaire.
- `.posmap` / `.word_pos_map` : tableaux **denses positionnels** à valeur
  arbitraire, accès `data[p*4]` en O(1) — le varint le détruirait sans rien
  gagner. Deux autres défauts nets : table d'offsets en **u64** pour des
  fichiers de moins de 200 Mo, et l'audit les marque **dérivables**. La
  bonne optimisation là-bas est la suppression, pas l'encodage.
- `.bytemap` : bitmap 256 bits par ordinal. Le levier est la **sparsité**
  (5-15 octets distincts sur 256) et la déduplication.

## ≈13:40 — ce que WSP3 change au jeu de travail (et pas qu'au disque)

Même mesure `test_touched_bytes` sur l'index reconstruit en WSP3, chauffe
sur `netdev`, éviction, puis la requête :

| | WSP2 (4 378 Mo) | WSP3 (3 818 Mo) |
|---|---|---|
| `kmalloc` touché | 1 330 Mo (30,4 %) | **928 Mo (24,3 %)** |
| dont `word_sfxpost` | 598 Mo (78 %) | **219 Mo (72 %)** |
| `kmalloc` à froid | 381 ms | **160 ms** |
| `zzqqxxwwvv` touché | 67 Mo | 63 Mo |

Le jeu de travail baisse de 30 % et la requête à froid est **2,4× plus
rapide** — c'est bien les octets lus qui commandent, pas le CPU.

Nouveau classement du jeu de travail de `kmalloc` : `sfx` 314 Mo,
`word_sfxpost` 219, `sibling_v3` 176, `posmap` 82, `sfxpost` 66,
`word_pos_map` 65, `termtexts` 4. `sibling_v3` est maintenant le deuxième
sidecar le plus lu — et c'est le plus facile à encoder (lecture
séquentielle, pas de recherche binaire à préserver).
