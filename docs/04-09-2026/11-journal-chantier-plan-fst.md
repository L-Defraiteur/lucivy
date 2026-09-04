# Journal du chantier « la phase FST d'une requête en un plan par shard »

Suite de [10](10-chantier-prescan-dictionnaire-rapport.md) (le rapport
écrit avant de coder). Le 5 septembre 2026, journée. Ce document dit ce
qui a été fait, mesuré, et ce qui a divergé du rapport — le rapport lui-même
n'est pas réécrit.

Point de départ ([09](09-journal-chantier-dictionnaire.md) §11) : sur
30 000 fichiers, l'index dictionnaire répond ×2 à ×22 plus lentement que
l'index v3 à froid, parce que la phase FST d'une requête est faite une fois
(mémo du lecteur partagé) mais sur un thread, par le premier segment qui
la demande, pendant que les 159 autres attendent.

---

## 1. Où ça s'insère — ce qui diverge de la doc 10

La doc 10 proposait un nœud `PlanShardNode` dans `search_dag.rs`. Le
plan est mis **ailleurs** : au début de `prescan_segments_more` des trois
requêtes v3 (`contains_query_v3.rs`, `fuzzy_query_v3.rs`,
`regex_query_v3.rs`), juste avant le scatter par segment. Raisons :

- c'est l'**unique** point d'entrée de la phase par segment, pour l'index
  simple (`weight()` → `prescan_segments`), pour `ShardedHandle`
  (`BuildWeightNode` appelle `prescan_segments` une fois pour tous les
  segments v3 de tous les shards), pour la recherche par lots
  (`search_internal` appelle `prescan_segments_more` par lot) et pour la
  fédération ; un nœud dans `search_dag.rs` n'aurait couvert que le
  chemin shardé ;
- les segments de plusieurs shards arrivent dans le même appel : le plan
  regroupe les segments par dictionnaire (identité du `DictionaryField`,
  `plan::dictionaries`) et planifie chaque shard ;
- rien à changer dans le DAG ni dans `BriquesContext`.

Le plan ne produit pas un objet `QueryPlan` transmis aux segments : **il
remplit les cellules de la mémo** (`FstMemo`) que les segments vont lire,
par vagues de tâches du scheduler, avant que le scatter ne démarre. La
mémo reste, comme support du plan ; ce qui disparaît, c'est le calcul en
ligne par le premier segment et les préfetchs lancés depuis les tâches de
segment (`prefetch_fuzzy_scans`, le préfetch des pièces dans
`resolve_pieces` — supprimés). Une cellule que le plan n'a pas prévue est
calculée en ligne comme avant : **le plan est une optimisation, jamais une
condition d'exactitude**. `V3_PLAN=0` le désactive (A/B, mesure de son
coût propre).

Module : `src/suffix_fst/briques/plan.rs`. Cellules : `Candidates(partition)`
(tag 1), `Count(partition)` (tag 6), `WalkChunks` (tag 2), `WalkWords`
(tag 3) — les tags sont maintenant des constantes de `fst_walk`
(`MEMO_TAG_*`), et `FstMemo::peek` lit une cellule faite sans attendre ni
calculer. Un job `LiteralJob` par (dictionnaire, littéral), un `FuzzyJob`
par (dictionnaire, requête floue) qui engendre des `LiteralJob` pour ses
pièces ; `Planner::run` boucle : chaque job donne les cellules de sa
prochaine vague, `fill` soumet celles qui manquent (une tâche par cellule,
priorité Critical), attend toutes (`try_wait` : coopératif sur un thread
du scheduler, bloquant ailleurs), et on recommence tant qu'un job a
quelque chose. **Personne n'attend sous les tâches** : une cellule ne
touche pas la mémo en la calculant, et les segments ne démarrent
qu'après.

Ce qu'un littéral demande (miroir de `composite::find_literal_v3`), **en
une vague** (état final ; la première version dérivait les restes vague
par vague depuis les marches, voir §3 bis) :

- ses candidats par partition (`candidate_partitions(anchor, strict)`),
  sa marche chunk si les chaînes chunk sont marchées (strict, ou mots
  longs possibles — `may_have_long_words` lu sur le `.termtexts` du
  dictionnaire, ou `V3_RELAXED_CHUNK_CHAINS=1`), sa marche mot si relâché ;
- pour **chaque suffixe** de la requête minuscule (les restes qu'une
  chaîne peut atteindre en sont tous — un superset, connu sans marcher) :
  sa marche (chunk, mot), son compte ancré quand il fait plus de deux
  octets (SI0 ; et 0x02 côté mot), et pour une racine ancrée sur le second
  token (strict, `h ≤ len/2`) la **liste** SI0, première position d'une
  chaîne.

Le fuzzy : vague 0, les comptes de tous ses n-grammes et de toutes les
pièces possibles (ce que faisait `prefetch_fuzzy_scans`) ; vague 1, la
décision du générateur — extraite dans `composite::fuzzy_generator`
(`Pieces` / `Pivot(keep)` / `AllNgrams`, lue sur la FST seule) pour que le
plan et les segments prennent **la même** — puis des `LiteralJob` pour les
pièces, ou les listes des n-grammes gardés. `resolve_all_trigrams` classe
maintenant les n-grammes par **compte** (sans décoder) et ne décode que
les listes gardées ; avant, toutes les listes étaient décodées pour être
comptées. La regex : un `LiteralJob` par littéral requis
(`regex_verified::plan`), strict.

`V3_PROFILE=1` imprime `[plan] … N waves, N cells computed, N held, wall`
puis, par vague, le mur, la somme CPU et la cellule la plus lente.

## 2. Première mesure : le plan marche, et son mur est une cellule

Référence 10 000 fichiers (`idx-dict2`), avant/après, `search` en ms :

| requête | mémo seule (5 sept. matin) | plan v1 |
|---|---|---|
| mutex_lock relax | 9,5 | 9,5 |
| sched term | 23,7 | 23,7 |
| schdule fz1 | 48 | 48 |
| regsiter fz2 | 91 | 91 |

Rien ne bouge — mais les lignes `[prescan]` disent où c'est passé : les
segments sont maintenant tous courts (max par segment ≈ moyenne, plus de
segment qui calcule pendant que les autres attendent), et le `[plan]`
prend tout : `sched term` 18,9 ms de plan pour 15 cellules en 4 vagues,
`schdule` 39 ms pour 83 cellules. Par vague, **une seule cellule fait le
mur** : les candidats ancrés d'un reste **d'un octet** sur la partition
mot — `cand/02 "d"` 16 ms, `"e"` 37,6 ms, `"r"` 25 ms, `"i"` 26 ms.
533 385 entrées pour `e` sur 10 000 fichiers : scan 11 ms, **tri 27 ms**
(`sort_by_key` stable sur une clé tuple).

Découper la cellule en 257 sous-plages (une tâche par octet suivant,
`compute_in_tasks`, soumis depuis le plan donc sans attente dessous —
exactement ce que [10](10-chantier-prescan-dictionnaire-rapport.md) §4.3
prévoyait) : le scan se parallélise mais **le tri reste** : 32,9 ms
d'attente pour `e`. Trier dans chaque partie puis tri stable adaptatif de
la concaténation : 27 → 10 ms, plus 5 de concaténation. Toujours 18 ms
pour une cellule, ×10 au-dessus de la cible.

## 3. Ne pas matérialiser : l'alternative par préfixe

La bonne question était [10](10-chantier-prescan-dictionnaire-rapport.md)
§4.4 : *faut-il* cette liste ? Dans `build_chains_from_splits`, la liste
des tokens commençant par le reste sert à **une chose** : la dernière
position de la chaîne « avalée » (le reste tient entier dans le token
suivant), où les résolveurs testent l'appartenance de l'ordinal trouvé à
`pos+1` (posmap) ou au prochain mot (word_pos_map) par recherche binaire
dans la liste. Or `.termtexts` donne le texte **étendu** de tout ordinal
(octets propres + overlap, ce que couvre exactement une clé SI=0) : « le
texte commence par le reste » est **la même question**, répondue sans
liste.

D'où `fst_walk::Alts` : une position d'une chaîne est `Ids(Arc<Vec<u64>>)`
(explicite, triée) ou `Prefix(Arc<str>)` (tout token dont le texte
minuscule commence par là). `Alts::contains(ord, termtexts)` fait la
recherche binaire ou le test de texte (ASCII sans allocation, sinon
`to_lowercase`). `build_chains_from_splits(…, prefix_alts)` construit un
`Prefix` pour la position avalée quand `prefix_alts` est vrai — c'est-à-dire
**sur un lecteur partagé (dictionnaire) et quand le résolveur peut tester**
(posmap + termtexts ; word_pos_map en plus côté mot) — gardé par le
**compte** ancré du reste (> 0), une cellule sans décodage ; sinon la
liste comme avant. La première position reste toujours explicite (on
résout ses postings) : `TokenChainV3::first_ids()` / `head()`. Les
résolveurs qui **énumèrent** les alternatives (`resolve_word_chains_v3`
sans word_pos_map, `resolve_chains_impl` en mode Strict/Relaxed sans
posmap) n'en voient jamais ; `Alts::explicit()` y panique avec un message
clair si c'était le cas. `resolve_chains_v3_posmap` prend maintenant
`termtexts: Option<&…>`.

L'index v3 (par segment, sans mémo) est **inchangé** : `prefix_alts` y est
faux, listes explicites comme avant — sa référence de temps reste la même.

Une différence théorique, non couverte : la FST minuscule les octets
propres et l'overlap **séparément**, le test de préfixe minuscule le texte
entier. Seul cas où ça diverge : le sigma grec en fin de token (`Σ` →
`ς` final quand il termine les octets propres, `σ` quand l'overlap le
suit dans le texte entier). Marginal ; à régler en minusculant les deux
morceaux séparément dans `starts_with_ci` si un corpus grec arrive.

Résultat, référence 10 000 (`idx-dict2`), `search` en ms, 9/9 identiques :

| requête | mémo seule | plan + préfixe |
|---|---|---|
| mutex_lock strict | 4,1 | 4,2 |
| mutex_lock relax | 9,5 | **4,0** |
| spin_lock strict | 3,7 | 3,4 |
| sched term | 23,7 | **6,7** |
| sched strict | 3,3 | 3,7 |
| printk sw | 4,6 | 3,5 |
| schdule fz1 | 48 | **11,0** |
| regsiter fz2 | 91 | **46** |
| spin_lock_[a-z]+ rx | 6,8 | 6,4 |
| schdule jw1 (chaud) | 9,6 | 8,1 |

Le plan vaut maintenant 0,5 à 5 ms partout (`schdule` : 5 vagues, 77
cellules, 5,0 ms ; la plus lente 2,6 ms, `count/02 "e"`). Le découpage en
sous-plages et `SPLIT_SCAN_MAX_PREFIX` n'ont plus d'objet : retirés
(`compute_in_tasks` avec eux) après les mesures ci-dessous.

## 3 bis. Ce qui restait : l'intersection avec le `.gmap`

Avec le plan et les alternatives par préfixe, le 30 000 passait de ×2-22
à ×2-5 (A/B intermédiaire, min de 3 passes : `mutex_lock strict` 7,1 ms,
relax 8,8, `sched term` 14,4, `schdule` fz1 24,1 contre 1,8 / 1,8 / 3,2 /
11,5 en v3). Le profil par segment (`V3_PROFILE`, sommes CPU) disait que
le mur n'était plus le plan (≤ 6 ms) mais **le travail par segment, ×7
celui de v3** : `mutex_lock strict` 92 ms de CPU contre 10.

Deux fausses pistes, mesurées : le verrou de la mémo (un `Mutex` sur la
table, pris à chaque consultation par 24 threads — passé en `RwLock` avec
une clé en octets sans allocation : rien de visible) ; les consultations
de `.termtexts` multi-générations dans la DFS de fratrie (0,3 µs l'une).

La vraie : `keep_in_segment`, la coupe d'une liste partagée aux
identifiants du segment. C'était une marche fusionnée — qui **parcourait
tout le `.gmap`** (25 000 identifiants) pour chaque liste, et une requête
fait 1 000 à 6 000 coupes par segment sur des listes de 200 entrées :
`mutex_lock strict` 418 660 entrées coupées → 9 355 gardées en **68 ms**
sur 88 de CPU ; `schdule` 1,45 M → 32 274 en 122 ms. Remplacée par une
intersection **en galop** depuis le côté le plus petit
(`GmapReader::lower_bound_from`, recherche exponentielle puis binaire) :
68 → 16 ms, CPU par segment 91 → 28 ms, `mutex_lock strict` 8,2 → 4,4 ms.

Puis deux réglages du plan : le compte d'un reste d'un ou deux octets
n'est plus demandé (`PREFIX_ASSUMED_MAX_BYTES` : `d` sur 6,5 M de textes,
4 ms, la cellule la plus lente ; un préfixe que personne ne commence ne
coûte que des tests d'appartenance ratés) ; et **une seule vague par
littéral** — les restes sont des suffixes de la requête, on les planifie
tous sans marcher (superset de ce que les segments demanderont) au lieu
d'une vague par profondeur de chaîne, chacune à 0,3 ms de latence
d'ordonnancement pour 0,1 ms de CPU. Les vagues de moins de trois
cellules sont calculées en ligne.

Profil 30 000 sur cinq requêtes après tout ça, `search` en ms (v3 entre
parenthèses) : `mutex_lock strict` 4,1 (2,0), relax 3,1 (1,7), `sched
term` 6,1 (3,4), `sched strict` 3,0 (2,1), `schdule` fz1 11,0 (13,3). Le
plan vaut 0,3 à 0,8 ms (fuzzy 1,5, trois vagues). Ce qui reste par
segment : la coupe (16 ms de CPU par requête, 9 µs par coupe de 230
entrées galopant dans 25 000), la DFS de fratrie (×2 : recherche binaire
dans le `.gmap` à chaque `siblings()`), et pour `sched` les candidats de
la racine (100 000 entrées sur trois partitions, marchés avec le `.gmap`
de chaque segment : 14 ms de CPU).

## 4. A/B 30 000 fichiers

Même binaire, 3 passes, min, `search` en ms ; index v3 (`idx30k-v7`,
conteneur 8) contre index dictionnaire (`idx30k-dict`, 120 segments,
6,5 M de textes). La colonne « mémo seule » est celle de [09](09-journal-chantier-dictionnaire.md) §11.

Les quatre colonnes de temps : v3 ; le dictionnaire le 5 au matin (mémo
seule, [09](09-journal-chantier-dictionnaire.md) §11) ; après le plan et
les alternatives par préfixe (§3, A/B intermédiaire) ; après la coupe en
galop et les réglages du plan (§3 bis, état final). 9/9 identiques aux
trois passes des deux côtés.

| requête | v3 | mémo seule | plan + préfixe | + galop, une vague | ratio final |
|---|---|---|---|---|---|
| mutex_lock strict | 2,1 | 10,2 | 7,1 | **3,9** | ×1,9 |
| mutex_lock relax | 1,7 | 21,3 | 8,8 | **2,9** | ×1,7 |
| spin_lock strict | 1,7 | 15,1 | 6,1 | **2,5** | ×1,5 |
| sched term | 3,3 | 69,6 | 14,4 | **5,3** | ×1,6 |
| sched strict | 2,1 | 5,5 | 5,3 | **2,9** | ×1,4 |
| printk sw | 2,4 | 22,4 | 9,3 | **3,8** | ×1,6 |
| schdule fz1 | 11,5 | 121,4 | 24,1 | **9,8** | ×0,9 |
| regsiter fz2 | 125,7 | 421,6 | 155,0 | **157,2** | ×1,3 |
| spin_lock_[a-z]+ rx | 10,6 | 18,3 | 17,0 | **17,1** | ×1,6 |
| schdule jw1 | 14,8 | 29,8 | 15,7 | **11,9** | ×0,8 |

Lecture honnête : de ×2-22 à **×0,8-1,9** à froid, au même binaire, sur
un index 20 % plus petit ; le fuzzy est **plus rapide** qu'en v3 (le plan
compte sans décoder, les segments ne coupent plus de listes de restes) ;
la règle du ×1,5 est tenue sur cinq requêtes sur dix et manquée de 0,1 à
0,4 sur les cinq autres — en valeur absolue, 0,8 à 2 ms de plus sur des
requêtes de 2 à 3 ms. Le mode dictionnaire n'est donc **pas encore le
défaut** ; il n'en est plus loin.

Ce qui reste, par ordre de rendement probable ([§3 bis](#3-bis-ce-qui-restait--lintersection-avec-le-gmap)) :

1. les coupes au `.gmap` : 16 ms de CPU par requête exacte (0,7 ms de
   mur), 9 µs par coupe de 230 entrées ; un `.gmap` à deux niveaux (index
   des premiers identifiants par bloc de 64) diviserait les défauts de
   cache du galop ;
2. les candidats de la racine sur une requête courte (`sched` : 100 000
   entrées, trois partitions, marchés avec le `.gmap` de chaque segment,
   14 ms de CPU) ;
3. la DFS de fratrie : `siblings()` fait une recherche binaire global →
   local dans le `.gmap` à chaque pas (×2 v3) ;
4. la regex (×1,6 : `spin_lock_` strict, dont la moitié en ancrage sur le
   second token) et le ~0,5 ms fixe hors prescan qui sépare encore les
   deux modes sur les requêtes courtes (à profiler : ouverture des vues
   par segment, `termtexts_reader()` multi-générations) ;
5. la statistique « mots longs » est celle du shard : en relâché, les 120
   segments marchent les chaînes chunk quand un seul a un mot de plus de
   256 octets (v3 : 2 segments sur 120) — un octet dans le `.gmap` par
   segment suffirait ; 3 ms de CPU par requête aujourd'hui.

**Couverture ajoutée** (5 septembre, soir) : variantes `sfx_version 4` de
`test_federated_search` (deux nœuds dictionnaire contre **un index v3**
qui tient tout : mêmes documents, **mêmes scores** ; le pré-filtre
compose), de `test_filtered_search_truth` (filtré = non filtré ∩ autorisés,
spans compris, onze types de requêtes, suppressions) et de
`test_luce_v3_roundtrip` (un dictionnaire shardé exporté puis importé
reste un dictionnaire et répond pareil). Toutes vertes.

## 4 bis. Le noyau entier, et une taille corrigée

Même protocole sur les 93 983 fichiers (`idx90k-dict`, 253 segments, 22,5 M
de textes, 2 générations) contre une référence v3 **reconstruite au format
courant** (`idx90k-v8`, conteneur 8, tables par blocs) — 3 passes, min,
`search` en ms, 9/9 partout :

| requête | v3 | dictionnaire | ratio |
|---|---|---|---|
| mutex_lock strict | 6,9 | 12,3 | ×1,8 |
| mutex_lock relax | 7,1 | 11,5 | ×1,6 |
| spin_lock strict | 7,1 | 11,4 | ×1,6 |
| sched term | 15,0 | 19,2 | ×1,3 |
| sched strict | 7,1 | 9,8 | ×1,4 |
| printk sw | 8,3 | 13,0 | ×1,6 |
| schdule fz1 | 50,8 | 44,1 | ×0,9 |
| regsiter fz2 | 638,7 | 765,4 | ×1,2 |
| spin_lock_[a-z]+ rx | 116,4 | 192,5 | ×1,7 |
| schdule jw1 | 78,5 | 68,3 | ×0,9 |

Le même profil qu'à 30 000 : l'approche **tient à l'échelle** — 22,5 M de
textes ne font pas exploser le plan (les racines courtes et les coupes y
sont ×3 plus lourdes, le ratio ne bouge pas). Les mêmes cinq requêtes
manquent le ×1,5 de 0,1 à 0,3.

**Taille, corrigée.** La référence v3 du noyau au format courant fait
**7,3 Go**, pas 11,06 : le 11,06 de [09](09-journal-chantier-dictionnaire.md)
§10 et de CLAUDE.md était l'index du matin du 5 (conteneur 5), pas un index
v3 au format du soir — la comparaison « 11,06 → 5,98 » mélangeait deux
étapes. À format égal, le dictionnaire vaut **7,3 → 5,6 Go, −23 %** sur le
noyau, cohérent avec le −20 % mesuré sur 30 000 (1 659 → 1 327 Mo). Le
×6,7 du texte reste vrai pour le dictionnaire ; le v3 au format courant est
à ×8,7.

## 6. Le `.gmap` à deux niveaux (5 septembre, soir)

Le premier point de la liste « ce qui reste » (§4) : le `.gmap` passe en
**layout 2** (`GMP2`, `gmap.rs`) — en-tête avec la **statistique « mots
longs » du segment** (u16, 0xFFFF = inconnue), les identifiants, puis
**la tête de chaque bloc de 64**. Le layout 1 (`GMAP`) s'ouvre toujours
(sans têtes ni statistique : galop et réponse du shard comme avant).

- `lower_bound_from` : la cible est-elle encore dans le bloc du dernier
  résultat (une comparaison) ? sinon recherche binaire sur les têtes
  (400 entrées, en cache), puis dans un bloc (256 octets). `local()` :
  têtes puis bloc, au lieu de quinze pas dans 100 Ko. L'intersection
  `keep_in_segment` et `siblings()` passent par là.
- La statistique par segment : `BriquesContext::segment_long_words`,
  posée depuis le `.gmap` par les trois requêtes ; `may_have_long_words`
  la préfère à celle du `.termtexts` du shard. Résultat sur 30 000
  relâché : `relaxed chunk walk: skipped=118 walked=2`, **comme en v3**
  (les 120 segments marchaient les chaînes chunk pour un seul mot long).
  Le collecteur la calcule sur ses métas (`SfxCollectorDataV3::
  max_word_content_len`), la fusion prend le max des entrées (inconnue si
  une entrée ne dit pas). Le plan planifie les cellules chunk dès qu'un
  segment du shard les marchera (`plan::dictionaries`, OU sur les `.gmap`).

Profil 30 000 (un passage, index reconstruit `idx30k-dict2`, 1,3 Go
comme avant) : CPU par segment `mutex_lock strict` 27 → 17 ms, relax 23 →
18, `sched term` 73 → 57 ; coupe 16,5 → 11,4 ms ; `search` 2,9 / 2,8 /
5,1 ms (v3 2,0 / 1,7 / 3,4). L'A/B trois passes : §6.1.

### 6.1 A/B 30 000, GMP2

Même binaire, 3 passes, min, `search` en ms, 9/9 partout ; index
dictionnaire reconstruit en GMP2 (`idx30k-dict2`) :

| requête | v3 | dictionnaire §4 | + GMP2 | ratio |
|---|---|---|---|---|
| mutex_lock strict | 2,0 | 3,9 | **2,9** | ×1,4 |
| mutex_lock relax | 1,7 | 2,9 | **2,3** | ×1,4 |
| spin_lock strict | 1,7 | 2,5 | **2,1** | ×1,2 |
| sched term | 3,3 | 5,3 | **5,1** | ×1,5 |
| sched strict | 2,0 | 2,9 | **2,5** | ×1,2 |
| printk sw | 2,3 | 3,8 | **3,2** | ×1,4 |
| schdule fz1 | 11,5 | 9,8 | **9,0** | ×0,8 |
| regsiter fz2 | 125,8 | 157,2 | **157,2** | ×1,2 |
| spin_lock_[a-z]+ rx | 9,9 | 17,1 | **15,7** | ×1,6 |
| schdule jw1 | 14,5 | 11,9 | **12,1** | ×0,8 |

**Le ×1,5 est tenu sur neuf requêtes sur dix** ; la regex reste à ×1,6
(15,7 contre 9,9 ms : trois littéraux stricts, `spin_lock_` ancré sur le
second token pour moitié). Ce qui reste pour elle : les listes des racines
ancrées (première position d'une chaîne, donc explicites) coupées par
segment — `anchored fst` 10 ms de CPU sur `mutex_lock strict` — et la
fenêtre regex elle-même, identique en v3.

## 7. La décision, l'option, et la vérité terrain sur le noyau (5 septembre, soir)

**Décision de Lucie** : le dictionnaire n'est **pas le défaut**, mais une
option facile à poser partout, avec une description qui dit ce qu'elle
fait — *réduit la taille disque et RAM d'environ 20 %, requêtes un peu
plus lentes à froid (×1,2 à ×1,6 sur les exactes, plus rapides en fuzzy),
mêmes réponses, fixée à la création*.

**L'option** : `shared_dictionary` dans `SchemaConfig` (alias de
`sfx_version` 4 : `effective_sfx_version()`, une contradiction entre les
deux clés est refusée, la clé est connue de `from_stored_json`). Exposée :
Python `Index.create(path, fields, shards=None, shared_dictionary=False)` et
`create_with_blob_store(..., shared_dictionary=False)` ; Node
`Index.create(path, fields, shards?, sharedDictionary?)` et
`BlobIndexOptions.sharedDictionary` (`index.d.ts` mis à jour à la main,
comme napi le générerait) ; C++ `lucivy_create` accepte maintenant un
objet schéma complet comme le chemin blob (`parse_schema_config`), un
`shards` > 1 prime ; emscripten `IndexConfig.shared_dictionary` (`lucivy.d.ts`,
JSDoc de `create`, README) ; bridge rag3db : le JSON de schéma tel quel.
Décrite dans chaque README, `lucivy_core/README.md` (section « A smaller
index »), le README racine, le CHANGELOG (« Unreleased », avec le format v4
de la branche), `IndexSettings::sfx_version`. Tests : `TestSharedDictionary`
(Python, mêmes réponses que l'index par défaut sur deux shards et plusieurs
commits, réouverture) et `tests/shared_dictionary.mjs` (Node, idem avec
spans) — pas de `maturin` ni `napi` sur cette machine, ils tournent en CI.

**Vérité terrain, mode dictionnaire** (`V3_SFX_VERSION=4`) :
`test_fuzzy_ground_truth` et `test_regex_ground_truth` (le dépôt lui-même,
comptes et spans contre le disque) acceptent la variable maintenant :
verts. Le noyau entier, index reconstruit au format courant
(`idx90k-dict2`, GMP2, 5,6 Go) : panel `v3_ground_truth_demo` **9/9**,
puis **tout `test_sfx_v3_ground_truth.rs`** sur les 93 983 fichiers
(`--include-ignored`, un thread) : §7.1.

### 7.1 La vérité terrain sur 90 000, dictionnaire

Première tentative : **tout** `test_sfx_v3_ground_truth.rs` avec
`V3_MAX_DOCS=100000 --include-ignored` — erreur : `V3_MAX_DOCS` vaut pour
chaque test, et les bancs de forme (`perf_shape_*`, `v3_distributed_*`,
`v3_sharded_filter_delete_delta`) reconstruisent **chacun** un index de
94 000 fichiers en RAM ; la machine s'est mise à genoux, l'éditeur de
Lucie a été tué. Noté dans [08](08-knowledge-dump-baselines-tests-outils.md)
§6 bis. Relancé proprement, un test à la fois, sur l'index disque
réutilisé (`idx90k-dict2`, GMP2) :

| test | corpus | résultat |
|---|---|---|
| `v3_ground_truth_demo` (panel 10, comptes et spans contre le disque) | 93 983 | 9/9 |
| `v3_ground_truth_contains` | 93 983 | **15 pass, 0 fail** (66 s) |
| `v3_ground_truth_coherence` (panel de 21 littéraux longs, strict et relâché) | 93 983 | **31 pass, 0 fail** (217 s) |
| `v3_distributed_two_nodes` | 3 000 | vert |
| `v3_distributed_coherence` | 3 000 | vert |
| `v3_sharded_filter_delete_delta` | 3 000 | vert |
| `test_fuzzy_ground_truth`, `test_regex_ground_truth` | le dépôt | verts |

Avec les variantes `sfx_version 4` de fédéré, filtré et roundtrip LUCE, et
`test_dictionary_index`, c'est toute la vérité terrain du dépôt qui tient
en mode dictionnaire, sur le noyau entier au format courant.

## 8. Prochain chantier : la compaction du dictionnaire en flux

Un commit n'écrit qu'une génération avec ses seuls textes nouveaux
([09](09-journal-chantier-dictionnaire.md) §9) ; mais au-delà de
`LUCIVY_DICT_MAX_GENERATIONS` (8) vivantes, la compaction est **naïve** :
`DictionaryField::all_texts()` charge tous les textes des vivantes dans un
`Vec` (22,5 M sur le noyau), et `write_generation` repasse chacun dans
`SuffixFstBuilderV3`, qui regénère tous les suffixes et les **retrie** —
le coût d'une construction complète, en RAM et en temps, tous les 8
commits ; jamais chiffré isolément.

Ce qu'il faut : une **fusion en flux**. Les N FST vivantes sont des flux
de clés triées ; leur union (`OpBuilder::union` de la crate FST, portée
par `lucivy_fst`) donne la FST compactée sans retrier, en fusionnant les
records de parents (v8) des clés présentes dans plusieurs générations ;
les `.termtexts` se concatènent par plages d'identifiants (section IDS).
Rien en mémoire au-delà des flux — comme `sparse_vector::merge_segments`
marche ses tables triées, et comme `merge_segments_dict` concatène déjà
les postings sans réinterner. À mesurer avant : le temps et la RAM
(`VmHWM` dans `/proc/self/status`, lu depuis le test — pas de
`/usr/bin/time` sur la machine) d'une compaction sur le noyau, puis la
même chose en flux. Où regarder : `indexer/dictionary_commit.rs`
(`fold_new_texts`, `write_generation`), `suffix_fst/dictionary.rs`
(`all_texts`, `compact`), `builder_v3.rs` (`encode_parent_record_v8`),
`termtexts_v3.rs` (`with_ids`, `id_runs`).

## 5. Ce que les docs d'avant disaient de faux

- [10](10-chantier-prescan-dictionnaire-rapport.md) §2 : « un nœud
  `PlanShardNode` dans `search_dag.rs` » — placé dans les requêtes (§1) ;
  « `FstMemo` et `for_segment` deviennent inutiles » — non : la mémo est le
  support du plan, la vue par segment reste la façon de couper les listes
  au `.gmap`. Ce qui est retiré : les préfetchs depuis les segments et le
  découpage en sous-plages.
- [10](10-chantier-prescan-dictionnaire-rapport.md) §4.3 : « la découpe
  par sous-plage a sa place dans le fan-out du nœud » — mesurée, elle ne
  résout rien : le scan se parallélise, le tri de la liste non ; et la
  liste n'a pas lieu d'être (§3).
- [09](09-journal-chantier-dictionnaire.md) §10 et CLAUDE.md : « noyau
  entier 11,06 → 5,98 Go » compare un index v3 du matin (conteneur 5) au
  dictionnaire du soir ; à format égal c'est 7,3 → 5,6 Go (§4 bis).
- [09](09-journal-chantier-dictionnaire.md) §8 « les chaînes se
  construisent par segment, le travail FST d'un reste est fait une fois »
  — vrai, mais le travail d'un reste court était une liste de 533 000
  entrées coupée à chaque segment par marche fusionnée : c'est un test de
  texte maintenant.
