# Journal — 5 septembre 2026, nuit : la vitrine, le banc, la présentation, et le coût du dictionnaire à l'indexation

Suite de [06](06-journal-session-5-septembre-soir.md) (les postings sans
octets, `derived_in_ram`, la fixture 3.0.8, 4.0.0). Pour repartir : ce
fichier, puis [04](04-progression-et-a-faire.md) (l'état et le todo — §2 ter
`?ram`, §2 bis les corpus, §2 quinquies le banc, **§2 sexies le chantier
suivant**), [09](09-plan-d-action-presentation.md) (sur quoi on se vend), puis
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

## 8. Commits de la nuit

`4a5967d` `?ram` mesuré · `39c6d9e` douze corpus, deux bugs · `e41bfce`
`c70d176` invite libre · `2c21021` commit par volume · `fe422d3` chiffres
4.0 · `5c83a57` face à Elasticsearch · `262d786` banc comparatif ·
`b80565d` plan de présentation · `ee8d609` docs de présentation · `81ab215`
page d'arrivée · `c706918` temps d'indexation remesurés · puis le trio
10/11/12.
