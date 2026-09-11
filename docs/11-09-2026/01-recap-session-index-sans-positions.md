# Récap de session — du 7 au 11 septembre 2026 (branche `v4.1`)

## 1. Diffusion (7-8 septembre)

- README des bindings : le tableau « one corpus, one truth » en place du lien
  mort vers le rapport, liens absolus (npm et PyPI ne résolvent pas `../../docs`).
- Réponses aux issues #10 à #15 postées (compte L-Defraiteur) ; #15 sans
  portage ni guide, l'import comme seule aide prévue ; #13 laissée ouverte.
- **4.0.2 publiée** (tag `v4.0.2` sur `5937c3a`) : correctif du répertoire blob
  (un fichier de verrou restait après une panne du store → `LockBusy`) et un
  test LUCE qui comparait un top-10 d'ex æquo. Le job `checks` a rougi deux
  fois sur des tests intermittents avant ; tag posé seulement sur CI verte.
- Post LinkedIn relu, posts Reddit (`docs/07-09-2026/02-04`), GIF refait
  depuis l'enregistrement du 7 (`docs/07-09-2026/images/demo.gif`, `.mp4`),
  carte de lien `og-card.png` refaite. r/rust : 900 vues, peu d'effet ;
  r/programming refuse l'autopromotion.
- Article « Every full-text engine lies a little »
  (`playground/blog/every-engine-lies-a-little.html`, copie
  `docs/07-09-2026/06-…md`), relu et corrigé (règle en encadré, tableau
  lisible, ligne strict, `©`, `pin_loc`, « not mine, today »). Pas encore
  soumis à HN (guide : `docs/07-09-2026/07-…`).
- Sonde tantivy/Elasticsearch (`docs/07-09-2026/08-…`, rapport §3 bis) : en
  trigrammes tout ≥ 3 caractères est trouvé ; < 3 caractères = zéro silencieux.
- Suggestions 4.1 notées : captures agrégées et casse (`docs/07-09-2026/05`),
  pistes de taille (`docs/07-09-2026/09`).

## 2. Le chantier : l'index sans positions (`positions: false`)

Note complète : `docs/08-09-2026/01-chantier-positions-optionnelles.md`.

**Ce que c'est.** Une option de création. Les postings ne gardent que les
documents et les fréquences (`SFP6`, `WSP6`) ; `.posmap`, `.word_pos_map`,
`.sibling_v3` ne sont ni écrits ni calculés. Une requête prend ses candidats
dans l'index (la phase FST ne lit aucune position ; chaînes bâties depuis
toutes les têtes, sans table des voisins) et vérifie chaque candidat sur le
texte stocké avec les définitions mêmes de la vérité terrain
(`src/suffix_fst/briques/stored.rs`). Champs texte stockés obligatoires ;
refusé avec `derived_in_ram` ; un index créé ainsi ne s'ouvre pas en 4.0.x.

**Résultats (noyau Linux 7.2 recloné : 101 141 fichiers, 941 Mo — le harnais
suit 12 liens symboliques de répertoires ; pas comparable chiffre pour chiffre
aux 93 983 fichiers du README) :**

| | index par défaut | `positions: false` |
|---|---|---|
| taille | 5 289 Mo (×5,62) | **2 598 Mo (×2,77), −51 %** |
| indexation | 109 s | 101 s |
| pic mémoire (harnais, index en RAM) | 15,4 Go | 13,6 Go |
| littérales | 11-19 ms | 27-56 ms |
| `schdule` fz1 / Jaro-Winkler | 50 / 78 ms | 203 / 198 ms |
| `regsiter` fz2 | 856 ms | 388 ms |
| regex | 237 ms | 22 ms |
| `de`, 7,9 M spans | 628-706 ms | 327 ms |

10 000 fichiers : −37 % ; 30 000 : −41 %. Panel de vérité terrain 10/10 aux
trois échelles. Temps pris fichiers en cache (à froid : non mesuré).

**Commits principaux (v4.1)** : `2f5c6b0` layout + littérales ; `c481cf0`
fuzzy + regex ; `70abfca` Myers + Jaro fenêtré ; `4da3487` préfiltre + trace +
bindings ; `10c3636` préfiltre retiré des littérales ; `7097b0c` chemin
bit-parallèle toujours pour les aiguilles courtes ; `39f676b` plus rien de
positionnel calculé ; docs jusqu'à `04eb765`.

**Accélérations exactes** : `fuzzy_spans::last_row` (Myers), `within_distance`
(sortie anticipée), `fuzzy_spans_long` (mémoire linéaire), `jaro_spans_windowed`
— chacune égale à la version complète sur des milliers de cas aléatoires.

**Décisions de Lucie (11 septembre)** : les temps sont acceptables (tout sous
la seconde) ; **pas** de raccourci des sous-chaînes d'un seul jeton ni de
filtre de pièce par jeton (peur de perdre en exactitude) — « un truc à la
fois » ; l'option reste optionnelle, le défaut ne change pas.

## 3. Incidents appris

- `/tmp` vidé à 10 jours : le noyau vit dans `~/lucivy_bench/linux-7.2`,
  `/tmp/lucivy-cmp` et `/tmp/lucivy-cmp-90k` sont des liens (mémo).
- L'outil Bash tourne sous zsh : `$CMD` n'est pas découpé — une chaîne de
  mesures a tourné à vide ; passer par un script bash (mémo).
- `/usr/bin/time` absent : pic mémoire par `VmHWM` (script dans le dump).
- Pytest affiche des « background fold failed » quand un test efface son
  répertoire pendant le repli du dictionnaire : tests verts, non vérifié si
  4.0.2 les affiche aussi.

## 4. La suite (objectifs)

1. **Rendre l'option facile à utiliser, documentée, interfacée** : README
   principal (section faite), README des quatre bindings (« What's new in
   4.1 »), `lucivy_core/README.md`, ARCHITECTURE.md, typages, playground
   (`?nopos` + libellé), exemples. Les interfaces existent déjà : Python
   `positions=False`, Node 7ᵉ argument / option `positions`, C++ et navigateur
   `"positions": false` dans l'objet schéma.
2. Mesurer à froid (caches vidés) avant d'annoncer des temps.
3. Publier 4.1 sur décision de Lucie (CI verte d'abord, tag par elle).
4. Plus tard : soumettre l'article à HN ; captures agrégées ; casse.
