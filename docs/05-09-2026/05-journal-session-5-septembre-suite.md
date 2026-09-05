# Journal — 5 septembre 2026, suite : compaction en flux, playground, navigateur

Ce journal prend la suite de [01](01-journal-session-5-septembre.md) (le
matin et l'après-midi du 5 : le plan par shard, le `.gmap` GMP2, l'option
`shared_dictionary`, la vérité du noyau). Il couvre la fin de la session :
la compaction du dictionnaire en fusion de flux, la validation du
playground et du navigateur avec tout ça, deux corrections trouvées en
mesurant, et ce qui reste. Pour repartir : ce fichier, puis
[04](04-progression-et-a-faire.md) (l'état et le todo, tenu à jour au fil
de l'eau), [02](02-architecture.md), [03](03-knowledge-dump-baselines-tests-outils.md).
Branche `v4`, jamais `main`. Dernier commit de la session : voir `git log`.

## 1. La compaction du dictionnaire en fusion de flux

Le point de départ, mesuré avant de coder (banc
`dictionary_compact::compaction_of_an_index_on_disk`) : la compaction
naïve du dictionnaire du noyau — `all_texts()` puis le builder qui regénère
et retrie 131 M de suffixes — coûtait **48 s et 12,8 Go de RAM anonyme**,
à chaque huitième commit. C'est ce que la construction du 90k payait cinq
fois et ce qui a fait tomber l'éditeur la veille.

Le remplacement, `src/suffix_fst/dictionary_compact.rs` : les FST des
générations parcourues ensemble en ordre de clés (`OpBuilder::union` de
`lucivy_fst`), record de parents **copié tel quel** quand une seule
génération tient la clé, parents concaténés-triés-dédoublonnés-réencodés
sinon ; FST et table des parents écrites **en flux** sur disque (fichiers
temporaires `dict-<g>.<champ>.sfx.fst.tmp` / `.sfx.parents.tmp`, puis le
conteneur assemblé, `file_v3::write_container`) ; `.termtexts` par un tas
sur les curseurs des générations en trois passes
(`termtexts_v3::write_merged`), seule la table des offsets en RAM.
`choose_compaction` : au-delà du maximum (8), les **plus petites**
générations fusionnent jusqu'à ramener le compte à 4 ; la plus grosse ne
rejoint une fusion que quand assez d'autres l'ont dépassée.
`remove_leftovers` efface ce qu'un commit planté a laissé sous un numéro
réutilisé (bloquait le commit suivant, `create_new`).

| Dictionnaire (champ contenu) | naïf | flux |
|---|---|---|
| 30 000 fichiers, 7 générations, 4,1 M clés | 13,0 s, 3,8 Go | 7,2 s, 0,68 Go résidents |
| noyau, 2 générations (902 + 21 Mo), 9,9 M clés, 22,5 M textes | 48,0 s, 12,8 Go anonymes | **18,9 s, 229 Mo anonymes** |

Fichiers **identiques octet pour octet** à la reconstruction (test unitaire
synthétique, 30 000, noyau). Vérité de bout en bout : index de 5 000
fichiers construit avec un commit tous les 500 et trois générations au plus
(six compactions), panel 9/9, `contains` 15/15, `coherence` 31/31. Détail :
[01](01-journal-session-5-septembre.md) §13.

## 2. Le playground et le navigateur avec tout ça — fait

- Build emscripten (`bash bindings/emscripten/build.sh`, emsdk 6.0.8,
  nightly `-Z build-std`) sans changement du binding. `?dict` crée l'index
  avec `shared_dictionary: true`, `?commit=N` commit tous les N fichiers,
  `?merges=N`, `?verbose`, `?open=user_index` rouvre l'index persistant.
- Démo (source de lucivy, 1 171 fichiers) en v3 et en dictionnaire : mêmes
  comptes sur les 8 requêtes et sur trois requêtes tapées ; sur ce petit
  corpus le dictionnaire ne gagne rien en mémoire (126 contre 118 Mo).
- **15 440 fichiers du noyau indexés dans le navigateur en mode
  dictionnaire** (4 shards ; 1 782 Mo en mémoire contre 1 996 en v3,
  −11 %), avec `commit=1000` deux compactions en flux passées dans WASM
  (fichiers temporaires par le répertoire à écriture différée, relus après
  `terminate()`). `mutex_lock` relâché : 1 547 documents des deux côtés,
  charge utile identique à l'octet.
- **Panel de 21 requêtes** (`parity_panel.json`) dictionnaire contre v3,
  froid puis chaud : même ordre ou plus rapide sur 19 (les `contains` et
  fuzzy d1 gagnent 10 à 30 %), plus lent sur la regex `spin_lock_[a-z]+`
  (×1,1) et `path contains ethernet/intel` (×1,5, le dictionnaire du champ
  `path` est minuscule). `fuzzy d2 kmalloc` : 7 320 contre 7 317, les deux
  **tronquées et signalées** (`last_search_truncated`, plafond de 20 000
  occurrences par segment sur wasm) — pas un rappel différent.
- Piège de test : **deux onglets qui indexent en même temps** échouent au
  premier commit (`I/O error` code 29, même répertoire OPFS `user_index`).

## 3. Les fusions dans le navigateur : de 1 à 2

Le « loading into memory… » lent (54 à 96 s) était **l'attente des fusions
de fond** que `preload` impose avant de lire (`wait_merges_quiet`), pas
l'OPFS (lecture 4 s) ni le choix RAM / flux (1,8 Go < 3 Go). Trace ajoutée
par fusion (`[merge] N segments: waited … ran …`, `LUCIVY_VERBOSE`) : quatre
fusions, une par shard, 24 à 30 s chacune, **strictement en file** derrière
le permis à 1 (`merge_permits.rs`, 24 août : une fusion v3 rebâtit la FST
en RAM, quatre à la fois tuaient le navigateur).

| 15 440 fichiers, dictionnaire, `commit=1000`, 4 shards | indexation | attente des fusions | pic mémoire WASM |
|---|---|---|---|
| fusions à 1 | 60 s | 73,6 s | 2 543 Mo (index 1 775) |
| fusions à 2 | 82 s (chevauchent l'indexation) | 3,8 s | 2 539 Mo |
| fusions à 4 | 62 s | 15,7 s | 2 539 Mo |
| fusions à 4, scheduler 12 threads | 65 s | 8,7 s | 2 539 Mo |
| fusions à 4, 4 threads d'indexation | **140 s** | 10,0 s | 2 283 Mo |

Une fusion en mode dictionnaire ne rebâtit aucune FST : le pic ne bouge
pas à l'octet. **Décision** : 2 par défaut dans le navigateur pour un index
à dictionnaire (posé par le binding à la création et à l'ouverture,
`[lucivy-wasm] merge concurrency 2 (shared dictionary)` dans le journal),
1 conservé pour v3 ; `--merge-concurrency=N` / option `mergeConcurrency`
pour forcer ; `memoryStatus().heap_bytes` = taille de la mémoire linéaire
(le pic). Plus de threads n'aide pas (borne mémoire et allocateur, pas
CPU) ; plus de threads d'indexation multiplie les segments et les fusions.
Le plancher WASM en indexation est de **1,5 Go avant tout index** (démo :
pic 1 650 Mo pour un index de 126 Mo) — réductible, de notre côté, noté.

Piège : `lucivy.js` **filtre les options d'initialisation** ; une option
ajoutée au worker sans l'ajouter à la liste de `lucivy.js` n'arrive jamais
au moteur (c'est ce qui a rendu le premier essai « 2 » identique à « 1 »).
La ligne du journal moteur est la preuve qu'un drapeau est arrivé.

## 4. Deux corrections trouvées en mesurant

**Les queues de mots chinois.** Le test `byte_spans_are_derivable`
(`postings_measure.rs`), écrit pour le chantier des postings, a trouvé 3
postings de mots sur 137 millions (les mêmes 3 sur 30 000 et sur le
noyau) dont `first_position` désignait le chunk d'après `byte_from`. Des
lignes chinoises entières (aucun séparateur → un « mot » de plusieurs
centaines d'octets) : au-delà de 264 octets le collecteur écrit une
**entrée de queue** (les 8 derniers octets) dont la position était celle
du dernier chunk du mot — faux quand les séparateurs de fin débordent dans
un chunk à eux (`解。\n\n` remplit le chunk, `.. ` commence le suivant).
Corrigé dans `collector_v3.rs` (position = le chunk qui contient
`byte_from`), test unitaire
`word_position_when_separators_spill_into_the_next_chunk`, index 30 000
reconstruit : 0 désaccord partout. Les octets, donc les highlights, étaient
justes ; la position, donc `.word_pos_map`, était décalée d'un chunk.

**`memory_status` prenait une seconde.** Lucie voyait la même requête
passer de 60 à 400 ms une fois sur deux ; le moteur disait 16 à 44 ms à
chaque fois. La page appelle `memoryStatus()` après chaque recherche
(drapeau de troncature), et l'appel recomptait les octets en **ouvrant les
1 700 fichiers sur OPFS** (0,8 à 1,3 s) sans passer par le cache que
`residency()` utilise ; le worker sert les messages en file, la frappe
suivante attendait derrière. Corrigé : `shard_bytes_and_files_cached`
(mémo par liste de segments), 6 à 9 ms, page et moteur à la milliseconde.

## 5. Le chantier des postings, cadré

Mesuré (`postings_without_byte_spans`) : les `byte_from`/`byte_to` des
postings pèsent **842 Mo sur le noyau, 37 % des postings, 15 % de
l'index** (5,7 → 4,9 Go), 151 Mo / 12 % sur 30 000. Et ils sont
**entièrement dérivables** (0 désaccord sur 167 M chunks et 137 M mots
après le correctif) : `byte_from` = somme cumulée des `own_len` par
`.posmap` + méta des textes, `byte_to − byte_from` = `own_len` (chunk) ou
`own_len − sep_len` (mot). Réserve : les queues de mots très longs partent
au milieu d'un chunk (il leur faudrait un décalage). Le vrai point dur,
à mesurer avant de coder : les résolveurs fabriquent aujourd'hui les spans
pour **chaque occurrence candidate**, pas seulement pour le top-k
(`resolve.rs`, 79 usages) ; dériver au lieu de lire coûterait à chaque
occurrence — il faudrait des spans paresseux (résolus pour les highlights,
les fenêtres regex et la vérification fuzzy). Détail et ordre proposé :
[04](04-progression-et-a-faire.md) §2.

## 6. Où on en est, en taille

Noyau entier, 857 Mo de texte : `main` non compacté 18 057 Mo (×21),
`main` compacté à 10 segments 11 025 Mo, v4 un `.sfx` par segment
7 422 Mo, **v4 dictionnaire 5 706 Mo (×6,7, −68 % depuis `main`)**.
Répartition : postings 40 %, cartes de positions et fratrie 27 %
(dérivées des postings), dictionnaire 21 %, store 6 %.

## 6 bis. La vitrine : MDN comme second acte, dans le terminal

Décision : un dépôt entier plutôt qu'un sixième de Linux, et d'abord un
corpus utile à qui l'essaie. Dépôts mesurés (fichiers texte ≤ 100 Ko) :
go 15 542 / 75 Mo, linux-2.6.0 entier 14 843 / 134 Mo, godot 13 782 /
117 Mo, **mdn/content 14 917 / 59 Mo** ; TypeScript et Rust tiennent en
texte mais pas en nombre de fichiers (62-66 000). MDN indexé pour de vrai
dans le navigateur : **14 629 pages en 14 s, 528 Mo en mémoire**, requêtes
de 6 à 16 ms, premier résultat = la page de référence attendue. Mis en
place au prompt du terminal, pas un bouton : `index mdn` (téléchargement,
indexation sous les yeux, attribution CC-BY-SA, panel de six requêtes, la
main), `index kernel`, `index github owner/repo[@branch]` (proxy, refus
au-delà de ~220 Mo de texte), `index list` / `open` / `drop` — **l'index
ouvert en RAM, les autres en OPFS**, rouverts en une seconde. La ligne de
préchargement dit désormais « its N index files (segments and
dictionary, not documents) ». Les corpus `.tar.gz` sont ignorés par git,
recette dans [04](04-progression-et-a-faire.md) §2 bis. Deux pièges de
pilotage notés dans [03](03-knowledge-dump-baselines-tests-outils.md)
§7 bis (le serveur de debug rend des chaînes ; `pkill -f` se tue lui-même).

## 7. Ce qui reste (le todo vit dans [04](04-progression-et-a-faire.md) §3)

Pour la vitrine : les tarballs `golang` / `godot` / `linux-2.6.0` si on les
veut sur la page, le texte de la page qui annonce le second acte, le
déploiement des corpus (ignorés par git : à fabriquer côté déploiement).
Pour 4.0.0 : la fixture 3.0.8 et le test de compatibilité de bout en bout
(04 §3). Puis :

Le chantier des postings (§5) ; les trois fichiers dérivés reconstruits en
RAM en option (jamais le défaut natif : une structure rebâtie est
résidente là où un fichier mappé ne coûte que ce qu'on touche) ; le
plancher de 1,5 Go du navigateur et les seuils calibrés sur les gros index
d'avant (`LUCIVY_RAM_INDEX_MAX` 3 Go, rechargement à 2 Go) ; la regex à
×1,6 en dictionnaire ; `index_bytes` / `preload` / `residency` et les
`dict-*` ; décisions : version 4.0.0, pile v2, `wip/publication-3.0.0`
dans `main`, tri stable des ex æquo.

## 8. Commits de la suite

`634f0e6` compaction en flux · `5a8f6b0` docs (5 000 fichiers, pas
10 000) · `68e0737` playground `?dict` `?commit`, validé · `4aa6782`
collecteur (queues de mots), mesures postings, preload wasm ·
`bb97ec8` fusions à 2 pour le dictionnaire, `--merge-concurrency`,
`heap_bytes`, trace `[merge]` · `f0608bb` docs WASM fait, panel 21
requêtes · `ce819e8` `memory_status` par le cache · `af6420a` ce journal
et les docs complétés · `8147b38` docs vitrine · `7a0854f` corpus MDN,
formulation du préchargement · `aae6065` `index mdn` / `index kernel` au
prompt · `e83a475` `index github`, `index list`, `open`, `drop`, règles du
serveur de debug.
