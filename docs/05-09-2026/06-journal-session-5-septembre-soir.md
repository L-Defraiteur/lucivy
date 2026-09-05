# Journal — 5 septembre 2026, soir : postings sans octets, dérivés en RAM, fixture 3.0.8, 4.0.0

Suite de [05](05-journal-session-5-septembre-suite.md) (la compaction en
flux, le playground, la vitrine) et de [01](01-journal-session-5-septembre.md)
(le dictionnaire de shard). Pour repartir : ce fichier, puis
[04](04-progression-et-a-faire.md) (l'état et le todo, tenu au fil de
l'eau — §2 postings, §2 ter dérivés, §2 quater fuzzy, §3 4.0.0), puis
[07](07-architecture.md) et [08](08-knowledge-dump-baselines-tests-outils.md),
tous autonomes. Branche `v4`, jamais `main` ; `origin/v4` au niveau du
dernier commit (`5d85de9`). **Le workspace est en 4.0.0, rien n'est publié.**

## 1. Les postings sans span d'octets (`SFP5`, `WSP5`, `PMP4`)

Mesuré avant de coder (profil du panel 30 000) : les résolveurs
matérialisaient un span par occurrence candidate — 2,57 M pour `regsiter`
d2, 42 582 rendus — mais sur le chemin v3 **aucune décision d'adjacence ne
lit un octet** : tout passe par les positions, et `rebuild_window_opts`
dérivait déjà ses offsets de position en position par `own_len`.

Fait en deux commits (`556262a`, `cbea452`) :

- **`PMP4`** : le `.posmap` garde l'offset d'octet d'une position sur 16
  par document (0,25 o/position ; une case vide remet à zéro, frontière de
  valeur). `BriquesContext::byte_at(doc, p)` = point de contrôle + somme
  des `own_len` (méta) ; sur un segment ancien, le posting du chunk. Le
  dictionnaire reçoit les `own_len` par la méta du shard à la fusion.
- **`SFP5`** : une entrée de chunk = un delta de position. **`WSP5`** :
  `d_doc`, `d_first`, `(last − first) << 1 | drapeau`, et le décalage dans
  le chunk pour les seules **entrées de queue** (mots de plus de 264
  octets, qui partent au milieu d'un chunk). Longueurs par la méta : chunk
  `own_len`, mot `own_len − sep_len` (0 désaccord sur 137 M mots du noyau).
- `PostingResolver` rend des positions (`positions`, `positions_filtered`,
  `positions_for_doc`, `has_position`, `has_byte_spans`). `MatchV3` sort
  des résolveurs **non placé** (`first_off`, `last_start_pos`, `last_off`,
  `last_consumed`) ; `orchestrator::place_spans` dérive les octets des
  matches gardés. Hits fuzzy et regex regroupés **par positions**. Le
  pipeline v2 (`sfx_version` 2) garde `SFP4`. Anciens layouts lus, leurs
  spans encore servis (`word_tail_off`).
- **Fusions** : plus d'octets à remapper ; un segment ancien fusionné
  convertit ses queues par son `.posmap` et ses postings à spans
  (`tail_off_from_spans`).

Deux choses apprises par la vérité, pas par les 1 460 tests unitaires :
pour un dernier jeton qui est un **mot**, son texte commence à son
**premier** chunk (`last_position` est la fin du span, l'adjacence) —
`mutex lock` relâché rendait `[5441..5456]` pour `[5441..5451]` ; et le jeu
de séparateurs du regroupement fuzzy était en **octets** : 32 positions
regroupaient quatre fois trop (DP 515 → 903 ms), ramené à 5 positions.

| fichiers SFX (tantivy exclu) | avant | après |
|---|---|---|
| 10 000, v3 | 460,2 Mo | 420,9 Mo (−8,5 %) |
| 10 000, dictionnaire | 331,3 Mo | 290,8 Mo (−12,2 %) |
| 30 000, v3 | 1 496,1 Mo | 1 352,7 Mo (−9,6 %) |
| 30 000, dictionnaire | 1 131,7 Mo | 977,9 Mo (−13,6 %) |

`du` tout compris : 10 000 dictionnaire 385 → **345 Mo**, 30 000 1 281 →
**1 128**, **noyau 5 717 → 4 938 Mo (×5,8 le texte ; `main` : 18 057)**.
Postings −35 à −38 %, posmap +8 %. A/B du protocole (trois passes alternées,
min, machine au repos, ancien layout rouvert contre neuf, même binaire) :
**×0,91 à ×1,09** sur les dix requêtes, fuzzy d2 155,0 → 141,4 ms. Vérité :
lib 1 460, référence 10 000 v3 et dictionnaire 9/9, `contains` 15/15,
`coherence` 31/31, anciens index rouverts 9/9, 30 000 et noyau neufs 9/9 ;
`byte_spans_are_derivable` : 0 désaccord sur 167 M positions et 137 M mots.
WASM rebâti, démo et un index OPFS d'ancien layout (MDN) vérifiés dans
Chrome, spans exacts.

## 2. Les fichiers dérivés reconstruits en RAM — `derived_in_ram`, sur option

« Ça va presque gagner un Go quand même, oui pour les fichiers dérivés mais
sur option » — puis « faut que ça se reconstruise avant requête, tant pis si
temps de chargement, je veux pas de lazy, ça trompe les gens et casse mes
showcases ». Donc (`f087689`, `5d85de9`) :

- `derived_in_ram: true` à la création (`SchemaConfig`,
  `IndexSettings.derived_in_ram`, `meta.json`, jamais le défaut) : l'index
  n'écrit plus `.posmap`, `.word_pos_map`, `.sibling_v3`. Le lecteur de
  segment les **rebâtit à l'ouverture** (`SegmentReader::open`,
  `suffix_fst/derived.rs`) — les lecteurs s'ouvrent en parallèle par le DAG
  du rechargement, et `Index::derived_cache` garde le résultat par segment
  (élagué aux segments vivants) pour que les rechargements ne refassent que
  les nouveaux. Le rebâti est **identique octet pour octet** aux fichiers :
  test unitaire (valeurs multiples, document vide, ligne chinoise à queue),
  `derived_files_match_the_index` sur 320/320 et 240/240 segments réels.
- `list_files_for(sfx_version, derived_in_ram)` ne nomme plus les trois
  dérivés sous l'option : `index_bytes`, `preload`, `residency`, LUCE,
  delta et GC ne cherchent pas des fichiers absents.
- Exposée partout : clé du schéma (C++, navigateur, rag3db), Python
  `derived_in_ram=True`, Node `derivedInRam`, un test par binding, `?ram`
  dans le playground (vérifié dans Chrome : douze segments sans dérivés
  dans l'OPFS, spans exacts, `index_bytes` 94 Mo).

| dictionnaire (`du`) | fichiers écrits | `derived_in_ram` |
|---|---|---|
| 30 000 | 1 128 Mo | **829 Mo** |
| noyau | 4 938 Mo | **3 344 Mo** (×3,9 le texte, 5,4 fois moins que `main`) |

Le prix est à l'ouverture : noyau 43 ms → **1,8 s** (253 segments, 43 s de
CPU répartis, 449 ms pour le plus gros), 30 000 21 → 286 ms ; les requêtes
gardent leurs temps, la première comprise (3,0 ms sur les 30 000, 11,5 sur
le noyau) ; structures résidentes (1,6 Go sur le noyau) là où un fichier
mappé ne coûte que ce qu'une requête touche.

## 3. La fuzzy d2 : tenté, mesuré, pas gardé

Tag **`stable-avant-fuzzy-fenetres`** (= `137b03b`, poussé ; pas de `v`
devant, les tags `v*` déclenchent la publication) posé avant l'essai. Les
fenêtres en deux passes (texte seul pour l'alignement, carte arrière pour
les 6 % de régions acceptées) : exactes — 9/9 trois fois sur les 30 000,
9/9 sur le noyau, 42 582 spans identiques — mais **143,1 ms contre 141,4**.
Le coût d'une fenêtre est la marche des positions et des textes, pas sa
carte d'octets. Code revenu au tag (`f0b5c6e`), résultat négatif noté
([04](04-progression-et-a-faire.md) §2 quater). La seule piste qui reste est
côté candidats (2,57 M de hits pour 42 582 spans, le pigeonhole à deux
éditions impose des pièces de deux lettres) ; pas de moyen exact connu.

## 4. La fixture 3.0.8 : le prérequis de 4.0.0 (`d576f53`)

Le wheel PyPI 3.0.8 (venv `uv`, `pip` n'est pas installé) construit deux
index (un shard, deux shards, 18 documents dont une ligne chinoise à queue
et un document de repli de casse) et répond à 14 requêtes ; le tout dans
`lucivy_core/tests/fixtures/index-3.0.8/` (7,1 Mo : le format 3.0.8 pèse
×45 le texte, c'est tout l'objet de v4), avec `build.py`.
`test_compat_308` : v4 rend **exactement** les documents et spans de la
3.0.8 ; six documents ajoutés et un commit (premier segment `SFP5`) : rien
de perdu, les nouveaux trouvés avec des spans exacts ; `compact` fusionne
les segments 3.0.8 dans les layouts courants : mêmes réponses ; réouverture :
mêmes réponses. Le contrat est dans le CHANGELOG : 4.0 ouvre 3.0.x, 3.0.x
n'ouvre pas 4.0, le premier commit convertit sans retour.

## 5. 4.0.0 (`9869471`)

Tout le workspace porte le numéro : les cinq crates et leurs dépendances
internes, les bindings, les sept paquets npm, pyproject, le playground
(estampillé `4.0.0-<sha>` par `build.sh` depuis `Cargo.toml`), le
CHANGELOG (« Lucivy 4.0.0 (branch v4 — not published yet) »). Aucun tag
`v*`. `release.yml` : tout tag `v*` construit ; les jobs de publication
partent si `PUBLISH_ENABLED == 'true'` (variable de dépôt) sous
l'environnement `release`, **sans réviseur** — vérifier la variable avant
de poser `v4.0.0`, ou la passer à `false` d'ici là.

## 6. Le CHANGELOG hérité

Le bas de `CHANGELOG.md` était l'historique de tantivy vendorisé (0.25 et
avant), le renommage du fork y avait remplacé le nom partout, liens compris
(le lien appveyor pointait sur `fulmicoton/lucivy`). Nom restitué sous un
titre qui dit ce que c'est (`f26c1d7`).

## 7. Ce qui reste (le todo vit dans [04](04-progression-et-a-faire.md))

- **Publier 4.0.0** : décision de Lucie ; vérifier `PUBLISH_ENABLED`
  (`gh`, avec son accord), suivre `RELEASE.md`, publier les crates en
  dernier.
- **La vitrine** ([04](04-progression-et-a-faire.md) §2 bis) : **douze
  corpus** choisis par Lucie (MDN, Linux 2.6.0 entier, Go, Godot,
  TypeScript, PostgreSQL, CPython, Redis, Git, curl, SQLite, nginx),
  décrits dans `playground/corpora.json`, fabriqués par
  `tools/build_corpus.py` et par `pages.yml`, mesurés dans Chrome (2.6.0 :
  28 s, 1,1 Go ; TypeScript 39 044 fichiers en 33 s) ; deux bugs de la page
  corrigés au passage (`drop` puis `index` dans la même page, noms longs du
  tar). Reste le chiffre en tête de page.
- **Le navigateur** : `?ram` **mesuré** (plus tard le 5, [04](04-progression-et-a-faire.md)
  §2 ter) sur le noyau (15 429 fichiers) et MDN : OPFS −26 % / −23 %,
  temps avant service égal, requêtes égales, **mais pic mémoire +524 Mo
  à l'indexation du noyau (3 859 Mo sur 4 096) et +252 au repos** — les
  temporaires du rebâti dans une mémoire qui ne redescend pas. Reste
  option du playground, pas la vitrine. Restent : le plancher de 1,5 Go
  de l'indexation ; les seuils (`LUCIVY_RAM_INDEX_MAX`, rechargement à
  2 Go) calibrés sur des index 40 % plus gros.
- La regex à ×1,6 en dictionnaire ; `index_bytes` / `preload` / `residency`
  et les `dict-*` ; la DFS de fratrie ; le sigma grec final.
- Décisions : pile v2, `wip/publication-3.0.0` dans `main`, tri stable des
  ex æquo.

## 8. Commits de la soirée

`556262a` PMP4 et `byte_at` · `cbea452` postings sans octets · `f26c1d7`
CHANGELOG tantivy, A/B trois passes · `f087689` `derived_in_ram` ·
`137b03b` WASM · `7c84871` tag et note · `f0b5c6e` fuzzy deux passes
revenue · `d576f53` fixture 3.0.8 et `test_compat_308` · `9869471` 4.0.0 ·
`5d85de9` `derived_in_ram` dans le listage, les bindings, le playground.
