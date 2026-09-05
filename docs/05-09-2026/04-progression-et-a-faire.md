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
| v4, dictionnaire par shard (`idx90k-dict2`) | 253 | **5 706 Mo** | ×6,7 |

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
  ([03](03-knowledge-dump-baselines-tests-outils.md) §7 bis). Reste : les
  tarballs `golang` / `godot` / `linux-2.6.0` si on les veut sur la page,
  et le texte de vitrine.
- [ ] **Mesurer 2k et 10k** dans le navigateur (temps, mémoire) pour avoir
  un palier intermédiaire à 30-40 s.
- [ ] **Le chiffre de vitrine** : « 15 440 fichiers du noyau indexés dans
  votre onglet en une minute, chaque requête en 30 ms, exacte » à côté du
  « 93 605 fichiers en natif, comptes et spans vérifiés contre le disque ».
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
