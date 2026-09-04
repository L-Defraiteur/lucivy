# Réduire la taille d'index — ce qu'on sait, et dans quel ordre on agit

4 septembre 2026. Le point de départ est la mesure du 28 août
([07](../28-08-2026/07-rapport-progression-et-taille-index.md)) : sur 93 983
fichiers du noyau, l'index fait **×21 le texte** (18 Go pour 857 Mo), contre
×0,8 pour tantivy en trigrammes et ×3,6 pour Elasticsearch. La promotion est
en pause tant que ce rapport tient. L'audit du format v3 est dans
[02](02-audit-taille-index-sfx-v3.md) ; ce document en tire le plan.

**Le chantier se fait sur la branche `v4`, jamais sur `main`.** Le format
change, un binaire 3.0.x ne lira pas un index v4 ; la version du workspace
passera à 4.0.0 quand ce sera publié.

---

## 1. Ce qu'on sait

Mesuré sur l'index de bench (93 605 fichiers, 10 segments, 11,56 Go, ×13,5),
en décodant un segment entier avec `benches/scan_index_size.py` :

1. **Le `.sfx` n'est pas une FST, c'est une table de parents.** 42,5 % de
   l'index, dont 1,24 Go de FST et **3,58 Go de table de parents** : une
   entrée de 11 octets par (suffixe, chunk), 325 millions d'entrées, qui
   répète pour chaque suffixe cinq champs de méta déjà stockés une fois par
   ordinal dans `.termtexts`.
2. **La FST a 13,4 clés par chunk distinct**, dont **3,95 millions de clés
   « marqueurs »** par segment qui ne servent qu'à rendre final le nœud à la
   frontière du token — une frontière que `own_len − sti` connaît déjà.
3. **`.bytemap` (11 %) répond à une seule question** sur tout le chemin v3 :
   « ce chunk contient-il un octet de contenu ? ». Deux champs de META y
   répondent.
4. **Le texte stocké n'est pas le problème** : `.store` fait 3 %. Et
   `.posmap` n'est pas dérivable à la requête sans coût explosif (des
   secondes par requête) — il reste.
5. **La compaction plafonne à 40 %** parce que 64 % de l'index est
   proportionnel aux *ordinaux distincts par segment* : un chunk présent
   dans dix segments y est dix fois, avec ses treize clés et sa méta.
6. **En WASM chaque octet gagné sur disque est un octet de RAM** : au-delà
   de 64 Ko, une lecture charge le fichier entier dans un LRU de 768 Mo.

Ce que l'audit **n'a pas** fait : lancer une requête. Tous les effets sur le
temps de requête sont des raisonnements sur le code, et le protocole ci-
dessous existe pour ça.

---

## 2. Ce qu'on ne fera pas

**Pas de suffix array à la place de la FST.** Il gagnerait 26 % sur le
papier, mais il réécrit toute la marche (`fst_walk.rs` : `falling_walk`,
look-ahead d'overlap, scan de plage), c'est-à-dire l'endroit exact où les
optimisations de temps de requête des derniers mois ont été faites. Le
risque n'est pas qu'il soit faux ; c'est qu'une fonctionnalité cesse de
marcher, qu'on la rattrape au cas par cas, et qu'on finisse avec des temps
qui explosent. Trop expérimental pour un chantier dont le but est de ne rien
perdre.

**Pas de dérivation de `.posmap`, `.word_pos_map` ni des octets des mots à
la requête** : plus lent par match émis, pour 3 à 6 %.

---

## 3. L'ordre d'action

Chaque étape est un commit qui passe le protocole du §4 avant le suivant.
Gains en % de l'index de bench, estimés dans l'audit ; ils se composent.

| # | changement | estimé | **mesuré (10 k)** | état |
|---|---|---|---|---|
| 1 | Table de parents : `u64` packé au lieu de 11 octets | −9 % | −9,6 % | **fait** `30ad3da` |
| 2 | Supprimer `.bytemap` (META répond) | −11 % | −16,0 % | **fait** `d0bb7d3` |
| 3 | `.posmap` en 3 octets par position | −1,4 % | −0,9 % | **fait** `2b3f5a8` |
| 4 | `.sibling_v3` sans `gap_len` (META) | −1 % | −1,1 % | **fait** `ff013a4` |
| 6 | `.termtexts` : méta dans la table d'offsets, 8 o par ordinal | −2 % | −1,1 %, **et rend les 3 ms** du fuzzy relâché | **fait** `84c9c6a`, passé avant la 5 |
| 5a | Plus de suffixe qui commence dans l'overlap | (partie de −17 %) | −6,8 % | **fait** `3ab5ba6` |
| 5b | Plus de marqueurs, clés arrêtées à `own_len` | (reste de −17 %) | — | **renoncé** : la clé `_` porte 54 747 parents ; sans marqueur, chaque frontière de ce type coûterait leur décodage plus une lecture de `.termtexts` par parent, ×2 à ×3 sur `mutex_lock`. Reviendrait seulement avec l'overlap dans la valeur du parent, impossible dans les 63 bits actuels |
| 8 | Parents en delta-varint, conteneur `.sfx` v5 | −7 % | −8,1 % | **fait** `37d6d52` |
| 7 | `.sfxpost` sans `bt − bf` ; `.word_sfxpost` sans `to − from` si l'hypothèse tient | −2,6 % | — | à faire |
| 9 | Deux espaces d'ordinaux (chunks / mots) | −1,7 % | — | à faire |

Les deltas mesurés se composent : **−36,2 %** sur 10 000 fichiers
(1 152 → 735 Mo), −33 % sur 30 000 (3,4 → 2,3 Go). Journal, mesures et
commandes : [03](03-journal-des-etapes.md).

Ce qui reste (7 et 9) vaut environ −4 %. Au-delà, ce qui pèse encore sur
les 735 Mo : la FST elle-même (256 Mo, 35 %), les parents (154 Mo, 21 %),
les deux fichiers de postings (147 Mo, 20 %). Le prochain palier n'est plus
de l'encodage : c'est le nombre de clés de la FST (6 par chunk après 5a,
dont la moitié de marqueurs) et les postings de chunks.

`LUCIVY_MIN_SUFFIX_LEN=3` (−6 %, zéro code) reste **hors plan** tant qu'un
test ne prouve pas qu'une requête d'un ou deux octets en fin de valeur
survit.

---

## 4. Le protocole, pour chaque étape

1. **Index de référence** : 10 000 fichiers de `/tmp/lucivy-cmp`, construit
   par le harnais avec `V3_INDEX_DIR`, une fois avant le changement, une
   fois après. Les tailles par fichier viennent de
   `benches/scan_index_size.py` sur les deux.
2. **Justesse** : le panel `v3_ground_truth_demo` (comptes **et** spans
   comparés au disque) doit rendre exactement les mêmes lignes qu'avant.
   `bench_sharding` ne compte pas : ses « 20 hits » sont le plafond.
3. **Temps** : le même panel chronométré, deux passes, machine au repos
   (`uptime` noté dans le rapport). Priorités posées par Lucie le
   4 septembre : la taille disque et RAM est ce qui rend la lib inviable en
   prod, l'exactitude est ce qu'on vend, et le temps est acceptable **tant
   qu'une requête ne s'approche pas de ×1,5**. Une milliseconde perdue
   contre des pourcents d'index est un bon échange ; on l'écrit, et on
   l'optimise ensuite si on sait d'où elle vient.
4. **Tests** : `cargo test --lib` et `cargo test -p lucivy-core`, verts.
5. **Compatibilité** : le lecteur accepte l'ancien et le nouveau format
   pendant tout le chantier (octet de version du conteneur), de sorte que
   l'index de référence v3 reste lisible et que la fusion d'un segment v3
   avec un segment v4 reste possible.

Les commandes exactes sont dans le knowledge dump du 28 août
([09](../28-08-2026/09-knowledge-dump-tests-benchs-publication.md), §2 et
§4).

---

## 5. État de la branche au départ

`v4` = `wip/publication-3.0.0` + l'audit. `wip/publication-3.0.0` = `main`
+ trois commits du 28 août non poussés (les deux bancs de comparaison et
les rapports 06 à 09), aucun code moteur. Le tag `v3.0.8` est trois commits
derrière `main`. Vérifié le 4 septembre : les 26 commits orphelins du dépôt
sont des stashes et des jumeaux de rebase dont le contenu est intégré ;
rien à récupérer.
