# Journal — 5 septembre 2026, nuit, et 6 au matin : la vitrine, le banc, la présentation, et le dictionnaire à l'indexation (mesuré, puis le repli différé)

Suite de [06](06-journal-session-5-septembre-soir.md) (les postings sans
octets, `derived_in_ram`, la fixture 3.0.8, 4.0.0). Pour repartir : ce
fichier (**§9 : le matin du 6, le repli différé**), puis [04](04-progression-et-a-faire.md) (l'état et le todo — §2 ter
`?ram`, §2 bis les corpus, §2 quinquies le banc, **§2 sexies le chantier
indexation, fait**), [09](09-plan-d-action-presentation.md) (sur quoi on se vend), puis
[11](11-architecture.md) et [12](12-knowledge-dump-baselines-tests-outils.md),
autonomes. Branche `v4`, jamais `main` ; `origin/v4` au niveau du dernier
commit. **4.0.0 numéroté, rien n'est publié.**

## 1. `derived_in_ram` dans le navigateur : mesuré, pas la vitrine

Noyau 15 429 fichiers et MDN, même build, page rechargée entre chaque
réouverture ([04](04-progression-et-a-faire.md) §2 ter) : OPFS −26 % / −23 %,
temps avant service égal (le rebâti coûte 1 s, le `preload` en rend 1,4),
requêtes égales — mais le **pic mémoire** monte : +524 Mo pendant
l'indexation du noyau (3 859 Mo sur 4 096), +252 au repos, +260 sur MDN.
Ce ne sont pas les dérivés (411 Mo, que le `preload` charge aussi sans
l'option) : ce sont les temporaires du rebâti dans une mémoire linéaire qui
ne redescend jamais. Décision de Lucie : option du playground (`?ram`), pas
la vitrine ; l'option reste entière en natif (×3,9 le texte).

## 2. Douze corpus au prompt, et deux bugs de la page

Choix de Lucie : MDN, Go, Godot, **Linux 2.6.0 entier** (« la version
autocontenue ancienne, pas une portion de l'actuel »), TypeScript à
l'essai, et tous les légers (PostgreSQL, CPython, Redis, Git, curl, SQLite,
nginx). Chaque visiteur ne télécharge que celui qu'il tape (122 Mo en tout).

- `playground/corpora.json` (source, licence, panel de requêtes ; `stats`
  écrits par le script) lu par la page : `EXTRA_CORPORA`, l'aide et
  l'invite en découlent. `tools/build_corpus.py` : même filtre que la page
  (extensions, ≤ 100 Ko, pas de NUL), listes élargies des deux côtés
  (`.rst`, `.gd`, `.cs`, `.s`…). Étape « Build the corpora » dans
  `pages.yml` : **les archives n'étaient pas déployées** (ignorées par git,
  fabriquées à la main), le site publié aurait rendu 404 sur `index mdn`.
- Mesuré dans Chrome ([04](04-progression-et-a-faire.md) §2 bis) : 2.6.0
  14 032 fichiers en 28 s et 1 087 Mo ; MDN 14 s ; Go 19 s ; Godot 19 s ;
  **TypeScript 39 044 fichiers en 33 s, 462 Mo, pic inchangé** (fichiers
  minuscules) ; PostgreSQL et CPython 10 s ; les petits 1 à 5 s.
- **Bug 1** : `drop <nom>` puis `index <nom>` dans la même page →
  « lucivy_create returned null », `I/O error (os error 29)` : WASMFS garde
  en cache le répertoire supprimé par le fil principal. Export
  `lucivy_drop_index`, opération `dropIndex` du worker,
  `Lucivy.dropIndex(path)`, `drop` l'appelle avant `removeEntry`.
- **Bug 2** : le lecteur tar de la page ne lisait que les 100 octets du
  champ nom — préfixe ustar, entrée GNU `L`, en-tête PAX `x` ignorés :
  **9 736 fichiers de TypeScript sur 39 044 perdus en silence** (29 312
  documents au premier essai), 266 de Godot, et tout dépôt GitHub aux
  chemins longs. Corrigé dans `extractTarGz`.
- Licences : redistribuer les sources avec leur notice est permis par toutes
  (Redis compris, RSALv2 / SSPLv1 / AGPLv3) ; rien ne déteint sur le MIT de
  lucivy, les corpus sont des données à côté, jamais dans le dépôt.

## 3. L'invite libre, et le pic mémoire borné

- **`$ lucivy `** puis `search "…" --fuzzy 1`, `index <nom>`, `open`, `drop`,
  `list`, `help`, ou une valeur nue (une recherche) ; `lucivy ` en trop
  avalé ; `search ` proposé, effaçable, en fin de démo et après chaque
  recherche ; `--help` seul marche et s'affiche `lucivy --help`, l'usage
  liste toutes les commandes.
- **Commit tous les 8 Mo de texte** en plus des 2 000 fichiers
  (`?commitmb=M`) : le pic suit la taille des segments, pas le nombre de
  documents — Godot 3 323 → **1 778 Mo**, 2.6.0 3 391 → **2 023**, pour 10 à
  13 s de plus ; 16 Mo mesuré (+1 285 Mo pour 4 s gagnées) et refusé.
  L'estimation affichée : 0,3 ms par fichier + 0,27 s par Mo.

## 4. Les chiffres 4.0 sur la page et dans le README

Le panel du noyau rejoué sur l'index dictionnaire (93 983 fichiers) :
`sched` 9 289 documents en 11 ms, 53 211 spans vérifiés, index 4 938 Mo au
lieu de 18 057 ; navigateur contre natif sur la 2.6.0 : natif 23 s / 905 Mo,
onglet 41 s / 1 089 Mo, mêmes comptes et spans. Face à l'Elasticsearch qui
fait le même travail (trigrammes + `wildcard`, 3 082 Mo) : **×1,6 sur
disque, ×1,08 en RAM**, contre ×5,9 le 28 août.

## 5. Le banc comparatif rejouable (`benches/compare_engines.sh`)

Demande de Lucie : « pas seulement la taille, les faire trébucher là où ils
trébuchent ». Une commande, un rapport (`docs/compare-engines-2026-09-05.md`)
en quatre parties, tout jugé par le même scan des fichiers
([04](04-progression-et-a-faire.md) §2 quinquies) : taille et indexation ;
les neuf requêtes vérifiées ; où les questions diffèrent (relâché 9 552
contre 6 577 / 6 601 ; `spinlokc` d2 10 034 contre 3 549 / 6 557 ; regex
5 510 contre 5 440 / 0 ; `de` 93 009 contre **0 et 0** ; phrase floue 14 449
contre 14 446) ; le prix des positions (15 ms tout, contre 179 ms pour 200
documents chez Elasticsearch, 96 ms pour tantivy).

Deux trouvailles : **la phrase de trigrammes de tantivy rend 0 sur tout**
(positions toutes à 0 dans son tokenizer ; le doc d'août disait « exact »
sur cette base) → le chemin honnête, ET de trigrammes puis vérification sur
le texte stocké, chronométré (107-151 ms, comptes exacts) ; et la phrase
floue `retrun` rendait 0 des deux côtés (une transposition = deux éditions
en Levenshtein, une pour la `fuzziness` d'Elasticsearch) → `retur`. Ce
qu'ils font mieux est écrit : sous-chaîne pure d'ES 3-8 ms contre 12-15,
indexation de tantivy en secondes, leur fuzzy par terme cinq fois plus vite.

## 6. La présentation

[09](09-plan-d-action-presentation.md) : la phrase (« répond à la question
que les autres ne peuvent pas poser, et prouve chaque réponse »), six
piliers avec preuve et phrase interdite. Lucie a ajouté **la transaction**
(store branchable, même commit, rollback — ni Elasticsearch ni tantivy) et
**la fédération** (mêmes scores qu'un index unique, en bibliothèque ;
Elasticsearch est un cluster, tantivy est mono-index, ce qu'il a fallu
réécrire en mars). Puis les docs relus et alignés : **`ARCHITECTURE.md`**
(la page que Google montre en premier) en 4.0.0 — quatre propriétés en
tête, table des fichiers au format 4.0 avec les poids mesurés, dictionnaire,
2.6.0, « one corpus, one truth » ; le README (trois lignes d'accroche,
« What's new in 4.0.0 », deux lignes constatées dans le comparatif) ; les
README Python, Node, C++, WASM et `lucivy_core` (même accroche, options
nommées, versions non publiées) ; la page d'arrivée (sous-titre, trois cartes
de plus, section comparative, machine en 4.0.0). **Reste le titre** (h1) et
le chiffre en tête qui en dépend.

## 7. Le coût du dictionnaire à l'indexation — mesuré, et le chantier

Temps d'indexation **remesurés à neuf** (la référence « ~255 s » du 08 datait
d'avant la compaction en flux) : noyau v3 **56 s**, dictionnaire **131 s**,
dictionnaire + `derived_in_ram` **134 s** (3.0.8 : 122 s). Question de Lucie :
« on est plus lent qu'avant, on peut rien y faire ? ». Sur 30 000 fichiers,
quatre constructions :

| construction | temps |
|---|---|
| v3 | 15,4 s |
| dictionnaire, commit tous les 2 000 (15 générations) | 31,3 s |
| dictionnaire, sans compaction (`LUCIVY_DICT_MAX_GENERATIONS=1000`) | 29,4 s |
| dictionnaire, commit tous les 10 000 (3 générations) | 26,8 s |

La compaction : 2 s. Le nombre de générations : 4-5 s. **Il reste ~11 s
au-dessus du v3** qui ne peuvent venir que du chemin par jeton et de
l'écriture de la génération. Lecture du code : en mode dictionnaire, le
collecteur appelle `lookup_or_mint` pour **chaque jeton distinct de chaque
segment** (`collector_v3.rs:567`) ; `lookup` (`dictionary.rs`) fait une
recherche FST **dans chaque génération**, décode les parents, relit le texte
dans `.termtexts` pour confirmer la casse, alloue les minuscules, et les
textes en attente passent par un `Mutex` global avec `key.to_string()`. Le v3
interne dans une table de hachage locale. Chantier cadré dans
[04](04-progression-et-a-faire.md) §2 sexies : chronométrer d'abord (deux
compteurs : `lookup_or_mint` cumulé par segment, écriture de la génération
par commit), puis un cache de hachage `(texte, forme) → id` par shard devant
les FST, puis recouvrir l'écriture de la génération avec les constructions de
segments du même commit. Cible : le dictionnaire à ×1,3 du v3 au lieu de ×2.

## 9. Le matin du 6 : le repli différé, ×2,0 → ×1,5

Lucie : « on reprend les pistes pour réduire le temps d'indexation ? », avec
la consigne de **surveiller le pic mémoire après chaque correctif** (WASM).

**Chronométré d'abord** (`LUCIVY_VERBOSE`, compteurs dans `lookup_or_mint`
et au commit). Le cadrage de la nuit se renverse : le chemin par jeton
cumule 46 s sur les fils (14,97 M appels, FST 32, verrou 6,7) mais tourne
**en parallèle** du flux ; le mur, c'est le **commit** — le harnais bâtit un
seul shard, et l'écriture de la génération (8,8 s), la compaction (3,4) et la
réouverture (1,4) s'enchaînent en série : ~14 des 15 s d'écart avec le v3.

**Étapes, chacune mesurée (temps, pic RSS, panel), sur 30 000 :**

| étape | 30 000 | note |
|---|---|---|
| référence | 32,2 s (v3 15,3), RSS 6 255 Mo | |
| lecteurs `.termtexts` et vues FST ouverts une fois par champ, minuscules et verrou sans allocation | **29,7 s** | zéro mémoire |
| cache des clés trouvées, éviction au budget | 110 s | quadratique : chaque insertion au-dessus du budget rebalayait la tranche |
| idem, éviction en bloc | 31,0 s | prend 5,7 M de marches FST, rend autant en verrou : **refusé** (32 Mo/shard) |
| moins de générations vivantes (4 / 2) | 36,5 / 55,1 s | la compaction coûte plus que les `get` économisés : refusé |
| FST des textes neufs bâtie par le segment (`.newsfx`), le commit fusionne en flux | 31,6 s | la fusion coûte 1,2 µs la clé, comme bâtir : pas un gain seul |
| passes FST et textes de la fusion en parallèle (natif) | **30,1 s** | commité `5170bcd` |
| **repli différé** : paires nommées, tâche de fond, `meta.json` réécrit après | **23,2-23,6 s** (v3 15,2-15,4), RSS 6 402 | commité `7358112` |
| noyau entier à neuf, commits de 10 000 | **106,8 s** (131 la veille, v3 56), 4 928 Mo, 9/9 ; avec `derived_in_ram` **110,9 s** (134), 3 334 Mo | le temps compte l'attente du dernier repli |

Le repli différé, en une phrase : le commit ne bâtit plus rien, il nomme les
paires de ses segments comme parties provisoires du dictionnaire et une
tâche de fond les fond en génération pendant que les documents suivants
arrivent. Détail et invariants : [11](11-architecture.md) §2.

**La fenêtre, et la règle de Lucie.** Juste après un commit, le dictionnaire
est en 12 à 16 morceaux au lieu de 4 à 8 le temps du repli ; une requête y
rend la même réponse mais marche plus de FST : **3 → 20 ms** sur le panel,
vu une fois quand le harnais a cherché sur des lecteurs rechargés avant le
repli. Lucie : « je veux pas que les gens voient de faux temps de requête ».
Donc, par défaut, **la recherche attend le repli** (une seconde au plus en
natif), la fermeture de l'écrivain et `wait_merges_quiet` attendent l'état
posé, et à la fin d'un repli l'acteur réécrit `meta.json` pour qu'une
réouverture ne voie que des générations ; `dictionary_wait: false` pour qui
préfère la latence. Panel après : 2,6-4,9 ms, inchangé.

**Vérifié** : 9/9 sur chaque construction ; lib 1 461 verts ; dictionnaire,
fédéré, LUCE (le snapshot devait tolérer une paire absente pour un champ),
compat 3.0.8, snapshot servi, dérivés ; nouveau test `deferred_fold_settles`
(recherche juste après un commit, `meta.json` sans paire après fermeture,
aucune paire sur disque, réouverture égale au v3). `test_snapshot_served`
réparé au passage (`list_files_for` à deux arguments).

**WASM, la règle de Lucie : surveiller le pic après chaque correctif.** Build
rebâti, playground, pages fraîches, commits 8 Mo : 2.6.0 42 s mais **2 279 Mo
contre 2 023** la veille ; Godot 36 s, 1 894 contre 1 778 ; noyau 15 440 en
75 s et 1 902 Mo (pas de référence à 8 Mo). Première hypothèse, le repli de
fond qui recouvre les constructions : repli synchrone sur wasm32 → toujours
2 279. Seconde, juste : **les FST par segment bâties en parallèle** dans une
mémoire linéaire qui ne redescend pas. Sans elles sur wasm32 (bâties au
commit, une à la fois, comme la veille) : 2.6.0 **2 023 Mo, 42 s**, Godot
**1 766 Mo, 31 s**. Décision de
Lucie : wasm32 garde le chemin d'avant, le différé est natif.

**Le filtre de Bloom (fin de matinée).** Lucie : « et le gain estimé ? » —
estimé 23 → 19-20 s (les 6,6 M marches pour rien). Fait : sur la clé FST
d'abord, 1,6 M marches sautées seulement (la clé est partagée par toutes
les casses et formes d'un texte) ; sur la clé d'internement, 6,46 M sautées,
FST cumulé 28,4 → 20,6 s, **et le mur natif inchangé** — l'estimation
supposait les collecteurs sur le chemin critique, ils ne le sont pas, et
les 7 M frappés coûtent 2,9 µs chacun. Ce qui a rendu 1,5-2 s, trouvé au
passage : le commit décodait 950 000 textes pour lire leurs ids. Chrome :
2.6.0 40 s (41-42), pic 2 023 égal. Le filtre est gardé (rien en mémoire,
un peu là où les fils manquent), et 30 000 finit à **23,0 s**.

## 8. Commits de la nuit

`4a5967d` `?ram` mesuré · `39c6d9e` douze corpus, deux bugs · `e41bfce`
`c70d176` invite libre · `2c21021` commit par volume · `fe422d3` chiffres
4.0 · `5c83a57` face à Elasticsearch · `262d786` banc comparatif ·
`b80565d` plan de présentation · `ee8d609` docs de présentation · `81ab215`
page d'arrivée · `c706918` temps d'indexation remesurés · puis le trio
10/11/12.
