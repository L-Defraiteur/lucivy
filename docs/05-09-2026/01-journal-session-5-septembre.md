# Journal de la session du 5 septembre 2026 — le dictionnaire devient utilisable

Écrit en fin de journée pour la session suivante, qui repart sans
l'historique. Se lit seul ; le détail brut, étape par étape, mesures
intermédiaires et fausses pistes comprises, est dans
[`../04-09-2026/11-journal-chantier-plan-fst.md`](../04-09-2026/11-journal-chantier-plan-fst.md).
L'architecture à jour : [02](02-architecture.md) ; les outils, baselines et
pièges : [03](03-knowledge-dump-baselines-tests-outils.md).

Tout est sur la branche **`v4`**, poussée : commits `4023f2d` (plan,
alternatives par préfixe, coupe en galop), `0d39507` (noyau entier,
variantes de tests), `57e453e` (`.gmap` layout 2), `8c4f580` (option
`shared_dictionary`, vérité terrain du noyau), puis les trois docs de ce
dossier.

---

## 1. Point de départ

Le dictionnaire partagé par shard (`sfx_version` 4, nuit du 4 au 5)
tenait sa promesse de taille — référence 10 000 fichiers 508 → 390 Mo,
30 000 : 1 659 → 1 327 Mo — mais **à froid, une requête était ×2 à ×22
plus lente** qu'en v3 sur 30 000 fichiers (`sched term` 3,2 → 70 ms,
`schdule` fz1 12 → 121). Cause : la phase FST d'une requête était faite
une fois (mémo du lecteur partagé) mais **sur un thread**, par le premier
segment qui la demandait, pendant que les 159 autres attendaient. La doc
[10](../04-09-2026/10-chantier-prescan-dictionnaire-rapport.md) proposait
un nœud « plan » par shard dans le DAG de recherche.

## 2. Le plan par shard — et sa première leçon

**Fait** : le plan vit dans `prescan_segments_more` des trois requêtes v3
(`contains_query_v3.rs`, `fuzzy_query_v3.rs`, `regex_query_v3.rs`), pas
dans `search_dag.rs` — c'est l'unique point d'entrée, pour l'index
simple, le shardé, la recherche par lots et la fédération. Module
`src/suffix_fst/briques/plan.rs` : par dictionnaire, il énumère les
cellules FST que la requête demandera et **remplit la mémo** (`FstMemo`)
par des tâches parallèles du scheduler avant le scatter par segment.
Personne n'attend sous ces tâches. Une cellule non prévue est calculée en
ligne comme avant : **le plan est une optimisation, jamais une condition
d'exactitude** (`V3_PLAN=0` le coupe).

**Première mesure** (10 000 fichiers) : rien ne bouge, mais le profil dit
pourquoi — les segments sont tous courts, et le plan prend tout, parce
qu'**une seule cellule fait le mur** : la liste des tokens commençant par
un reste **d'un octet** sur la partition mot (`e` : 533 385 entrées,
scan 11 ms, **tri 27 ms**). La découpe en 257 sous-plages parallélise le
scan, pas le tri. Fausse piste mesurée et retirée.

## 3. Ne pas matérialiser : `Alts::Prefix`

Cette liste ne servait qu'à **un** test d'appartenance : la dernière
position d'une chaîne cross-token « avale » le reste de la requête, et le
résolveur vérifie que le token trouvé à `pos+1` (posmap) ou le mot suivant
(word_pos_map) est dans la liste. Or `.termtexts` donne le texte **étendu**
de tout ordinal (octets propres + overlap — exactement ce qu'une clé SI=0
couvre) : « le texte commence par le reste » est la même question, sans
liste. D'où `fst_walk::Alts` : `Ids(Arc<Vec<u64>>)` ou `Prefix(Arc<str>)`,
`contains(ord, termtexts)`. Construit seulement sur un lecteur partagé et
quand le résolveur peut tester (posmap + termtexts, word_pos_map côté
mot) ; la première position d'une chaîne reste explicite ; les résolveurs
qui énumèrent paniquent avec un message clair s'ils en voyaient un.
**L'index v3 est inchangé.** Sur 10 000 : `sched term` 23,7 → 6,7 ms,
`schdule` fz1 48 → 11.

## 4. Ce qui restait par segment : la coupe au `.gmap`

À 30 000, ×2-5 encore. Deux fausses pistes mesurées (verrou de la mémo
passé en `RwLock` : rien ; consultations `.termtexts` multi-générations :
0,3 µs). La vraie : `keep_in_segment`, la coupe d'une liste partagée aux
identifiants du segment, était une marche fusionnée qui **parcourait tout
le `.gmap`** (25 000 ids) à chaque liste, et une requête fait 1 000 à
6 000 coupes par segment sur des listes de 200 entrées — 68 ms de CPU sur
88. Remplacée par une **intersection en galop** depuis le côté le plus
petit : 68 → 16 ms. Puis deux réglages du plan : un reste d'un ou deux
octets est présumé présent (pas de compte : `d` sur 6,5 M de textes
coûtait 4 ms), et **une seule vague par littéral** (les restes sont des
suffixes de la requête : planifiés tous, sans marcher, au lieu d'une vague
par profondeur à 0,3 ms de latence chacune).

## 5. A/B 30 000, même binaire, 3 passes, min (ms)

| requête | v3 | matin (mémo seule) | plan + préfixe + galop | + GMP2 (§8) | ratio final |
|---|---|---|---|---|---|
| mutex_lock strict | 2,0 | 10,2 | 3,9 | **2,9** | ×1,4 |
| mutex_lock relax | 1,7 | 21,3 | 2,9 | **2,3** | ×1,4 |
| spin_lock strict | 1,7 | 15,1 | 2,5 | **2,1** | ×1,2 |
| sched term | 3,3 | 69,6 | 5,3 | **5,1** | ×1,5 |
| sched strict | 2,0 | 5,5 | 2,9 | **2,5** | ×1,2 |
| printk sw | 2,3 | 22,4 | 3,8 | **3,2** | ×1,4 |
| schdule fz1 | 11,5 | 121,4 | 9,8 | **9,0** | ×0,8 |
| regsiter fz2 | 125,8 | 421,6 | 157,2 | **157,2** | ×1,2 |
| spin_lock_[a-z]+ rx | 9,9 | 18,3 | 17,1 | **15,7** | ×1,6 |
| schdule jw1 | 14,5 | 29,8 | 11,9 | **12,1** | ×0,8 |

9/9 identiques aux trois passes des deux côtés. **Le ×1,5 est tenu sur
neuf requêtes sur dix** ; la regex reste à ×1,6 (trois littéraux stricts,
racines ancrées sur le second token en listes explicites). Le fuzzy est
plus rapide qu'en v3.

## 6. Le noyau entier, et une taille corrigée

Même protocole sur 93 983 fichiers, contre une référence v3 **reconstruite
au format courant** (`idx90k-v8`) : ×0,9 à ×1,8, même profil qu'à 30 000
— l'approche tient à 22,5 M de textes. Au passage : cette référence fait
**7,3 Go, pas 11,06** — le « 11,06 → 5,98 » du matin comparait un v3 en
conteneur 5. À format égal, le dictionnaire vaut **7,3 → 5,6 Go, −23 %**,
cohérent avec le −20 % du 30 000. Corrigé partout.

## 7. Couverture ajoutée

Variantes `sfx_version 4` de `test_federated_search` (deux nœuds
dictionnaire contre **un index v3** qui tient tout : mêmes documents,
**mêmes scores**), `test_filtered_search_truth` (onze types de requêtes,
spans, suppressions) et `test_luce_v3_roundtrip` (un dictionnaire shardé
exporté puis importé reste un dictionnaire et répond pareil). Toutes
vertes, avec `test_dictionary_index` (v3 contre v4, 300 fichiers).

## 8. Le `.gmap` à deux niveaux (`GMP2`)

En-tête avec la **statistique « mots longs » du segment** (jusque-là celle
du shard : un seul mot de plus de 256 octets faisait marcher les chaînes
chunk à tous les segments en relâché — maintenant 2 sur 120, comme en v3)
et **la tête de chaque bloc de 64 identifiants** : l'intersection reste
dans le bloc courant tant que la cible y est, sinon cherche sur les têtes
(en cache) puis dans un bloc. `GMAP` (layout 1) s'ouvre toujours. CPU par
segment −25 % sur les requêtes courtes ; c'est la dernière colonne de §5.

## 9. La décision et l'option

**Décision de Lucie** : le dictionnaire **n'est pas le défaut**, mais une
option facile à poser partout, avec une description qui dit ce qu'elle
fait — *réduit la taille disque et RAM d'environ 20 %, requêtes un peu
plus lentes à froid (×1,2 à ×1,6 sur les exactes, plus rapides en fuzzy),
mêmes réponses, fixée à la création*.

`shared_dictionary` dans `SchemaConfig` (alias de `sfx_version` 4,
`effective_sfx_version()`, contradiction entre les deux clés refusée).
Python `Index.create(path, fields, shards=None, shared_dictionary=False)`
et `create_with_blob_store(...)` ; Node `Index.create(path, fields,
shards?, sharedDictionary?)` et `BlobIndexOptions.sharedDictionary`
(`index.d.ts` à la main) ; C++ `lucivy_create` accepte un objet schéma
complet ; emscripten `IndexConfig.shared_dictionary` ; bridge rag3db :
le JSON de schéma tel quel. Décrite dans chaque README, le README racine,
`lucivy_core/README.md`, le CHANGELOG (« Unreleased »). Tests Python
(`TestSharedDictionary`) et Node (`tests/shared_dictionary.mjs`) — mêmes
réponses que l'index par défaut ; pas de `maturin` ni `napi` ici, ils
tournent en CI.

## 10. Vérité terrain du noyau en mode dictionnaire, et un plantage

`test_fuzzy_ground_truth` et `test_regex_ground_truth` acceptent
`V3_SFX_VERSION` : verts. Noyau entier, index reconstruit au format
courant (`idx90k-dict2`, GMP2) : panel `demo` **9/9**, `contains` **15/15**,
`cohérence` (21 littéraux longs, strict et relâché) **31/31** ;
distribués et shardé-filtré-delta verts à 3 000 fichiers.

**L'incident** : lancer *tout* `test_sfx_v3_ground_truth.rs` avec
`V3_MAX_DOCS=100000` a mis la machine à genoux et tué l'éditeur de Lucie —
`V3_MAX_DOCS` vaut pour chaque test, et les bancs de forme (`perf_shape_*`,
distribués) **reconstruisent chacun un index de 94 000 fichiers en RAM**.
Règle : les vérités une par une (`-- --exact`), `V3_INDEX_DIR` sur l'index
disque, jamais deux constructions 90k à la fois
([03](03-knowledge-dump-baselines-tests-outils.md) §6 bis).

## 11. Ce qui reste, et le prochain chantier

- ~~Compaction du dictionnaire~~ — **fait plus tard dans la session**,
  §13 : fusion de flux, 48 s et 12,8 Go → 19 s et 229 Mo sur le noyau,
  fichiers identiques octet pour octet.
- La regex à ×1,6 : les listes des racines ancrées sur le second token
  (première position d'une chaîne, explicites) coupées par segment.
- La DFS de fratrie : `siblings()` fait une recherche global → local à
  chaque pas (×2 v3 ; GMP2 l'a réduite).
- Marginal, non couvert : la FST minuscule octets propres et overlap
  séparément, le test de préfixe minuscule le texte entier — seul le
  sigma grec final diverge.
- `index_bytes`, `preload`, `residency` ignorent encore les `dict-*`.
- Décisions en attente : la version 4.0.0 ; la pile v2 ; fusionner
  `wip/publication-3.0.0` dans `main` (trois commits du 28 août non
  poussés) ; le tri stable des ex æquo dans le merge des shards.

## 12. Ce que les docs d'avant disaient de faux

- Doc 10 : « un nœud `PlanShardNode` dans `search_dag.rs` » — placé dans
  les requêtes ; « `FstMemo` et `for_segment` deviennent inutiles » — non,
  la mémo est le support du plan ; « la découpe par sous-plage a sa place
  dans le fan-out » — mesurée, elle ne résout rien.
- Doc 09 §10 et CLAUDE.md : « noyau entier 11,06 → 5,98 Go » comparait un
  v3 du matin ; à format égal c'est 7,3 → 5,6.
- Doc 09 §8 : « le travail FST d'un reste est fait une fois » — vrai,
  mais c'était une liste de 533 000 entrées coupée à chaque segment ;
  c'est un test de texte maintenant.

## 13. La compaction du dictionnaire en fusion de flux

Mesurée d'abord, comme prévu, avec un banc qui compacte les générations
d'un index sur disque sans le reconstruire (`dictionary_compact::
compaction_of_an_index_on_disk`, ignoré, `LUCIVY_DICT_BENCH_DIR`) : la
compaction naïve (`all_texts` puis le builder) sur le dictionnaire du
noyau entier — 22,5 millions d'identifiants, 9,9 millions de clés, 131
millions de suffixes à retrier — coûtait **48 s et 12,8 Go de RAM
anonyme**, à chaque huitième commit. C'est ce que la construction du 90k
payait cinq fois, et la raison pour laquelle un index du noyau en mode
dictionnaire ne se construisait pas sur une machine ordinaire.

Le remplacement (`src/suffix_fst/dictionary_compact.rs`) :

- **Le `.sfx`** : les FST des générations sont parcourues ensemble dans
  l'ordre des clés (`OpBuilder::union` de `lucivy_fst`). Une clé tenue
  par une seule génération a son record de parents **copié tel quel** ;
  une clé tenue par plusieurs a ses parents concaténés, triés par
  (ordinal, sti), dédoublonnés, ré-encodés (`encode_parent_record_v8`) —
  ce que le builder aurait fait. La FST de sortie est construite dans la
  même passe **directement sur disque** (`MapBuilder::new(writer)`, mémoire
  bornée), la table des parents aussi ; les deux vont dans des fichiers
  temporaires (`dict-<g>.<champ>.sfx.fst.tmp`, `.sfx.parents.tmp`) parce
  que l'en-tête du conteneur veut leurs longueurs, puis le conteneur est
  assemblé en copiant depuis leurs mmaps (`file_v3::write_container`).
- **Le `.termtexts`** : un tas sur les curseurs des générations (chaque
  génération est croissante par identifiant), écrit en **trois passes**
  (offsets et plages d'identifiants, puis les métas, puis les textes) pour
  que seule la table des offsets soit en mémoire
  (`termtexts_v3::write_merged`, `MergedEntries`).
- **Quelles générations** (`choose_compaction`) : au-delà du maximum, les
  **plus petites**, autant qu'il faut pour ramener le compte à la moitié
  du maximum. La plus grosse génération ne rejoint une fusion que lorsque
  assez d'autres l'ont dépassée : un commit ne repaie plus jamais tout le
  dictionnaire, chaque octet est fusionné à peu près autant de fois que
  le compte double. Avec le maximum à 8 : un compte de 9 fusionne les 6
  plus petites, il en reste 4.
- Un fichier de génération laissé par un commit planté entre l'écriture
  et `meta.json` bloquait le commit suivant (le numéro est réutilisé, le
  répertoire refuse de créer un fichier existant) : `remove_leftovers`
  les efface avant d'écrire.

Vérité : `streamed_merge_equals_the_rebuild` (données synthétiques :
identifiants entrelacés entre trois générations, clés partagées, un
record de plus de 32 parents, entrées mot et chunk) et le banc en mode
`compare` — les fichiers fusionnés sont **identiques octet pour octet** à
ceux d'une reconstruction, sur 30 000 fichiers et sur le noyau ;
`test_dictionary_index` (trois générations au plus, deux compactions),
le panel vérifié sur un index 10 000 construit avec un commit tous les
500 fichiers et trois générations au plus (§13 bis).

| Dictionnaire (champ contenu) | naïf | flux |
|---|---|---|
| 30 000 fichiers, 7 générations, 4,1 M clés, 6,5 M textes | 13,0 s, 3,8 Go résidents | **7,2 s, 0,68 Go résidents** (dont les mmaps) |
| noyau, 2 générations (902 + 21 Mo), 9,9 M clés, 22,5 M textes | 48,0 s, **12,8 Go anonymes** | **18,9 s, 229 Mo anonymes** (1,9 Go résidents avec les fichiers mappés) |

Le temps restant du flux est la construction de la FST elle-même
(`MapBuilder`, 16 s sur 9,9 M clés) : la reconstruction naïve la payait
aussi (15,6 s de `build+serialize`), plus 27 s de tri. Et avec la
politique des plus petites, ce cas — la génération de 902 Mo dans la
fusion — ne se produit plus qu'une fois sur plusieurs dizaines de
commits.

### 13 bis. La vérité de bout en bout avec des compactions

Index de référence 10 000 reconstruit en mode dictionnaire avec un commit
tous les 500 fichiers et **trois générations au plus**
(`LUCIVY_DICT_MAX_GENERATIONS=3`, `V3_COMMIT_EVERY=500`,
`V3_INDEX_DIR` neuf) : **six compactions** pendant la construction
(générations 5, 9, 13, 17, 21, 25 ; 4 parts chacune, 0,7 à 1,4 s sur le
champ contenu, jusqu'à 1,15 M clés et 2,06 M textes), deux générations
vivantes à la fin (25 : 85,7 Mo, 26 : 13,5 Mo). Panel `v3_ground_truth_demo`
**9/9**, `v3_ground_truth_contains` **15/15**, `v3_ground_truth_coherence`
**31/31** — comptes et spans contre le disque. Lancement des deux
derniers : `<nom> -- --exact --nocapture`, **sans** `--ignored` (ils ne
sont pas ignorés : avec, « 0 passed, 12 filtered out »).
