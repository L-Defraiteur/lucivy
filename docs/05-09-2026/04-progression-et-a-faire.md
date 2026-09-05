# Progression et à faire — après la compaction en flux

Ce fichier suit le programme décidé le 5 septembre après la compaction du
dictionnaire ([01](01-journal-session-5-septembre.md) §13) : ce qu'on
vend, c'est **polyvalent et exact** — tout, et bien. Les étapes, dans
l'ordre, et leur état ; à tenir à jour au fil de la session.

## 0. Où on en est (taille sur disque, mesurée)

Noyau entier, 93 605 fichiers, 857 Mo de texte :

| Index | segments | taille | ×texte |
|---|---|---|---|
| `main` 3.0.x non compacté (`/tmp/lucivy-idx-90k`) | 1 504 | 18 057 Mo | ×21 |
| `main` 3.0.x compacté (`lucivy_bench_sharding/single`) | 10 | 11 025 Mo | ×12,9 |
| v4, un `.sfx` par segment (`idx90k-v8`) | 253 | 7 422 Mo | ×8,7 |
| v4, dictionnaire par shard (`idx90k-dict2`) | 253 | 5 706 Mo | ×6,7 |
| v4, dictionnaire, postings sans octets (`idx90k-dict-sfp5`, 5 septembre au soir) | 253 | **4 938 Mo** | **×5,8** |
| … et `derived_in_ram` (`idx90k-dict-ram`, option) | 253 | **3 344 Mo** | **×3,9** |
| *Elasticsearch 8.19, trigrammes + `wildcard` (la config qui répond à une sous-chaîne ; `28-08-2026/06` §3)* | | *3 084 Mo* | *×3,6* |
| *Elasticsearch standard (pas de sous-chaîne)* | | *759 Mo* | *×0,9* |

**Où on en est face à Elasticsearch** (question de Lucie, tard le 5) : la
config d'Elasticsearch qui fait le même travail pèse 3 084 Mo ; lucivy 4.0
est à **×1,6** avec les fichiers écrits et **×1,08** avec `derived_in_ram`
(×5,9 le 28 août). À ce prix, la comparaison du 28 août rappelle ce qu'il ne
rend pas : les séparateurs relâchés (3 549 documents contre 10 034 exacts),
la regex à 70 documents près, la phrase floue partielle. Jamais mesurés côte
à côte : le temps de requête et la RAM au repos d'Elasticsearch (la JVM).

Répartition des 5,7 Go : `.sfxpost` 22 %, `.word_sfxpost` 18 %, `dict.sfx`
16 %, `.word_pos_map` 11 %, `.posmap` 8 %, `.sibling_v3` 8 %, `.store`
6 %, `dict.termtexts` 5 %, `.gmap` 5 %. **Les postings font 40 %**, les
cartes de positions et la fratrie 27 %, le dictionnaire 21 %.

10 000 fichiers : 1 152 Mo (`main`, 320 segments) → 372 Mo ; 30 000 :
3 400 → 1 266 Mo.

## 1. Le playground marche encore avec tout ça — validé le 5 septembre, **WASM : fait**

Bilan : build sans changement du binding, mêmes comptes que v3 (démo,
15 440 fichiers du noyau, panel de 21 requêtes), deux compactions en flux
passées dans WASM, réglage retenu 8 threads de scheduler / 1 thread
d'indexation / 2 fusions pour un index à dictionnaire, requêtes du même
ordre ou plus rapides qu'en v3 (§1 ter). Ce qui reste pour le navigateur
est noté, pas bloquant : le plancher de 1,5 Go de l'indexation, les seuils
calibrés sur les gros index d'avant, Memory64 un jour.


- [x] Build WASM (`bash bindings/emscripten/build.sh`, emsdk 6.0.8, nightly
  `-Z build-std`) : passe sans changement du binding ; `pkg/` et
  `playground/pkg/` recopiés, `index.html` estampillé `3.0.8-5a8f6b0`.
- [x] `?dict` sur le playground : l'index est créé avec
  `shared_dictionary: true` (option déjà lue par `lucivy_create` via
  `SchemaConfig`).
- [x] Lancé (`node playground/serve.mjs`, port 9877). La démo (la source
  de lucivy, 1 171 fichiers, 4 shards) en v3 et en `?dict`, dans deux
  onglets : **mêmes comptes** sur les 8 requêtes de la démo (5, 100+, 2,
  5, 100+, 100+, 41, 9 sur 12), mêmes premiers résultats, mêmes scores ;
  puis trois requêtes tapées dans les deux : `--strict "spin_lock_init"`
  17 / 17, `--fuzzy 1 "compation"` 12 / 12, `--regex "SfxFileReaderV[0-9]"`
  21 / 21. Console vide d'erreurs. Temps du même ordre (démo v3 8,7 à
  46 ms, dictionnaire 8,7 à 62 ms).
- [x] Sur ce petit corpus le dictionnaire **ne gagne rien en mémoire** :
  126 Mo contre 118 (4 shards, un dictionnaire par shard, peu de
  répétition entre segments) — le gain vient de la répétition entre
  segments, qui n'existe qu'à partir de quelques milliers de fichiers.
- [x] `?corpus=corpus-kernel-16k.tar.gz&dict` : **15 440 fichiers du noyau
  indexés dans le navigateur en mode dictionnaire**, 4 shards, commit
  tous les 2 000 ; index 1 782 Mo en mémoire (2 117 fichiers relus en
  3,8 s) ; `spin_lock_init` 20 résultats en 51 ms, highlights justes.
- [x] Piège de test, pas du moteur : **deux onglets qui indexent en même
  temps** échouent tous les deux au premier commit (`I/O error`, code 29,
  sur un `.term` ou un `.idx`) — ils écrivent le même répertoire OPFS
  `user_index`, dont les poignées d'accès sont exclusives. Un onglet à la
  fois. À noter dans la doc du playground.
- [x] `?corpus=corpus-kernel-16k.tar.gz&dict&commit=1000` (nouveau
  paramètre `?commit=N`) : 16 commits, tous « done OK » en 2 à 4 s
  chacun, dont ceux qui ont compacté (16 générations → **deux compactions
  en flux dans WASM**, fichiers temporaires par le répertoire à écriture
  différée, relus après `terminate()`) ; `mutex_lock` relâché, limite
  2 000 : **1 547 résultats**, recherche 67 ms côté moteur.
- [x] Même corpus, même `commit=1000`, sans `dict` : `mutex_lock`
  **1 547 résultats, la même charge utile à l'octet près** (52 738 522
  octets de résultats et de highlights des deux côtés). En mémoire :
  v3 1 996 Mo, dictionnaire 1 775 Mo (−11 % sur 15 440 fichiers, 4
  shards).
- **Tranché** (onglet neuf, WASM avec le journal séparé) : `[preload]
  waited for merges: 0 rounds, 70201ms` puis `1719 files, 1768 MB in
  4114ms`. Le « loading into memory… » lent, c'est **l'attente des fusions
  de fond** que `preload` impose avant de lire (une fusion bâtit sa FST
  dans le même espace d'adresses), pas la lecture OPFS (4,1 s) ni le
  choix RAM / flux (1,8 Go < 3 Go, l'index est tenu entier). Avec
  `commit=1000` il y a deux fois plus de petits segments à fusionner
  après le dernier commit, et la fusion en WASM est à concurrence 1 : 70 s.
  Le message du playground devrait le dire (« merging… » plutôt que
  « loading into memory… »), à faire.
- Observation d'origine : le `preload` de cet index avait pris **81,5 s**
  (1 694 fichiers, 1 769 Mo) contre 3,8 s pour l'index de la même taille
  bâti avec un commit tous les 2 000 (2 117 fichiers) ; et **95,8 s en
  v3** avec `commit=1000` (1 680 fichiers, 1 957 Mo). Donc lié au
  `commit=1000` (ou à la succession d'index de 1,8 Go dans le même
  onglet), pas au dictionnaire. Soit l'OPFS était encore occupé par les
  fusions de fond, soit la RAM de l'onglet approchait la borne — à
  reproduire dans un onglet neuf avant de conclure.

### 1 bis. Les fusions à concurrence 1 en WASM — à remesurer

Pourquoi 1 (`src/indexer/merge_permits.rs`, 24 août) : une fusion v3
tient en RAM tous les sidecars sources, les tables fusionnées et la table
de clés du builder de FST (~500 Mo pour 14 segments de 40 fichiers) ;
quatre à la fois dans les 4 Go du navigateur ont tué le premier commit du
playground sur un `realloc` de 192 Mo. Ce qui a changé depuis : **une
fusion en mode dictionnaire ne rebâtit aucune FST** (union des `.gmap`,
remappage, concaténation des postings), et les index ont fondu de 40 %.
Outillage ajouté le 5 septembre : `--merge-concurrency=N` (option
`mergeConcurrency`, `?merges=N` dans le playground), et `memoryStatus`
rend `heap_bytes`, la taille de la mémoire linéaire WASM — elle ne fait
que croître, c'est le **pic** de tout ce que le moteur a tenu en même
temps ; le playground le journalise après l'indexation. Protocole :
`?corpus=corpus-kernel-16k.tar.gz&dict&commit=1000` sans puis avec
`&merges=2`, comparer le pic mémoire et l'attente des fusions.

| 15 440 fichiers du noyau, dictionnaire, `commit=1000`, 4 shards | indexation | attente des fusions | lecture | pic mémoire WASM |
|---|---|---|---|---|
| fusions à 1 (défaut) | 60 s | 73,6 s | 5,0 s | 2 543 Mo (index 1 775 Mo) |
| fusions à 2 | 82 s (les fusions chevauchent l'indexation) | 3,8 s | 4,3 s | 2 539 Mo (index 1 772 Mo) |
| fusions à 4 | 62 s | 15,7 s | 4,4 s | 2 539 Mo (index 1 774 Mo) |
| fusions à 4, scheduler 12 threads (8 par défaut) | 65 s | 8,7 s | 4,8 s | 2 539 Mo |
| fusions à 4, 4 threads d'indexation (1 par défaut) | **140 s** | 10,0 s | 9,2 s (3 844 fichiers) | 2 283 Mo |

Lecture : à 2, deux fusions tournent ensemble (22 à 28 s chacune, comme
seules), **le pic mémoire ne bouge pas** (2 539 contre 2 543 Mo : une
fusion en mode dictionnaire ne pèse pas assez pour se voir à côté de
l'index et des indexeurs), et l'index est servable **86 s** après le début
au lieu de 134 s. À 4, les quatre fusions tournent ensemble (24 à 30 s
chacune), même pic à l'octet près, servable en **82 s**. La raison du
« 1 » (une fusion v3 rebâtit la FST, quatre à la fois tuaient le
navigateur) ne s'applique pas au mode dictionnaire, au moins à cette
taille. Décision du 5 septembre : **2 par défaut dans le navigateur pour
un index à dictionnaire partagé**, 1 pour un index v3 (non remesuré),
`mergeConcurrency` / `--merge-concurrency=N` pour forcer.

Plus de threads de scheduler (12 au lieu des 8 par défaut sur cette
machine, `available_parallelism` borné à 2..8) : **rien** — chaque fusion
est même plus lente (30 à 41 s contre 24 à 30), le total est le même. Le
navigateur est borné par la mémoire et l'allocateur, pas par le nombre de
threads ; c'était déjà la conclusion du 25 août pour les requêtes. Note :
le pic de mémoire WASM est de **1 650 Mo pour la démo de 1 171 fichiers**
(index de 126 Mo) — le plancher du moteur en indexation (tas de l'écrivain,
arènes, pool) est déjà de 1,5 Go avant tout index.

Quatre threads d'indexation : **deux fois plus lent** (140 s). Chaque
thread écrit ses propres segments, donc quatre fois plus de segments
(3 844 fichiers au lieu de 1 719), 26 fusions de 8 à 14 segments au lieu
de 4, des attentes de créneau jusqu'à 112 s. Le « 1 » des threads
d'indexation en WASM reste le bon réglage. **Optimum fonctionnel retenu
et marqué fait** : scheduler 8 (défaut), 1 thread d'indexation, 2
fusions pour un index à dictionnaire ; 4 fusions n'apporte que quelques
secondes de plus et n'a pas été retenu par prudence.

Trace par fusion (`[merge] N segments: waited … for a slot, ran …`,
`LUCIVY_VERBOSE`, ajoutée le 5 septembre) à concurrence 1 : **quatre
fusions, une par shard, de 8 à 9 segments, 24 à 27 s chacune, strictement
en file** (attente du créneau 0, 20, 46, 71 s) — les 73 s sont 4 × 25 s
sérialisées par le permis, pas une fusion géante. Le premier essai à
« 2 » n'avait rien changé parce que la couche JS (`lucivy.js`) filtre
les options d'initialisation et ne transmettait pas `mergeConcurrency` :
corrigé.

### 1 ter. Les requêtes dans le navigateur ne s'envolent pas

Le panel de parité du playground (`parity_panel.json`, 21 requêtes, lancé
par `parity_run.js` via le serveur de debug, `limit` 100 000, highlights)
sur l'index dictionnaire des 15 440 fichiers (4 shards, `commit=1000`) :
passe froide (index rouvert par `?open=user_index`) puis chaude.

Puis la même chose sur un index **v3** du même corpus, même forme (1 958 Mo
en mémoire contre 1 768). Temps en ms, comptes identiques sauf la ligne
marquée.

| requête | docs | dict froid | v3 froid | dict chaud | v3 chaud |
|---|---|---|---|---|---|
| contains strict kmalloc | 1 216 | 85 | 111 | 34 | 43 |
| contains relaxed kmalloc | 1 217 | 29 | 32 | 26 | 35 |
| contains strict spin_lock_init | 1 112 | 49 | 45 | 27 | 41 |
| contains relaxed spin_lock_init | 1 120 | 34 | 44 | 26 | 38 |
| contains strict ->next | 859 | 39 | 37 | 33 | 30 |
| contains strict return -ENOMEM; | 4 268 | 104 | 109 | 77 | 84 |
| split spin lock init | 11 901 | 366 | 375 | 305 | 316 |
| startsWith netdev | 2 769 | 100 | 112 | 97 | 89 |
| term kfree | 3 833 | 78 | 75 | 79 | 78 |
| phrase return -ENOMEM | 4 272 | 81 | 83 | 87 | 80 |
| fuzzy d1 kmallc | 1 340 | 53 | 72 | 55 | 70 |
| fuzzy d1 spin_lock_ini | 1 214 | 60 | 67 | 46 | 77 |
| fuzzy d2 kmalloc (**tronquée**, voir ci-dessous) | 7 320 / 7 317 | 387 | 367 | 384 | 387 |
| regex spin_lock_[a-z]+ | 1 875 | 206 | 185 | 207 | 186 |
| regex ETH_P_[0-9A-Z]+ | 580 | 73 | 71 | 72 | 71 |
| parse mutex unlock | 4 964 | 121 | 117 | 114 | 109 |
| parse kmalloc AND NOT kfree | 72 | 33 | 24 | 21 | 26 |
| parse "spin_lock_init" -kfree | 267 | 22 | 28 | 24 | 29 |
| ext filter netdev .h | 636 | 60 | 67 | 38 | 42 |
| path contains ethernet/intel | 382 | 95 | 65 | 96 | 55 |
| no hit | 0 | 37 | 10 | 7 | 9 |

**Verdict** : dans le navigateur le dictionnaire est **du même ordre ou
plus rapide** que v3 sur 19 requêtes sur 21 (les `contains` et les fuzzy
d1 gagnent 10 à 30 %), plus lent sur deux : la regex `spin_lock_[a-z]+`
(×1,1) et `path contains ethernet/intel` (×1,5 — le champ `path`, un
dictionnaire minuscule : les 4 shards ont chacun le leur, et le plan par
shard ne rattrape pas grand-chose sur 2 700 clés). Rien ne s'envole ; la
règle du ×1,5 tient à la limite sur `path`.

**`fuzzy d2 kmalloc` : 7 320 en dictionnaire, 7 317 en v3, et 7 321 sur
une passe faite juste après l'indexation.** Ce n'est pas un rappel
différent : `memoryStatus().last_search_truncated` est **vrai** sur cette
requête en v3 — le plafond `LUCIVY_MAX_MATCHES_PER_SEGMENT` (20 000 sur
wasm) est atteint, la recherche le dit, et le nombre de documents perdus
dépend de la taille des segments, donc de la forme de l'index (v3 et
dictionnaire ne fusionnent pas dans le même ordre, et les fusions de fond
changent la forme après coup). En natif, les deux modes rendent le compte
exact du panel de vérité. Côté dictionnaire le drapeau est **vrai aussi**
(7 321 juste après la construction, index de 1 857 Mo) : les deux modes
sont tronqués sur cette requête et le disent, et l'écart de 3 ou 4
documents est l'endroit où le plafond tombe selon la forme des segments.

### 1 quater. Les « explosions » de temps dans le playground : `memoryStatus`

Observé par Lucie : la même requête tapée deux fois passe de 60 à 400 ms
une fois sur deux. Le moteur, lui, est stable — son journal donne 16 à
44 ms sur toutes ces requêtes, et douze rejeux enchaînés par le serveur
de debug font 18 à 30 ms de bout en bout. En simulant la frappe dans la
page (événement `input`, rendu, lecture de l'en-tête), la page affiche
27 à 40 ms… et 75 à 81 ms une fois sur trois alors que le moteur dit
18 à 21 ms. Cause trouvée : après chaque recherche la page appelle
`memoryStatus()` pour afficher le drapeau de troncature, et cet appel
prenait **0,8 à 1,3 s** — `lucivy_memory_status` recomptait les octets de
chaque shard en **ouvrant chacun des 1 700 fichiers sur OPFS**, à chaque
appel, sans passer par le cache que `residency()` utilise. Le worker
traite les messages en file : la recherche de la frappe suivante attendait
derrière ce comptage, d'où les pics, aléatoires selon l'instant de la
frappe. Corrigé le 5 septembre : `shard_bytes_and_files_cached` (mémo par
liste de segments, comme `residency`), `memory_status` l'utilise. **Vérifié** après reconstruction du WASM :
`memoryStatus()` passe de 800-1 300 ms à **6-9 ms**, et douze frappes
simulées affichent 27 à 36 ms pour 25 à 35 ms côté moteur — la page et le
moteur disent la même chose, à la milliseconde. La suite `lucivy-core`
verte.

## 2. Réfléchir : perdre encore du poids, ou reconstruire en RAM à l'ouverture

Ce que les formats disent (lu le 5 septembre, à mesurer avant de choisir) :

**Trois fichiers sur neuf sont des dérivés.** `.posmap` (position → ordinal,
3 octets par position) est l'inverse exact de `.sfxpost` (ordinal → documents,
positions, octets) ; `.word_pos_map` (position → mot qui y commence, 4 octets)
est l'inverse exact de `.word_sfxpost`, « bâti des mêmes entrées » ;
`.sibling_v3` (ordinal → ordinaux suivants) se déduit de `.posmap`
(positions consécutives dans chaque document). Ensemble : **27 % de l'index
du noyau (1,55 Go)**, 23 % sur 30 000 fichiers. Aucun ne porte
d'information que les postings n'ont pas.

**Ce qu'une reconstruction à l'ouverture coûterait.** Elle lit tous les
postings du segment (1,2 + 1,0 Go sur le noyau) — des secondes, pas des
millisecondes — et le résultat est **résident** : un fichier mappé ne coûte
de la RAM que là où une requête le touche, une structure rebâtie coûte sa
taille entière tout le temps. Sur disque le gain est réel (−27 %) ; en RAM
c'est l'inverse. Donc : **optionnel, jamais le défaut natif**, décidé à
l'ouverture (`rebuild_derived: true`), et par segment à la première
requête qui en a besoin plutôt qu'à l'ouverture (l'ouverture reste
instantanée, le premier `regex` d'un segment paie). En WASM tout est en
mémoire de toute façon : là le gain est le téléchargement et l'OPFS (un
snapshot LUCE 27 % plus petit), le pic de RAM inchangé.

**Ce qui pourrait maigrir sans rien dériver**, dans l'ordre des parts :

- `.sfxpost` + `.word_sfxpost` (40 %) : déjà deltas + varints (SFP3/WSP3),
  points de contrôle tous les 32. À regarder : `byte_from`/`byte_to` par
  occurrence sont dérivables de la position et du `.termtexts` (la
  longueur du texte est connue par ordinal) — un seul décalage d'octets par
  document suffirait si les positions et les longueurs redonnent les
  spans ; les entrées mot répètent `first`/`last`/`from`/`to`.
- `dict.sfx` (16 %) : la FST 104 Mo et la table des parents 800 Mo sur le
  noyau. Les parents sont ce qu'il reste après le conteneur 8 ; les
  records groupés par overlap sont déjà au plus court. Peu à gagner sans
  changer le modèle.
- `.store` (6 %) : les documents compressés ; c'est ce qu'on rend.
- `dict.termtexts` (5 %), `.gmap` (5 %) : petits.

**Ordre proposé** : (1) mesurer sur le noyau ce que `.sfxpost` +
`.word_sfxpost` deviennent sans `byte_from`/`byte_to` explicites (un
script sur les entrées décodées, sans rien changer au moteur) ; (2) si
c'est ≥ 10 % de l'index, le faire ; (3) ensuite seulement la
reconstruction optionnelle des trois dérivés, en commençant par
`.sibling_v3`, le plus simple et le plus lu.

**(1) mesuré le 5 septembre** (`suffix_fst/postings_measure.rs`, test
ignoré `postings_without_byte_spans`, `LUCIVY_POSTINGS_DIR=<index>`) :
chaque fichier est décodé puis ré-encodé par l'écrivain d'aujourd'hui
(même taille au Mo près : la mesure est exacte au format), puis ré-encodé
avec `byte_from`/`byte_to` à zéro, moins l'octet que chaque zéro coûte
encore.

| 30 000 fichiers, dictionnaire (1 266 Mo) | entrées | aujourd'hui | sans spans | gain |
|---|---|---|---|---|
| `.sfxpost` (240 fichiers) | 31,0 M | 243,5 Mo, 8,23 o/entrée | 158,5 Mo, 5,36 o/entrée | −34,9 % |
| `.word_sfxpost` (240 fichiers) | 23,9 M | 183,1 Mo, 8,03 o/entrée | 117,2 Mo, 5,14 o/entrée | −36,0 % |

| noyau entier, dictionnaire (5 706 Mo) | entrées | aujourd'hui | sans spans | gain |
|---|---|---|---|---|
| `.sfxpost` (506 fichiers) | 167,0 M | 1 236,9 Mo, 7,77 o/entrée | 773,3 Mo, 4,86 o/entrée | −37,5 % |
| `.word_sfxpost` (506 fichiers) | 136,7 M | 1 017,9 Mo, 7,80 o/entrée | 639,2 Mo, 4,90 o/entrée | −37,2 % |

**Les spans d'octets pèsent 150,9 Mo sur 30 000 (35 % des postings, 12 %
de l'index) et 842 Mo sur le noyau (37 % des postings, 15 % de
l'index : 5,7 → 4,9 Go).** Au-dessus du seuil des 10 %, donc le chantier vaut le coup —
à condition de savoir les redériver au prix d'une requête qui reste sous
×1,5. Ce qu'il faudrait pour ça :

- `byte_from` d'une position `p` d'un document = la somme des `own_len`
  des ordinaux aux positions `0..p` : `.posmap` donne l'ordinal à chaque
  position, `.termtexts` (le dictionnaire) donne `own_len`. Un préfixe
  cumulé par document, c'est O(p) — trop cher à la requête. Avec un
  **point de contrôle tous les K positions** (l'offset d'octet, 4 octets
  toutes les 16 positions = 0,25 o/position contre les ~2,9 o/entrée
  qu'on retire), une résolution coûte au plus K lectures de `.posmap` +
  K métas.
- `byte_to` d'un chunk = `byte_from` + la longueur de contenu de l'ordinal
  (méta). Pour un mot, le posting porte la longueur du mot parce qu'« une
  clé 0x02 ne fixe pas la longueur de son mot » (doc de `word_sfxpost`) :
  à vérifier si `own_len − sep_len` de l'ordinal mot la donne quand même,
  sinon garder `to − from` (un varint, ~1 o) et ne retirer que `from`.
- Qui lit les spans, et quand : les highlights (à la fin, sur le top-k
  seulement — bon marché), la validation regex (fenêtres reconstruites
  sur les octets : sur le chemin chaud, c'est là que le ×1,5 se joue), la
  fuzzy (vérification sur le texte). À profiler avant de coder.
- Le collecteur dit déjà la moitié : pour un chunk, `byte_to = byte_from
  + content_len + sep_len` = `byte_from + own_len` de l'ordinal
  (`collector_v3.rs`, `add_value`) — dérivable de la méta. Pour un mot,
  `byte_to` est la fin du **contenu** du mot, et le commentaire du
  collecteur dit qu'« `init` est `in`+`it` dans un document et le mot
  `init` dans un autre sous un même ordinal » : la méta de l'entrée FST
  ne suffit pas, mais `.termtexts` porte le texte original de l'ordinal
  mot, donc sa longueur de contenu — à **vérifier sur le noyau** entrée
  par entrée avant de coder (le même test ignoré, augmenté : `to − from`
  contre la méta, et `byte_from` contre la somme cumulée des `own_len`
  par `.posmap`).

**Dérivabilité vérifiée sur les 30 000** (`byte_spans_are_derivable`,
240 segments, 60 000 documents, 31,0 M positions) : `byte_from` d'un chunk
= somme cumulée des `own_len` par `.posmap`, **0 désaccord sur 31,0 M** ;
`byte_to − byte_from` d'un chunk = `own_len`, **0 sur 31,0 M** ; pas une
position vide, pas un document dont le premier chunk ne part pas de
l'octet 0 ; pour les mots, `byte_to − byte_from` = `own_len − sep_len` de
l'ordinal mot, **0 sur 23,9 M** ; `byte_from` d'un mot = somme cumulée à
`first_position`, **3 désaccords sur 23,9 M** — et **les mêmes 3 sur les
136,7 M du noyau** (0 désaccord partout ailleurs, sur 167 M chunks et
137 M mots). Les trois sont des documents chinois de la documentation du
noyau, et le même motif : un mot dont le contenu est `解。` ou `免。`
(idéogramme + point pleine chasse, 6 octets) suivi d'une **suite de
séparateurs plus longue que ce que son chunk peut tenir** (`解。\n\n` fait
8 octets, les `..` ou `\t\t` suivants débordent dans le chunk d'après,
`.. to`, `\n\t\tTh`). Le posting du mot dit `from 716, to 722` — les
octets de `解。`, justes — mais `first_position = last_position = 102`,
le chunk du débordement, alors que `解。` est le chunk 101 (octet 716).
**Le mot est à la bonne place en octets et à la mauvaise en position.**
Diagnostic (reproduit dans `collector_v3.rs`,
`word_position_when_separators_spill_into_the_next_chunk`) : ces « mots »
sont des **lignes chinoises entières** — pas un séparateur dans une ligne
d'idéogrammes, donc un mot de plusieurs centaines d'octets — et au-delà de
264 octets le collecteur écrit une **entrée de queue** (les 8 derniers
octets du mot, pour que les requêtes près de la fin du mot le trouvent).
La position de cette queue était **le dernier chunk du mot**, ce qui est
juste tant que ce chunk contient les derniers octets ; quand les
séparateurs de fin débordent dans un chunk à eux, le dernier chunk n'a
que des séparateurs et la queue pointait à côté. Corrigé le 5 septembre :
`first_position` = le chunk qui contient `byte_from`, `last_position` = le
dernier chunk du mot (comme pour le mot lui-même, pour l'adjacence).
Trois occurrences sur 137 millions ; la vérité terrain ne l'avait jamais
vu parce qu'aucune requête du panel ne touche ces documents, et le
`word_pos_map` disait « un mot commence en 102 » sur un chunk de
séparateurs. **Vérifié** : index 30 000 reconstruit avec le correctif
(`idx30k-dict3`, panel 9/9), `byte_spans_are_derivable` dessus : **0
désaccord partout** — 31,0 M chunks (`from` et `to`), 23,9 M mots (`from`
et `to`). La suite lib (1 456) verte. Conclusion pour le chantier : **les
spans d'octets des postings sont entièrement dérivables** de la position
(somme cumulée des `own_len` par `.posmap`) et de la méta de l'ordinal ;
seule réserve, les entrées de queue des mots de plus de 264 octets, dont
le `from` tombe *dans* un chunk (pas à son début) : il faudrait leur
garder un décalage, ou le recalculer depuis la longueur du mot (le mot
principal, lui, part au début de son premier chunk).

**Qui lit les spans, mesuré** (5 septembre après-midi, panel de vérité sur les
30 000 fichiers en dictionnaire, `V3_PROFILE=1`, sommes sur les 120
segments) : ce que les résolveurs matérialisent contre ce qui sort.

| requête | ms | spans matérialisés (matches émis / hits) | highlights rendus | lookups de postings pour les fenêtres |
|---|---|---|---|---|
| mutex_lock strict | 2,9 | 1 506 | 753 | — |
| spin_lock strict | 2,5 | 4 551 | 2 278 | — |
| sched term | 5,2 | 15 189 | 4 273 | — |
| sched strict | 2,9 | 8 264 | 8 264 | — |
| printk sw | 3,6 | 11 285 | 5 470 | — |
| schdule fz1 | 9,6 | 44 246 hits | 2 284 | 37 678 (2 par fenêtre, 18 839 fenêtres) |
| regsiter fz2 | 160,3 | **2 571 045 hits** | 42 582 | **1 427 958** (713 979 fenêtres) |
| spin_lock_[a-z]+ rx | 18,1 | 1 486 | 1 486 | 878 |

Lecture du code (`resolve.rs`, `composite.rs`, `regex_verified.rs`,
`orchestrator.rs`) : sur le chemin v3 **aucune décision d'adjacence ne lit
un octet** — les chaînes se vérifient par positions (`.posmap`,
`.word_pos_map`). Les octets servent à : la clé de dédoublonnage
`(doc, position, byte_from)` (équivalente à `(doc, position, sti)`) ; le
filtre `exact_match` de secours sans posmap (`token_end`) ; le
regroupement des hits fuzzy en régions (tri et écart **en octets**) et
des hits regex (écart `2n + 2` octets) ; l'ancrage des fenêtres
reconstruites (`rebuild_window_opts` : **deux** `resolve_doc_at` par
fenêtre, la base et un contrôle, puis les offsets sont déjà **dérivés**
par `own_len` de position en position — le code fait aujourd'hui, à
l'échelle d'une fenêtre, exactement ce que le chantier généralise) ; le
placement du débordement d'overlap ; et les highlights, un triplet par
match, jusqu'au plafond puis réparés sur le top-k. Le fuzzy à 2 éditions
est le cas dimensionnant : 2,57 M de hits dont 1,7 % finissent en
highlight, et 1,43 M lectures de postings dont la moitié ne sert qu'à
contrôler la dérivation (`derive_miss=0` partout).

**Plan de réalisation (décidé le 5 septembre, après-midi)** :

1. **`.posmap` layout 4 (`PMP4`)** : après les cases PMP3 de chaque
   document, un point de contrôle `u32` = offset d'octet **toutes les 16
   positions** (0,25 o/position ; 7,75 Mo sur les 31,0 M positions des
   30 000, contre 151 Mo retirés). `byte_at(doc, p)` = le point de
   contrôle ≤ p, plus les `own_len` (méta) des positions intermédiaires ;
   une **case vide remet l'offset à 0** (frontière de valeur : le
   collecteur repart à l'octet 0 à chaque valeur, une position de
   séparation entre deux). Le collecteur et les fusions écrivent PMP4
   (ils connaissent `own_len`) ; PMP3 et PMAP toujours lus.
2. **Un service d'offsets sur le contexte** (`BriquesContext::offsets`) :
   `byte_at(doc, p) -> Option<u32>`, deux dos — PMP4 par dérivation ;
   ancien index (PMP3 + postings avec octets) par `posmap.ordinal_at` puis
   `resolve_doc_at(...).byte_from`. **Plus aucune brique ne lit
   `e.byte_from`** : `rebuild_window_opts` (une dérivation au lieu de deux
   lookups), `place_overlap_overflow`, les émissions de `resolve.rs`.
   Étape mesurable seule, format compatible (A/B sur `regsiter` fz2, poste
   « window »).
3. **Postings sans octets** : `SFP5` (une entrée = `d_position`, l'en-tête
   par document inchangé) et `WSP5` (`d_doc`, `d_first`,
   `(last − first) << 1 | a_un_décalage`, `[décalage]` — le décalage dans
   le chunk, présent seulement pour les **entrées de queue** des mots de
   plus de 264 octets, qui partent au milieu d'un chunk ; 0 octet pour
   les autres). Longueurs par la méta : chunk `own_len`, mot
   `own_len − sep_len` (0 désaccord sur 137 M). `PostingResolver` rend des
   positions ; `MatchV3` garde `(doc, position, span, sti, ordinaux,
   consommé)` et ses octets sont **placés** en une passe triée par
   document juste avant `verify_literal` et les highlights ; les hits
   fuzzy et regex se regroupent **par positions** (une position ≥ 1
   octet, pas de position vide : le même seuil numérique regroupe un
   sur-ensemble, la vérification sur fenêtre reste ce qui décide). Les
   lecteurs SFP2-4 / WSP2-4 restent lus ; le pipeline v2 (`sfx_version`
   2) continue d'écrire SFP4 avec ses octets, ses requêtes ne changent
   pas.
4. **Fusions** : `merge_segments_v3`, `merge_segments_dict`, `sfx_merge`
   (v2) — plus d'octets à remapper, le `.posmap` rebâti avec `own_len`.
5. **Vérité** : référence 10 000 (`9 pass`), réouverture des index
   d'avant (`idx30k-dict3`, `idx30k-v7` : les anciens layouts par le
   second dos du service), 30 000 A/B de temps (la fuzzy et la regex
   surtout), noyau entier ; `byte_spans_are_derivable` devient le test du
   service ; scan des tailles ; puis les docs.

Ce que ça retire, attendu : 842 Mo sur le noyau (15 %), 151 Mo sur les
30 000 (12 %), moins les points de contrôle (~1,8 % de ce qu'on retire).

**Réalisé le 5 septembre, après-midi** (commits `556262a` puis la suite) :

- [x] Étape 1, `PMP4` + `byte_at` : validée seule (référence 10 000 v3 et
  dictionnaire 9/9, index d'hier rouvert 9/9, 30 000 neuf 9/9) ; fenêtres
  du fuzzy à 2 éditions 1 738 → 1 560 ms de somme sur les segments, posmap
  +8 Mo sur les 30 000.
- [x] Étapes 2 à 4, `SFP5` / `WSP5`, résolveurs en positions, `place_spans`,
  fusions (v3 et dictionnaire, conversion des queues d'un segment ancien par
  son `.posmap` et ses postings à spans), pipeline v2 inchangé (`SFP4`).
  **Un bug trouvé par la vérité, pas par les tests unitaires** : pour un
  dernier jeton qui est un mot, son texte commence à son **premier** chunk,
  pas à la dernière position du span (celle de l'adjacence) — `mutex lock`
  relâché rendait `[5441..5456]` pour `[5441..5451]`. `MatchV3` porte
  depuis `last_start_pos`. Et le jeu des séparateurs du regroupement fuzzy
  (`MAX_SEPARATOR_SLACK`) était en octets : 32 positions regroupaient
  quatre fois plus de texte (714 000 → 182 000 régions, DP 515 → 903 ms) ;
  ramené à 5 positions, ce que 32 octets de séparateurs peuvent occuper.
- [x] Vérité : suite lib 1 460 verts ; `test_sfx_v3_pipeline` 40/40,
  dictionnaire, fédéré, filtré, LUCE, fuzzy et regex de vérité verts ;
  référence 10 000 v3 **et** dictionnaire 9/9 ; **les anciens index
  rouverts 9/9** — `idx30k-dict3` (PMP3 + SFP4), `idx30k-dict4` (PMP4 +
  SFP4), `idx30k-v7` (v3) — par le second dos de `byte_at` et
  `word_tail_off` ; 30 000 dictionnaire et v3 neufs 9/9 ; noyau entier
  9/9 ; `byte_spans_are_derivable` (le `byte_at` du `PMP4` contre une
  somme cumulée indépendante, chaque posting de mot contre les chunks
  sous lui) : **0 désaccord** sur les 8,1 M positions des 10 000, les
  31,0 M des 30 000 (v3 et dictionnaire) et les 167,0 M positions /
  136,7 M mots du noyau.

| index (fichiers SFX, `scan_index_size.py`, tantivy exclu) | avant | après | postings avant → après |
|---|---|---|---|
| 10 000, v3 | 460,2 Mo | **420,9 Mo** (−8,5 %) | `.sfxpost` 68,6 → 46,2 ; `.word_sfxpost` 45,3 → 28,4 |
| 10 000, dictionnaire | 331,3 Mo | **290,8 Mo** (−12,2 %) | 70,1 → 47,4 ; 48,6 → 30,6 |
| 30 000, v3 | 1 496,1 Mo | **1 352,7 Mo** (−9,6 %) | 239,2 → 154,3 ; 173,1 → 106,7 |
| 30 000, dictionnaire | 1 131,7 Mo | **977,9 Mo** (−13,6 %) | 243,5 → 158,0 ; 183,2 → 114,9 |
| noyau entier, dictionnaire | 5 077 Mo (`idx90k-dict2`) | **4 259,1 Mo** | 1 236,9 → 771,6 ; 1 017,9 → 626,8 |

Sur disque tout compris (`du`, store tantivy inclus) : **noyau entier
5 717 → 4 938 Mo (−13,6 %, ×5,8 le texte ; `main` en faisait 18 057)**,
30 000 dictionnaire 1 281 → **1 128 Mo**, 10 000 dictionnaire 385 →
**345 Mo**. Les postings perdent 35 à 38 % (noyau : 2 255 → 1 398 Mo), le
`.posmap` prend 8 % (7,7 Mo sur les 30 000, 40 sur le noyau). Le panel de
vérité du noyau : 9/9.

- [x] **WASM rebâti** (`bash bindings/emscripten/build.sh`, `pkg/` et
  `playground/pkg/` recopiés) et le playground revérifié dans Chrome : la
  démo (`?dict`, 1 171 fichiers, 116 Mo en mémoire) rend des spans exacts
  en octets — `spin_lock_init` strict, `mutex lock` relâché
  (`mutex_lock`, `mutex", "lock`, `mutexlock`), fuzzy `compation` d1
  (`Compaction`), regex `SfxFileReaderV[0-9]`, `term lucivy` — lus en
  découpant le texte UTF-8 aux offsets rendus (`TextEncoder`, pas
  `slice` sur la chaîne JS : les offsets sont des octets). Et **un index
  OPFS d'ancien layout** (`?open=user_index`, MDN, 14 629 pages, 529 Mo,
  écrit par le WASM d'avant) rouvert par le nouveau : `addEventListener`,
  `grid template columns` relâché (`grid-template-columns`), regex
  `aria-[a-z]+`, `term fetch`, `async function`, fuzzy `kmallc` — spans
  exacts, 7 à 18 ms.

Temps — l'A/B du protocole (30 000 dictionnaire, même binaire, index
d'ancien layout rouvert (`idx30k-dict4`) contre index neuf (`idx30k-dict5`),
trois passes alternées, min, machine au repos) :

| requête | ancien layout | `SFP5` | ratio |
|---|---|---|---|
| mutex_lock strict | 2,9 ms | 2,8 | 0,97 |
| mutex_lock relax | 2,3 | 2,4 | 1,04 |
| spin_lock strict | 2,3 | 2,5 | 1,09 |
| sched term | 4,5 | 4,6 | 1,02 |
| sched strict | 2,8 | 2,7 | 0,96 |
| printk sw | 3,6 | 3,7 | 1,03 |
| schdule fz1 | 8,9 | 9,3 | 1,04 |
| regsiter fz2 | 155,0 | **141,4** | 0,91 |
| spin_lock_[a-z]+ rx | 14,7 | 14,8 | 1,01 |
| schdule jw1 | 12,0 | 12,0 | 1,00 |

De 0,91 à 1,09 : la règle du ×1,5 n'est pas approchée. Le placement
(`place_spans`) coûte 0,6 à 3,3 ms de somme sur 120 segments (4 à 15 % des
étapes profilées d'une exacte, moins que la vérification).

## 2 ter. Les fichiers dérivés reconstruits en RAM — fait, **sur option** (5 septembre au soir)

Décision de Lucie : oui aux dérivés reconstruits, jamais par défaut.
`derived_in_ram: true` dans le schéma à la création (`SchemaConfig`,
`IndexSettings.derived_in_ram`, `meta.json` ; Python
`derived_in_ram=True`, Node `Index.create(path, fields, shards,
sharedDictionary, derivedInRam)` / `BlobIndexOptions.derivedInRam`, C++,
navigateur et rag3db : la clé du schéma). L'index n'écrit plus `.posmap`,
`.word_pos_map`, `.sibling_v3` ; le lecteur de segment les **rebâtit
depuis les postings à l'ouverture** (`suffix_fst/derived.rs`,
`SegmentReader::open` ; les lecteurs s'ouvrent en parallèle par le DAG du
rechargement ; `Index::derived_cache` garde le résultat par segment pour
que les rechargements ne refassent que les segments nouveaux, élagué aux
segments vivants) — d'abord codé à la première requête, refusé par Lucie :
« tant pis si temps de chargement, je veux pas de lazy, ça trompe les gens
et casse mes showcases » — le posmap depuis les positions
de `.sfxpost` et les `own_len` (`PMP4`), la carte des mots depuis
`.word_sfxpost`, la fratrie depuis les positions consécutives et les mots
consécutifs d'une valeur (entrées de queue exclues : de deux entrées finissant
à la même position, le mot est celle qui commence en premier).

**Identiques octet pour octet** aux fichiers écrits : test unitaire sur un
corpus synthétique (valeurs multiples, document vide, ligne chinoise avec
queue), et `derived_files_match_the_index` sur les vrais index — 320/320
segments des 10 000, 240/240 des 30 000, les trois fichiers (le fichier sur
disque moins le pied du répertoire). `test_derived_in_ram` : mêmes
documents et spans que l'index à fichiers sur le panel de onze requêtes,
en v3 et avec le dictionnaire, après réouverture ; les trois fichiers
absents du disque ; le réglage dans `meta.json`.

| dictionnaire (`du`, tout compris) | fichiers écrits | `derived_in_ram` |
|---|---|---|
| 30 000 fichiers | 1 128 Mo | **829 Mo** (−26,5 %) |
| noyau entier | 4 938 Mo | **3 344 Mo** (−32 %, ×3,9 le texte ; `main` : 18 057) |

Le prix, mesuré : **l'ouverture** paie le rebâti de tous les segments
(`[reader] opened N segment readers`, `LUCIVY_VERBOSE`) : **30 000 : 21 → 286 ms ; noyau : 43 ms → 1 791 ms** pour 253 segments rebâtis en parallèle (43 s de CPU, 449 ms pour le plus gros) — et la première requête retombe à 3,0 ms sur les 30 000, 11,5 sur le noyau, comme les autres ; les requêtes ont les temps de l'index à fichiers (noyau :
11,5 / 11,6 / 19,9 / 11,3 ms contre 11,0 / 11,9 / 21,5 / 11,4 ; fuzzy d2
809 contre 816 ; regex 234 contre 238 — mesuré quand le rebâti était encore
à la première requête, celle-là exclue). Et les structures rebâties sont
résidentes en RAM (1,6 Go sur le noyau) là où un fichier mappé ne coûte que
ce qu'une requête touche. Panels 9/9 partout. Le listage des fichiers d'un
segment (`list_files_for(sfx_version, derived_in_ram)`) ne nomme plus les
trois dérivés sous l'option : `index_bytes`, `preload`, `residency`, le
snapshot LUCE, le delta et le GC ne cherchent pas des fichiers qui
n'existent pas (un compte incomplet est un plancher, pas une mesure).
Exposée dans tous les bindings avec un test chacun (Python
`TestSharedDictionary::test_derived_in_ram_…`, Node
`tests/derived_in_ram.mjs`, C++ `schema_object_with_shared_dictionary_and_
derived_in_ram`, navigateur `derived_in_ram` dans `IndexConfig`) et dans le
playground (`?ram`). Vérifié dans Chrome (`?dict&ram`, démo de 1 171
fichiers) : spans exacts sur strict, relâché, fuzzy et regex ; 12 segments
`.sfxpost` sans aucun `.posmap` / `.word_pos_map` / `.sibling_v3` à côté
dans l'OPFS ; `index_bytes` 94 Mo (les dérivés rebâtis vivent en mémoire,
pas dans ce compte) ; pic WASM 1 650 Mo, le plancher habituel.

**Mesuré dans Chrome sur le noyau et sur MDN** (tard le 5 septembre, même
build WASM, dictionnaire, 4 shards, commandes `index kernel` / `index mdn`
du terminal, page rechargée entre l'indexation et chaque réouverture,
`heap_bytes` = taille de la mémoire linéaire, qui ne redescend jamais) :

| noyau, 15 429 fichiers | fichiers écrits | `?ram` |
|---|---|---|
| indexation | 41 s | 40 s |
| pic mémoire pendant l'indexation | 3 335 Mo (départ 1 518) | **3 859 Mo** (départ 1 646) |
| index dans l'OPFS | 1 571 Mo, 2 056 fichiers (666 dérivés, 411 Mo) | **1 159 Mo**, 1 406 fichiers (−26 %) |
| réouverture (`openDirect`) | 1,6-1,7 s | 2,6-2,7 s |
| `preload` | 3,8-4,0 s (2 042 fichiers) | 2,5-2,6 s (1 392 fichiers) |
| avant service, total | 5,4-5,7 s | 5,1-5,3 s |
| mémoire après ouverture + preload | 2 803 Mo | **3 055 Mo** |
| panel strict / relâché / fuzzy 1 / regex | 71-80 / 20-23 / 43 / 164-172 ms | 54-59 / 24-28 / 42-44 / 170-174 ms |

| MDN, 14 629 pages | fichiers écrits | `?ram` |
|---|---|---|
| indexation | 14 s | 14 s |
| pic mémoire pendant l'indexation | 1 646 Mo | **1 906 Mo** |
| index dans l'OPFS | 478 Mo (288 dérivés, 109 Mo) | **369 Mo** (−23 %) |
| réouverture + `preload` | 0,8 + 2,2 s | 1,3 + 1,5 s |
| mémoire après ouverture | 1 522 Mo (dans le plancher) | 1 518 Mo (idem) |

Ce que ça dit : l'option tient sa promesse sur le stockage (−23 à −26 %
d'OPFS, le tiers des fichiers en moins), le temps avant service ne bouge
pas (le rebâti à l'ouverture coûte 1 s, le `preload` en rend 1,4 parce
qu'il a moins à lire), les requêtes sont les mêmes — mais **le pic mémoire
monte** : +524 Mo pendant l'indexation du noyau (3 859 Mo dans un onglet
qui en a 4 096, trop près), +252 Mo au repos une fois le noyau ouvert,
+260 Mo pendant l'indexation de MDN. Ce ne sont pas les dérivés eux-mêmes
(411 Mo pour le noyau, 109 pour MDN, et sans l'option le `preload` les
charge aussi) : ce sont les **temporaires du rebâti** — les postings de
chaque segment décodés en vecteurs, plusieurs segments à la fois, à
chaque rechargement des lecteurs pendant l'indexation — dans une mémoire
linéaire qui ne redescend jamais. **Décision** : `?ram` reste une option
du playground, pas la vitrine ; pour le noyau dans un onglet, les fichiers
écrits. Si on la veut un jour dans le navigateur : borner le rebâti (un
segment à la fois par shard, ou en flux) et remesurer le pic.

## 2 quinquies. Le banc comparatif rejouable : lucivy, Elasticsearch, tantivy (tard le 5 septembre)

Demande de Lucie : pas seulement la taille, « les faire trébucher là où ils
trébuchent », en une commande. `benches/compare_engines.sh <corpus>
[dossier]` produit `compare_engines.md` en quatre parties, toutes jugées par
le même scan des fichiers (le harnais de vérité) :

1. **taille et temps d'indexation** — Elasticsearch standard et trigrammes +
   `wildcard`, tantivy défaut et `NgramTokenizer`, lucivy v3, dictionnaire,
   dictionnaire + `derived_in_ram` ;
2. **les neuf requêtes vérifiées** (panel `v3_ground_truth_demo`) : vérité,
   lucivy (documents, spans, ms), Elasticsearch (`took`), tantivy ;
3. **où les questions diffèrent** : `spin_lock` strict et relâché, `spinlokc`
   à deux éditions à travers la frontière, la regex, `ude` / `de` (trois et
   deux caractères), `retur -ENOMEM` en phrase floue — vérité par
   `V3_QUERIES` sur l'index dictionnaire avec `LUCIVY_HIGHLIGHT_SPAN_CAP=0`
   (`de` seul fait 7,7 M de spans), et pour chaque moteur la meilleure
   formulation qu'il a, avec la note qui dit pourquoi elle ne pose pas la
   même question ;
4. **le prix des positions** : lucivy rend tous les spans de tous les
   documents dans son temps de recherche ; Elasticsearch (`highlight`,
   fragment entier, balises `\u0001`/`\u0002`, temps de reparsing compté)
   et tantivy (`SnippetGenerator`) sur les 200 premiers documents.

Pièces : `benches/compare_elasticsearch.py` (étendu : panel « où les
questions diffèrent », `highlight_cost`, clé `truth` par ligne en syntaxe
`V3_QUERIES`, tout dans `/tmp/es_compare.json`),
`lucivy_core/benches/compare_tantivy.rs` (étendu : lignes de trébuchement,
`printk*`, JSON par `CMP_OUT`, un trigramme unique devient un `TermQuery` —
la `PhraseQuery` d'un terme fait paniquer tantivy), `benches/
compare_engines_report.py` (assemble le Markdown depuis les journaux du
harnais et les deux JSON ; un compte en gras = la vérité). Un index lucivy
déjà dans le dossier de travail à la même forme est réutilisé.

Deux pièges du banc : la phrase floue `retrun -ENOMEM` en `fz1` rend 0
(vérité 0 aussi) — une transposition vaut **deux** éditions en Levenshtein
pur, une seule pour la `fuzziness` d'Elasticsearch (Damerau) : la ligne est
devenue `retur -ENOMEM`, une lettre en moins, une édition des deux côtés ;
et le tokenizer de la 2.6.0 du navigateur n'a rien à voir ici, le corpus du
banc est le noyau moderne (`/tmp/lucivy-cmp-90k`).

**Résultats** (noyau moderne, 93 983 fichiers, 857 Mo ; rapport complet
généré dans `docs/compare-engines-2026-09-05.md`, section README « Against
Elasticsearch and tantivy ») :

- **Taille** : Elasticsearch standard 781 Mo (×0,9), trigrammes + `wildcard`
  3 082 Mo (×3,6), tantivy 612 / 680 Mo (×0,7 / ×0,8), lucivy v3 6 617 Mo
  (×7,7), dictionnaire **4 926 Mo (×5,8)**, + `derived_in_ram` **3 335 Mo
  (×3,9)**. Indexation : ES 28 / 123 s, tantivy 1,3 / 4,9 s (!), lucivy 56 s
  (v3) et **131 s** (dictionnaire ; 134 s avec `derived_in_ram`) — remesurés
  à neuf tard le 5 : la référence « ~255 s » du 08 datait d'avant la
  compaction du dictionnaire en flux.
- **Les neuf requêtes** : lucivy 9/9 exact sur documents et spans (les
  trois layouts). Sur la sous-chaîne pure les trois moteurs rendent le même
  compte au document près — tantivy seulement par le chemin honnête (ET de
  trigrammes puis vérification sur le texte stocké, 107-151 ms ; la phrase
  de trigrammes rend **0** : positions toutes à 0). Elasticsearch est plus
  rapide sur la sous-chaîne (3-8 ms contre 12-15). Mot entier et préfixe :
  comptes proches mais pas égaux (définition du mot par tokenizer).
- **Où les questions diffèrent** : relâché 9 552 contre 6 577 (ES,
  inexprimable) et 6 601 (tantivy, qui ne sait qu'être relâché) ; `spinlokc`
  d2 10 034 contre 3 549 / 6 557 ; regex 5 510 contre 5 440 (ES, 70 de
  moins, 480 ms) et 0 (tantivy) ; `de` 93 009 contre **0 et 0** ; phrase
  floue 14 449 contre 14 446 (ES `span_near`, qu'il fait bien).
- **Positions** : lucivy 20 797 spans dans 5 145 documents en 15 ms ; ES
  `highlight` sur 200 documents 179 ms ; tantivy, vérification de 5 145
  textes stockés, 96 ms.
- **Ce qu'ils font mieux, écrit tel quel** : la sous-chaîne pure d'ES en
  3-8 ms, l'indexation de tantivy en secondes, leur fuzzy par terme cinq fois
  plus vite que notre d2 (autre question).

Piège corrigé pendant le banc : la `PhraseQuery` de trigrammes rend 0 sur
tout le panel (positions à 0) — le premier passage affichait des zéros là
où le doc d'août disait « exact » ; le chemin honnête (`verified_substring`)
est ce qui rend les vrais comptes, et c'est lui qui est chronométré.

## 2 sexies. Le prochain chantier : le dictionnaire à l'indexation (cadré dans la nuit du 5 au 6)

Question de Lucie : « on est plus lent qu'avant en indexation, on peut rien y
faire ? ». Remesuré à neuf, noyau : v3 **56 s**, dictionnaire **131 s**,
+ `derived_in_ram` **134 s** (3.0.8 : 122 s). Donc le v3 de 4.0 va deux fois
plus vite que la 3.0.8 et le dictionnaire revient à son niveau : le prix de
l'option, ×2,0-2,3 à toutes les tailles (10 000 : 8 s / 19 ; 30 000 : 15,4 /
31,3).

**Où passe le temps** (30 000 fichiers, quatre constructions) : compaction
2 s (`LUCIVY_DICT_MAX_GENERATIONS=1000` → 29,4 s) ; nombre de générations
4-5 s (3 commits au lieu de 15 → 26,8 s) ; **~11 s restants** = le chemin par
jeton et l'écriture de la génération. Lecture du code : `collector_v3.rs:567`
appelle `lookup_or_mint` pour chaque jeton distinct du segment ;
`dictionary.rs::lookup` fait une recherche FST **par génération** (≤ 8),
décode les parents, confirme le texte dans `.termtexts`, alloue les
minuscules ; les textes en attente passent par un `Mutex` global avec
`key.to_string()`. Le v3 interne dans une table de hachage locale, et bâtit
ses FST par segment sur tous les cœurs ; la génération, elle, est par shard
(quatre en parallèle au plus sur 24 cœurs).

Plan, par ordre :

- [ ] **Chronométrer** : deux compteurs cumulés sous `LUCIVY_VERBOSE` —
  `lookup_or_mint` par segment (temps, appels, hits par génération / pending
  / mint) et l'écriture de la génération par commit (FST des nouveaux textes,
  union, `.gmap`, compaction). Sur 30 000 fichiers, dix minutes.
- [ ] **Un cache de hachage `(texte, forme) → id` par shard** devant les FST,
  rempli au fil des lookups (et par la génération à l'ouverture si peu
  cher) : un jeton déjà vu ne touche plus une FST. Borner sa mémoire
  (jamais dans le navigateur sans borne) ; mesurer le gain.
- [ ] **Recouvrir** l'écriture de la génération avec les constructions de
  segments du même commit, au lieu de l'enchaîner ; ou la découper par
  blocs de textes bâtis en parallèle avant l'union en flux, comme la
  compaction.
- [ ] Cible : le dictionnaire à **×1,3 du v3** au lieu de ×2 ; vérité 9/9 et
  fichiers identiques octet pour octet à une construction sans cache.
- Le v3 reste disponible pour qui veut la vitesse d'indexation ; la vitrine
  est en dictionnaire avec des temps acceptables (2.6.0 en 28 s).

## 2 quater. La fuzzy d2 : une étape à risque, un checkpoint avant

`regsiter` d2 sur les 30 000 : 161 ms de mur, 2,57 M de hits (pièces de
deux ou trois lettres, le pigeonhole à deux éditions ne garantit pas
mieux), 463 000 régions dont 91 % rejetées par l'alignement — après que
leur fenêtre a été reconstruite **avec sa carte arrière** (1,5 s de somme
sur les 1,5 + 0,6 s de vérification). Tentative : la fenêtre en deux
passes, texte seul pour l'alignement, carte arrière seulement pour les
régions acceptées (ce que `verify_literal` fait déjà). Exact par
construction ; gain attendu 10 à 20 % sur la fuzzy d2, rien ailleurs ;
gardé seulement si l'A/B du protocole le montre et qu'aucun span ne bouge.
Ce qui serait tenter le diable et qu'on ne fait pas : exiger deux pièces
par région, raccourcir les fenêtres, élaguer par fréquence, approximer la
vérification (le bug de rappel de 3.0.2 à 3.0.6).

**Le dernier checkpoint stable avant cette étape est le tag
`stable-avant-fuzzy-fenetres` (= `137b03b`, poussé)** : lib 1 461 verts,
panels 9/9 partout, postings sans octets, option `derived_in_ram`. Si la
fuzzy perd un span, on y revient.

**Tenté, mesuré, pas gardé.** Les deux passes (texte seul, carte arrière
pour les 6 % de régions acceptées) sont exactes — 9/9 trois fois sur les
30 000, 9/9 sur le noyau, 42 582 spans identiques, vérités fuzzy, regex,
pipeline et dictionnaire vertes — mais ne rendent rien : `regsiter` d2
**143,1 ms contre 141,4** (min de 3, même index, machine au repos),
fenêtres 1 525 → 1 416 ms de somme, `byte_at` 1,43 M → 28 000 appels pour
rien de visible. Le coût d'une fenêtre est la **marche des positions et des
textes** (posmap, méta, texte de chaque token, minuscule), pas sa carte
d'octets. Le code est revenu à celui du tag ; la fuzzy à deux éditions
reste où elle est, et la piste qui resterait est du côté des candidats
(2,57 M de hits pour 42 582 spans), pas de la vérification.

**Seuils calibrés sur les gros index d'avant** (remarque du 5 septembre) :
`LUCIVY_RAM_INDEX_MAX` (3 Go sur wasm32, index tenu entier en dessous) et
le « recharge la page pour servir » du playground (2 Go) datent du 25 août,
quand 10 000 fichiers du noyau pesaient 2,9 Go ; aujourd'hui 15 440 en font
1,8. À remesurer : combien de fichiers un onglet tient maintenant. Ce
n'est pas la cause du « loading into memory » lent : celui-là est
`preload`, qui **attend d'abord les fusions de fond** (`wait_merges_quiet`)
avant de lire — 54 s en onglet neuf avec `commit=1000` (1 844 fichiers,
1 770 Mo), contre 3,8 s de lecture pure mesurés avec `commit=2000`. Le
binding journalise maintenant l'attente à part (`[preload] waited for
merges`) ; WASM rebâti, à relancer dans le navigateur pour le chiffrer.

## 2 bis. La vitrine : un second acte (décidé le 5 septembre)

La démo actuelle — la source de lucivy, 1 171 fichiers, 8,6 Mo, indexée
en 3 s, 126 Mo en mémoire — **reste le premier acte** : une vraie requête
en moins de 10 s après l'ouverture, c'est ce qui retient un visiteur. On
n'y touche pas.

Ce que le navigateur tient aujourd'hui (mesuré, dictionnaire, 4 shards) :

| corpus bundlé dans `playground/` | téléchargement | indexation + fusions | en mémoire |
|---|---|---|---|
| lucivy, 1 171 fichiers | 8,6 Mo | 3 s | 126 Mo |
| `corpus-kernel-2k.tar.gz` | 7,4 Mo | à mesurer (~10 s) | ~250 Mo |
| `corpus-kernel-10k.tar.gz` | 31,5 Mo | à mesurer (~40 s) | ~1,2 Go |
| `corpus-kernel-16k.tar.gz`, 15 440 fichiers | 48,7 Mo | 60 s + 4 s | 1,8 Go |

Le plafond est là : 15 000 fichiers du noyau font 1,8 Go dans un onglet de
4 Go dont l'indexation occupe déjà 1,5 Go de plancher ; 30 000 ne passent
pas, un téléphone s'arrête vers 2 000. « Indexer un plus gros git au
chargement » a donc une borne dure autour de 15 000 fichiers de code et
un coût d'attente d'une minute — pas au chargement, en opt-in.

**Un dépôt entier plutôt qu'un sixième de Linux** (remarque de Lucie) :
mesuré sur les tarballs GitHub (fichiers texte ≤ 100 Ko, le filtre de la
page) — golang/go 15 542 fichiers / 75 Mo, **linux-2.6.0 entier** 14 843 /
134 Mo, godot 13 782 / 117 Mo, **mdn/content 14 917 / 59 Mo**, postgres
7 392 / 70, cpython 5 861 / 62, zig 20 203 / 99 ; trop gros ou trop de
fichiers : TypeScript 66 356 / 126 Mo, rust 62 396 / 136 Mo (des dizaines
de milliers de fichiers de tests : le texte tient, mais ~260 fichiers par
seconde en WASM font des minutes), kubernetes 30 887 / 163, node 50 289 /
303, linux-2.6.32 30 170 / 291. Xubuntu embarque le noyau complet (une
configuration, pas un sous-ensemble) : pas de « mini Linux » de ce côté.

**MDN indexé pour de vrai** (`playground/corpus-mdn.tar.gz`, 13,9 Mo,
14 916 fichiers texte de `mdn/content` `main` ; la page en garde 14 629) :
**15 s d'indexation, 529 Mo en mémoire, pic WASM 1 510 Mo**, rechargement
2,1 s. Requêtes qui servent à un développeur web, à froid :

| requête | docs | ms |
|---|---|---|
| addEventListener | 1 917 | 14 |
| grid-template-columns (strict) | 134 | 7 |
| querySelectorAll | 225 | 9 |
| preventDefault | 183 | 9 |
| IntersectionObserver | 50 | 7 |
| Content-Security-Policy | 222 | 6 |
| fuzzy querySelctor (1 édition) | 1 618 | 59 |
| fuzzy acessibility (1 édition) | 782 | 24 |
| regex `aria-[a-z]+` | 372 | 16 |
| regex `on[a-z]+change` | 207, **tronquée** | 185 |
| phrase « async function » | 446 | 7 |
| flex AND NOT grid | 569 | 16 |
| term fetch | 943 | 11 |

Le premier résultat est presque toujours la page de référence attendue
(`web/api/event/preventdefault`, `web/css/reference/properties/grid-template-columns`…).
La regex `on[a-z]+change` atteint le plafond de 20 000 occurrences par
segment et la page le dit. **C'est le candidat pour le second acte** :
utile à qui le teste, 15 s, un demi-giga. Licence : le contenu MDN est
CC-BY-SA 2.5 (les exemples de code CC0) — la page devra l'attribuer.
Formulation corrigée au passage : « 992 files read » se lisait « ils
n'ont chargé que 992 fichiers » ; la ligne dit maintenant que ce sont
les fichiers de l'index (segments et dictionnaire), pas les documents.

**Les corpus de la vitrine — fabriqués et mesurés (tard le 5 septembre).**
Décision de Lucie : MDN, Go, Godot, Linux (**la 2.6.0 entière, un dépôt
autocontenu, pas une tranche de l'actuel**), TypeScript à l'essai, et tous
les plus légers qui tiennent (PostgreSQL, CPython, Redis, Git, curl, SQLite,
nginx) — chaque visiteur ne télécharge que celui qu'il tape. Mise en place :
`playground/corpora.json` (source `github:owner/repo@ref` ou URL d'un
tarball, ligne de licence, panel de requêtes ; rempli par le script avec les
comptes mesurés) lu par la page au chargement — la table `EXTRA_CORPORA`,
l'aide et l'invite en découlent —, `playground/tools/build_corpus.py`
(bibliothèque standard seule : télécharge, garde les fichiers texte de
≤ 100 Ko avec **le même filtre d'extensions que la page**, élargi aux
`.rst`, `.gd`, `.tscn`, `.cs`, `.s`, `.mjs`… des deux côtés, réemballe sous
un seul dossier), et l'étape « Build the corpora » de `pages.yml` : les
archives ne sont plus fabriquées à la main ni ignorées par le déploiement.
Mesuré dans Chrome (dictionnaire, 4 shards, page rechargée avant chaque
gros corpus ; pic = mémoire linéaire, depuis ~1 520 Mo au repos) :

| `index <nom>` | fichiers | texte | archive | indexation | index | pic mémoire |
|---|---|---|---|---|---|---|
| `linux` (2.6.0 entier) | 14 032 | 126 Mo | 32 Mo | **28 s** (41 avec les commits de 8 Mo) | 1 087 Mo | 3 391 Mo (**2 023** avec les commits de 8 Mo) |
| `mdn` | 14 611 | 57 Mo | 13 Mo | 14 s | 475 Mo | 1 650 Mo |
| `go` | 14 166 | 71 Mo | 16 Mo | 19 s | 686 Mo | 2 291 Mo |
| `godot` | 11 015 | 111 Mo | 20 Mo | 19 s (30 avec les commits de 8 Mo) | 816 Mo | 3 323 Mo (**1 778** avec les commits de 8 Mo) |
| `typescript` | **39 044** | 67 Mo | 9 Mo | 33 s | 462 Mo | 1 522 Mo (inchangé) |
| `postgres` | 5 199 | 56 Mo | 13 Mo | 10 s | 483 Mo | 2 943 Mo |
| `cpython` | 5 344 | 57 Mo | 13 Mo | 10 s | 466 Mo | 2 811 Mo |
| `git` | 3 491 | 26 Mo | 7 Mo | 5 s | 242 Mo | — |
| `curl` | 2 227 | 12 Mo | 3 Mo | 3 s | 110 Mo | — |
| `redis` | 1 731 | 12 Mo | 3 Mo | 2 s | 115 Mo | — |
| `sqlite` | 627 | 9 Mo | 2 Mo | 2 s | 97 Mo | — |
| `nginx` | 439 | 5 Mo | 1 Mo | 1 s | 32 Mo | — |

TypeScript **passe** : 39 044 fichiers en 33 s, 462 Mo, et le pic ne bouge
pas (des fichiers minuscules) ; seule sa requête stricte longue
(`checkExpression`, 2 120 fichiers d'index) coûte 336 ms. Les panels
rendent tous des résultats (les comptes sont dans le scratchpad,
`browser-ram.md`). Le pic dépend de la **taille des fichiers**, pas du
nombre : Godot (111 Mo, gros fichiers C++) monte à 3 323 Mo là où Go
(71 Mo) reste à 2 291 et TypeScript à 1 522 — ce sont les fusions de
segments de 2 000 documents plus gros. **Donc le playground commite aussi
par volume** : tous les 2 000 fichiers **ou tous les 8 Mo de texte**
(`?commitmb=M` pour changer ; 8 Mo = le poids de 2 000 pages MDN, dont la
cadence ne bouge pas). Mesuré, même page, même machine (pic = croissance
de la mémoire linéaire depuis l'ouverture) :

| commit | Godot | Linux 2.6.0 |
|---|---|---|
| 2 000 fichiers (avant) | 19 s, pic +1 800 Mo (3 323) | 28 s, +1 741 Mo (3 391) |
| 8 Mo de texte (**défaut**) | **30 s, +512 Mo (1 778)** | **41 s, +641 Mo (2 023)** |
| 16 Mo de texte | — | 37 s, +1 285 Mo (2 923) |

Le pic tombe de 1,5 Go pour 10 à 13 s d'indexation de plus : c'est ce qui
rend la vitrine sûre sur une machine moins large que celle-ci (16 Mo ne
rend que 4 s pour 650 Mo de plus, refusé). Les requêtes ne changent pas.
L'estimation affichée avant l'indexation est ajustée : 0,3 ms par fichier
+ 0,27 s par Mo de texte (MDN 14 s, Go 19, Godot 30, 2.6.0 41 retrouvés à
quelques secondes). Total des archives : 122 Mo, servies à côté de la page.

**L'invite est libre** (remarque de Lucie : on était forcé d'écrire
derrière `lucivy search`) : elle affiche `$ lucivy ` et prend la suite —
`search "…" --fuzzy 1`, `index mdn`, `open mdn`, `drop mdn`, `list`,
`help`, ou une valeur nue qui est une recherche ; un `lucivy ` tapé en
trop est avalé. La ligne figée montre la commande normalisée
(`lucivy search "…"`, `lucivy index mdn`). Vérifié dans Chrome : les quatre
formes (`ShardedHandle`, `search "wait_merges_quiet" --strict`,
`lucivy search --fuzzy 1 mimaloc`, `list`, `help`) répondent.

Deux bugs de la page trouvés en mesurant, corrigés :

- **`drop <nom>` puis `index <nom>` dans la même page échouait**
  (« lucivy_create returned null », `[create] error: cannot write
  _shard_config.json: I/O error (os error 29)`) : WASMFS garde en cache les
  répertoires qu'il a montés, un répertoire supprimé par le fil principal
  (`removeEntry`) existe encore pour le worker et la création échoue à son
  premier fichier. Correctif : export `lucivy_drop_index(path)`
  (`remove_dir_all` côté worker, idempotent), opération `dropIndex` du
  worker, `Lucivy.dropIndex(path)` (typé dans `lucivy.d.ts`) ; `drop`
  l'appelle avant `removeEntry`. WASM rebâti.
- **Le lecteur tar de la page ne lisait que les 100 octets du champ nom** :
  les noms longs (préfixe ustar, entrée GNU `L`, en-tête PAX `x` avec
  `path=`) étaient perdus en silence — **9 736 fichiers de TypeScript sur
  39 044** (29 312 documents indexés au premier essai), 266 de Godot, et
  tout dépôt GitHub cloné par le proxy aux chemins longs. Corrigé dans
  `extractTarGz` ; vérifié : 39 044 documents, et l'archive de la démo
  (GNU tar) lue comme avant.

À faire :

- [x] **Le second acte, dans le terminal** (décision de Lucie : pas un
  bouton, une commande, on enchaîne) : après la démo, le prompt propose
  `index mdn` — téléchargement, indexation sous les yeux (dictionnaire,
  barre, fusions), attribution CC-BY-SA, un panel de six requêtes MDN
  rejoué, puis la main. `index kernel` pour la démonstration de charge.
  **L'index ouvert vit en RAM, les autres en OPFS** : `index mdn` une
  deuxième fois rouvre en 0,9 s ; `open <nom>`, `index list` (avec la
  taille en stockage), `drop <nom>`. Et `index github owner/repo[@branch]`
  ou une URL github.com, par le proxy, avec un refus honnête au-delà de
  ~220 Mo de texte (curl/curl : 2 225 fichiers, 3 s). Vérifié le 5
  septembre de bout en bout par le pilote du serveur de debug
  ([03](03-knowledge-dump-baselines-tests-outils.md) §7 bis). **Les
  corpus sont choisis, fabriqués, mesurés et déployés par `pages.yml`**
  (ci-dessus) ; `kernel` (la tranche du noyau moderne) laisse la place à
  `linux` (2.6.0 entier), un index de l'ancien nom encore dans un
  navigateur répond toujours à `kernel`. Le texte de la page annonce le
  second acte (note sous le terminal) et la limite honnête dit 200 Mo de
  texte, 2.6.0 en 28 s et 1,1 Go, la tranche moderne en une minute et
  1,6 Go.
- [ ] **Mesurer 2k et 10k** dans le navigateur (temps, mémoire) pour avoir
  un palier intermédiaire à 30-40 s.
- [ ] **Le chiffre de vitrine** en tête de page : « le noyau Linux 2.6.0
  entier indexé dans votre onglet en 28 s, chaque requête en 20 à 50 ms,
  exacte » à côté du « 93 605 fichiers en natif, comptes et spans vérifiés
  contre le disque ». La note sous le terminal le dit déjà en une phrase ;
  le titre et la section « Numbers » ne l'ont pas encore.
- [ ] **Le plancher de 1,5 Go de l'indexation WASM** (§1 bis) : c'est lui
  qui décide combien de fichiers un onglet tient, plus que la taille de
  l'index. Mesurer par paliers de `heap_bytes` pendant la démo `?verbose`
  qui tient les 1,5 Go (tas de l'écrivain, arènes de construction, cache
  de fichiers, budget du collecteur), puis réduire ce qui peut l'être.
- Le clonage `owner/repo@branch` à la demande existe déjà (proxy, limite
  anonyme GitHub de 60 requêtes par heure partagée, taille inconnue) : une
  fonction, pas une promesse de la page.

## 3. À faire, en vrac (repris de [01](01-journal-session-5-septembre.md) §11)

- La regex à ×1,6 en mode dictionnaire.
- La DFS de fratrie (recherche global → local à chaque pas).
- `index_bytes`, `preload`, `residency` ignorent les `dict-*`.
- Le sigma grec final dans `starts_with_ci`.
- [x] **Le prérequis de 4.0.0 est levé** (5 septembre au soir) :
  `lucivy_core/tests/fixtures/index-3.0.8/` — deux index (un shard, deux
  shards, 18 documents dont une ligne chinoise à queue et un document de
  repli de casse) construits par le **wheel PyPI 3.0.8** (`build.py`, venv
  `uv` avec `lucivy==3.0.8`), et `panel-3.0.8.json`, les réponses du wheel
  à 14 requêtes (documents et spans). `test_compat_308.rs` : le binaire v4
  ouvre l'index (`sfx_version` 3, `SFP3`/`WSP3`/`PMP3` sur disque) et rend
  **exactement** les réponses de la 3.0.8 ; six documents ajoutés, un
  commit (le premier segment v4, `SFP5`) : aucun document ni span perdu, les
  nouveaux trouvés avec des spans exacts ; `compact` fusionne les segments
  3.0.8 dans les layouts courants (le merge lit les anciens, convertit les
  queues, écrit `SFP5`/`WSP5`/`PMP4`) : mêmes réponses ; réouverture : mêmes
  réponses. 7,1 Mo de fixture (le format 3.0.8 pèse ×45 le texte : c'est
  tout l'objet de v4). La ligne du CHANGELOG est écrite. **Le numéro est
  posé** (5 septembre au soir) : tout le workspace est en 4.0.0 — les cinq
  crates et leurs dépendances internes, les bindings, les sept paquets npm,
  pyproject, le playground (estampillé par `build.sh` depuis `Cargo.toml`)
  ; rien n'est publié, la 3.0.8 reste la dernière sur les registres.
- **Version 4.0.0 — décidé le 5 septembre, avec un prérequis.** Le majeur
  se justifie par le contrat sur le disque : un binaire v4 lit un index
  3.0.x (chaque lecteur ouvre les anciens layouts, tests unitaires par
  layout), mais un binaire 3.0.x **ne lit pas** un index v4 (conteneur 8,
  ordinaux 28 bits, tables par blocs, PMP3, SIB3, layout 3), et le premier
  commit ou la première fusion en v4 dans un index 3.0.x le convertit sans
  retour. Prérequis avant le numéro : un **test de compatibilité de bout
  en bout** — un petit index construit par la 3.0.8 publiée (le wheel
  PyPI), gardé en fixture, ouvert par le binaire v4 : mêmes comptes et
  spans sur un panel, puis un commit dessus et la vérité toujours juste
  après conversion. Et la ligne du CHANGELOG : « 4.0 ouvre vos index
  3.0.x ; 3.0.x n'ouvre pas les index 4.0 ; le premier commit en 4.0
  convertit sans retour ». Leçon de `sparse.mmap` en 3.0.6 : plus de
  changement de format en mineur.
- Décisions restantes : pile v2 ; fusionner `wip/publication-3.0.0`
  dans `main` ; tri stable des ex æquo dans le merge des shards.
