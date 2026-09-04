# Piste retenue : un dictionnaire partagé par shard

Décision de Lucie le 4 septembre au soir, parmi les trois pistes de
conception listées dans [04](04-recap-journee-et-a-faire.md) §4 : celle-ci
est la plus rentable, et la plus proche de ce qui a déjà été fait pour
`sparse_vector` en août (dimension = identifiant global de token, tables
triées, fusion sans remappage). Ce document fixe la mesure qui la justifie,
ce qu'elle change, et ce qu'il faut mesurer avant de coder.

---

## 1. La mesure

Sur les index de référence de ce soir (format v4, étape 8), en comptant les
textes distincts de `.termtexts` (texte étendu + partition) à travers tous
les segments du champ `content` :

| index | segments | ordinaux (somme) | distincts | répétition | dictionnaire partagé |
|---|---|---|---|---|---|
| 10 000 fichiers | 160 | 5 165 273 | 2 335 603 | **×2,21** | 45 % de l'actuel |
| 30 000 fichiers | 120 | 14 837 570 | 6 478 014 | **×2,29** | 44 % de l'actuel |

Trois quarts des textes distincts n'existent que dans un segment (ce sont
les identifiants rares), mais ils ne font qu'un tiers des ordinaux ; le
reste est répété, et 5 % des ordinaux sont des textes présents dans la
moitié des segments — les `if (`, `return`, `struct` du noyau, avec leurs
clés, leurs parents et leurs liens de fratrie recopiés 60 à 80 fois.

**Ce que ça vaut.** Le dictionnaire — `.sfx` (FST + parents), `.termtexts`,
`.sibling_v3` — fait 71 % de l'index. À ×2,2 de répétition, le partager
ramène ces 71 % à environ 32 %, soit **−35 à −40 % de l'index** en plus des
−36 % du jour : 735 → ~450 Mo sur 10 000 fichiers, ×21 → ~×7 sur le noyau
avant toute compaction. Les postings (`.sfxpost`, `.word_sfxpost`) et les
cartes de positions restent par segment : ils décrivent des occurrences,
pas des termes.

**Ce que la compaction fait déjà, et pourquoi elle ne suffit pas.** Fusionner
des segments dédoublonne le dictionnaire de la même façon — l'index de
bench du 28 août, 10 segments, était à ×13,5 quand celui de ce soir, 253
segments, est à ×12,3 après −36 %. Mais la fusion a deux défauts que le
partage n'a pas : elle recopie tout à chaque étage (le coût de fusion
mesuré en août, 190 s à 1 150 s sur 50 000 documents), et elle bute sur les
**24 bits d'ordinal** : ce soir, `merge_segments_v3` a refusé deux fois de
fusionner le noyau vers 10 segments (« 17 898 500 distinct terms across
4 segments exceed the 16 777 216 ordinals the v3 encoding can address »).
Un segment ne peut pas dépasser ~50 000 fichiers de noyau.

---

## 2. Ce que ça change

**Le modèle.** Aujourd'hui un segment porte tout : dictionnaire et
occurrences, avec des ordinaux locaux. Demain :

- **un dictionnaire par shard**, en générations : une FST (clés = suffixes
  des chunks et des mots, comme aujourd'hui) + table de parents + textes +
  fratrie, à identifiants **globaux au shard** ;
- **des segments qui ne portent que les occurrences** : `.sfxpost`,
  `.word_sfxpost`, `.posmap`, `.word_pos_map`, le docstore, indexés par
  identifiant global.

**L'indexation.** Un commit interne ses nouveaux textes dans le dictionnaire
du shard. Une FST est immuable, donc le dictionnaire est **générationnel**,
exactement comme les segments de `sparse_vector` : chaque commit écrit une
petite génération avec ses nouveaux termes seulement, une requête consulte
les N générations vivantes (N petit), et une compaction de dictionnaire les
fusionne de temps en temps — une fusion de tables triées, sans remappage
puisque les identifiants sont globaux. Les segments d'occurrences, eux, ne
se fusionnent plus que pour leurs postings.

**La requête.** C'est le point que je n'avais pas vu en listant la piste :
aujourd'hui le prescan marche la FST **de chaque segment** — 253 marches de
`mutex_lock` sur l'index de ce soir. Avec un dictionnaire par shard, une
marche par génération de dictionnaire, puis les postings de chaque segment
par identifiant global. Moins de travail, pas plus.

**La fusion.** Réduite aux postings : concaténation par identifiant global,
comme `segments::merge_segments` de `sparse_vector` marche ses tables
triées ensemble.

**La fédération et le distribué : intacts, vérifié dans le code.** Ce qui
voyage entre nœuds est `ExportableStats` (`bm25_global.rs`) : `doc_freqs`
indexé par les octets du terme, `contains_doc_freqs` par le texte de la
requête, `regex_doc_freqs` par le motif. Aucun ordinal ne quitte un shard.
Le dictionnaire est **par shard**, pas par nœud : un shard reste l'unité
autonome qui se déplace, s'exporte et se fédère, et
`export_stats → merge → search_with_global_stats` ne voit rien.

**Les deltas et la synchronisation** (LUCID, LUCIDS, le navigateur) : un
`ShardVersion` porte aujourd'hui une version et des identifiants de
segments (`lucistore/src/delta_sharded.rs`). Demain un shard a deux sortes
d'unités, générations de dictionnaire et segments d'occurrences, toutes deux
append-only : le delta reste « ce que le client n'a pas », avec une règle en
plus — les générations avant les segments qui les citent — et une
compaction de générations est une nouvelle version, comme une fusion de
segments l'est déjà. Contrainte réelle : un segment d'occurrences n'a de
sens qu'avec ses générations, donc l'import d'un delta est atomique sur le
couple (temporaire + `rename` + `sync`, comme le commit de `sparse_vector`).

**Les bornes.** Le noyau entier a de l'ordre de 15 à 20 millions de textes
distincts : **un dictionnaire de shard dépasse les 24 bits** du mot de
parent actuel. Le partage impose donc d'élargir l'ordinal, et la voie
propre est celle qui règle aussi l'overlap (piste 1 de [04](04-recap-journee-et-a-faire.md)) :
tous les parents dans la table à taille variable, la FST ne portant qu'un
offset. Les deux refontes n'en font qu'une.

---

## 3. Ce qu'il faut mesurer avant de coder

1. **Le vrai plafond de gain** sur le noyau entier : distincts contre somme
   sur les 253 segments (le compte ci-dessus est sur 10 000 et 30 000
   fichiers ; à 93 983 la répétition est probablement plus forte).
2. **Le coût d'une génération** : temps d'écriture d'une FST de *k* nouveaux
   termes par commit, et combien de générations une requête tolère avant
   de perdre ce qu'elle gagne sur les marches par segment.
3. **La taille d'un identifiant global** dans les postings : 3 octets
   aujourd'hui dans `.posmap`, il en faudra 4, soit +25 % sur ce fichier,
   à mettre en face des −35 %.
4. **La mémoire de construction** : le dictionnaire de shard est-il en RAM
   pendant l'internement (une table de hachage de 6 millions d'entrées),
   ou consulte-t-on la FST ? Compte pour le WASM (128 Mo de heap collecteur).

---

## 4. Ce que ça touche dans le code

`collector_v3` (internement → contre le dictionnaire du shard),
`builder_v3` + `file_v3` (générations, ordinaux larges, parents en table),
`sfx_dag_v3` (fusion = postings seulement), les lecteurs de `briques/`
(N dictionnaires par shard, un prescan par génération), `ShardedHandle`
(un dictionnaire par shard, routage inchangé), snapshot/delta dans
`lucistore` (générations dans LUCE/LUCID/LUCIDS), et la fédération
(`export_stats` par shard, inchangé dans l'idée). Le format des segments
d'occurrences ne change pas, ce qui est ce qui rend le chantier faisable
par étapes mesurées comme aujourd'hui.
