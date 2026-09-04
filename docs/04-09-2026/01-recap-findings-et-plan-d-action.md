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

| # | changement | gain | modèle de recherche |
|---|---|---|---|
| **1** | **Table de parents compacte** : le même `u64` que la valeur inline (63 bits utiles) au lieu de 11 octets ; `count` en varint | −9 % | intact — même décodage, moins d'octets |
| **2** | **Supprimer `.bytemap`** : contenu ⇔ `own_len − sep_len > 0` en META ; le préfiltre DFA regex relit les ≤ 8 octets du chunk | −11 % | intact |
| **3** | `.posmap` en 3 octets par position (ordinaux ≤ 2²⁴) | −1,4 % | intact |
| **4** | `.sibling_v3` sans `gap_len` (c'est la longueur de contenu de la destination, en META) | −1 % | une lecture META de plus dans la branche relaxée |
| **5** | **Clés sans overlap** : la clé s'arrête à `own_len`, l'overlap se vérifie sur `termtexts` ; plus de marqueurs, plus de suffixes en zone d'overlap | −17 % | **touché** : `add_token`, `check_split`, `fst_candidates_v3` — l'étape à prouver le plus durement |
| 6 | `.termtexts` : META 6 → 4 octets, offsets par bloc | −2 % | intact |
| 7 | `.sfxpost` sans `bt − bf` (= `own_len`) ; `.word_sfxpost` sans `to − from` si l'hypothèse tient | −2,6 % | une lecture META par ordinal résolu |
| 8 | Parents en delta-varint (≈ 5,5 o) **si** le décodage séquentiel ne coûte rien sur les listes de 300 k | −7 % de plus | décodage plus branchu — à mesurer |
| 9 | Deux espaces d'ordinaux (chunks / mots) | −1,7 % | plomberie |

Étapes 1 à 4 : encodage pur, aucun algorithme touché, environ **−22 %**.
Étape 5 : le vrai changement, et le seul qui baisse aussi la RAM de
construction (−46 % d'entrées dans le builder) et le cache WASM. Cumul
1 à 8 : de l'ordre de **−45 à −50 %**. Ce n'est pas ×5 ; c'est ce qui se
fait sans toucher aux occurrences ni au modèle de recherche, et c'est le
préalable à toute discussion sur le reste.

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
   (`uptime` noté dans le rapport). Une étape qui ralentit une requête
   n'est pas prise, quel que soit son gain.
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
