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

## 1. Le playground marche encore avec tout ça — validé le 5 septembre

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
- Observation à creuser : le `preload` de cet index a pris **81,5 s**
  (1 694 fichiers, 1 769 Mo) contre 3,8 s pour l'index de la même taille
  bâti avec un commit tous les 2 000 (2 117 fichiers) ; et **95,8 s en
  v3** avec `commit=1000` (1 680 fichiers, 1 957 Mo). Donc lié au
  `commit=1000` (ou à la succession d'index de 1,8 Go dans le même
  onglet), pas au dictionnaire. Soit l'OPFS était encore occupé par les
  fusions de fond, soit la RAM de l'onglet approchait la borne — à
  reproduire dans un onglet neuf avant de conclure.

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

## 3. À faire, en vrac (repris de [01](01-journal-session-5-septembre.md) §11)

- La regex à ×1,6 en mode dictionnaire.
- La DFS de fratrie (recherche global → local à chaque pas).
- `index_bytes`, `preload`, `residency` ignorent les `dict-*`.
- Le sigma grec final dans `starts_with_ci`.
- Décisions : version 4.0.0 ; pile v2 ; fusionner `wip/publication-3.0.0`
  dans `main` ; tri stable des ex æquo dans le merge des shards.
