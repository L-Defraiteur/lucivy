# L'indexation, profilée — 13 septembre 2026

Point de départ : le noyau entier (101 141 fichiers, 899 Mo) s'indexait en **94-97 s**
en dictionnaire, contre 5 s pour tantivy — le plus gros écart qui nous restait, et
jamais profilé depuis le repli différé du 6 septembre. Arrivée : **65,6 s**, mêmes
réponses, deux changements de quelques lignes chacun et un cache. Voici le chemin,
avec ses fausses pistes, parce qu'elles disent comment mesurer la prochaine fois.

## 1. Outils

- **`samply`** (installé, `cargo install samply`) refuse de tourner tant que
  `kernel.perf_event_paranoid` vaut 2 ; il faut `echo 1 | sudo tee
  /proc/sys/kernel/perf_event_paranoid` (ne survit pas au redémarrage). Pas fait
  cette session.
- **Le profileur du pauvre** qui a servi : `benches/gdb_sample.sh` lance le binaire
  sous gdb, lui envoie `SIGALRM` toutes les 250 ms, gdb l'intercepte (le programme ne
  le voit pas), note la pile de tous les fils et continue. `benches/gdb_top.py`
  agrège : temps propre, inclusif, par fil, par fenêtre de temps ; il compte à part les
  fils en attente (futex, condvar, park). `ptrace_scope` 1 suffit, le programme est
  l'enfant de gdb. Deux pièges rencontrés : `handle SIGALRM noprint` implique
  `nostop` (utiliser `stop print nopass`), et le pid à viser est l'enfant de gdb, qui
  met plusieurs secondes à charger les symboles.

```bash
CARGO_TARGET_DIR=~/lucivy_bench/target-prof CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth --no-run
benches/gdb_sample.sh samples.txt 0.25 400 -- <binaire> v3_ground_truth_demo --ignored --nocapture
python3 benches/gdb_top.py samples.txt --inclusive --top 60
python3 benches/gdb_top.py samples.txt --threads
```

- **Les compteurs du code** : `LUCIVY_VERBOSE=1` imprime par commit les recherches
  dans le dictionnaire (`[dictionary] commit: … lookups (… from the cache, … in a
  generation, … pending, … minted, … skipped by the filter): … ms, of which fst …,
  lock …`), les replis (`[dictionary] fold: …`), la finalisation de chaque segment
  (`[finalize] field 2: sfx build … ms`). Horodater chaque ligne (`python3 -c` qui
  préfixe `time.time()`) donne la chronologie sans profileur.

## 2. La leçon de méthode, d'abord

Mes premières références à 10 000 fichiers disaient 6,0 s. Après la réécriture du
collecteur : 4,7 s, « −21 % ». Un A/B propre par `git stash` a donné **4,8 s pour
l'ancien code aussi** : les 6,0 s étaient mesurés pendant qu'une compilation tournait
en fond (CPU 30,6 s au lieu de 23,5 pour le même binaire). Le mémo « sur machine
chargée, on mesure la charge » vaut pour mes propres tâches de fond. Règle appliquée
ensuite : **avant et après dans le même état, et l'ancien binaire rebâti** (attention :
`Cargo.lock` n'est pas suivi par git, un `git stash` qui le nomme échoue en bloc).

La réécriture du collecteur (table d'internement sans clé allouée, `into_data` sans
`BTreeMap` de clés formatées ni clones) est donc **neutre en temps**. Elle est gardée
comme simplification : la clé d'internement n'est plus stockée (le texte vivait deux
fois), plus de `BTreeMap`, et en mode v3 les ordinaux sont ordonnés par (texte, forme)
numériquement au lieu de l'ordre lexicographique d'un nombre décimal.

## 3. Ce que la chronologie a montré

À 10 000 fichiers (commit tous les 2 000), le profil par fil sur 40 échantillons :

- le fil principal passe **79 % du temps dans `commit`**, à attendre ;
- pendant ce temps le pool a ~4 fils occupés sur 24 : le flux (8 fils dans
  `add_value`) et la finalisation des segments (8 fils, puis une queue à 1-2)
  **alternent au lieu de se recouvrir** ; un commit est un arrêt du monde ;
- CPU : `add_value` 45 % dont `lookup_or_mint` 25-35 % (marche des parents dans la
  FST du dictionnaire 17 %), `finalize` 32 % dont `into_data` 21 %, désallocation 12 %.

Au noyau entier (commit tous les 10 000), les traces horodatées :

- **à partir du 3ᵉ commit, chaque commit replie le dictionnaire de façon synchrone**
  : 18 à 41 paires en attente, au-delà du plafond `LUCIVY_DICT_MAX_PENDING` = 16 ;
  1,8 à 2,9 s par commit, **10,2 s** pour celui qui compacte, 25 s en tout, plus 6 s de
  « nommage » — 31 s des 97 sur le chemin sériel ;
- les recherches dans le dictionnaire : **68,6 M de lookups**, 46,9 M de marches
  FST (un texte qui existe est cherché partie par partie), **4,1 µs la marche, 190 s
  de CPU** sur les 8 fils collecteurs ; 32,9 % de mintages, presque tous filtrés par le
  Bloom ; 57,9 % trouvés en génération, 9,2 % en attente ;
- le flux passe de 0,3 ms/doc à 30 000 fichiers à 0,7 ms/doc au noyau : plus de
  générations et de paires à traverser par recherche.

## 4. Ce qui a été fait, et ce que ça donne

| étape | 30 000 fichiers | noyau entier |
|---|---|---|
| avant | 19,5-20,0 s | 97,4 s |
| plafond de paires en attente 16 → **64** (`LUCIVY_DICT_MAX_PENDING`) | — | 75,9 s |
| + cache partagé des ids trouvés, vérifié | **14,2-14,8 s** | 65,6 s |
| + compaction et replis en pipeline sans allocation par clé (§5 bis) | — | 57,2 s |
| + 16 fils d'indexation à budgets par fil constants (§5 ter) | — | **48,2 s** |

Les temps du noyau comptent 3,4 s de persistance du harnais (copie RAM → disque).

1. **Plafond 64.** Un commit de 10 000 fichiers nomme 18 à 41 paires ; à 64 les
   replis restent en fond et les recherches ne sont pas plus lentes (fst 190 s de
   CPU dans les deux cas — les paires supplémentaires ne coûtent rien de mesurable).
   Sur WASM le repli est synchrone de toute façon (`LUCIVY_DICT_SYNC_FOLD`).
2. **Le cache partagé** (`dictionary::LookupCache`) : une table fixe de paires
   atomiques (hash de champ + clé d'internement → id), deux slots par hash, lue et
   écrite en `Relaxed`, **sans verrou**. Deux écrivains peuvent déchirer une paire, un
   texte récent en évince un ancien : sans importance, parce qu'un hit n'est utilisé
   qu'après `SfxDictionary::verify`, qui relit texte et forme de cet id dans
   `.termtexts`. **Le cache propose, le fichier décide** ; l'exactitude vient de la
   vérification, pas de la structure. 64 Mo par index en natif (4 M de slots, pages
   touchées au remplissage), 4 Mo en WASM. Ids stables à travers les replis : rien à
   vider. Résultat au noyau : 28,1 M de hits sur 39,7 M de textes trouvés en
   génération (71 %), marches FST 190 → 87 s de CPU.
   - Un cache **par fil** collecteur, essayé d'abord, ne voyait que 30 % des
     répétitions : le premier hit d'un texte sur chacun des huit fils marchait encore.
   - Le cache partagé **à verrous** avait été mesuré et refusé le 6 septembre ; c'est
     le verrou qui coûtait, pas le partage.
3. **Compteur d'ids atomique** (`fetch_add` par champ au lieu d'un mutex global par
   mintage) : neutre en temps mesuré, gardé parce que plus simple.
4. Essayé et retiré : trier les parties du dictionnaire « la plus grosse d'abord »
   (fst 22,8 → 26,3 s à 30k — la localité compte : les textes qu'un segment réutilise
   sont dans les parties les plus récentes) ; 64 stripes au lieu de 16 (rien).

## 5. Ce qui reste, par taille

Chronologie du noyau après (57,2 s) : flux jusqu'à 53,9 s pour 100 000 fichiers
(la compaction de 6 générations, 3,0 s, tourne en fond pendant les derniers
commits), derniers replis 0,5 s, persistance 3,6 s.

1. **La compaction** : faite, §5 bis ; son plancher est l'insertion dans la FST de
   sortie.
2. **Les recherches** : encore 166 s de CPU (87 fst, 26 verrou-et-travail sous
   verrou de mintage, le reste Bloom et vérification). La marche restante décode
   linéairement les groupes de parents d'une clé (jusqu'à 35 000 parents pour un
   texte fréquent) : un index de groupes à largeur fixe dans le record (format 9)
   en ferait une recherche binaire. Les 21,7 M de mintages coûtent chacun une
   allocation de clé et une insertion sous stripe.
3. **La finalisation d'un segment** (0,5-0,65 s pour 256 documents) enchaîne ses
   nœuds (préparation, FST, postings) en ligne sur le fil de la tâche ; deux tâches
   du scheduler raccourciraient la queue de chaque commit.
4. **L'arrêt du monde au commit** : le harnais commite depuis un seul fil, donc rien
   ne recouvre la finalisation. Fermer les segments par taille avant le commit
   (`WRITER_HEAP`) recouvre déjà en partie au noyau ; à 2 000 documents par commit,
   tout tombe sur le commit.
5. **La persistance** du harnais (3,4 s) n'est pas le moteur.

## 5 bis. La compaction, faite dans la foulée (13 septembre, après-midi)

Reproduite hors moteur (`lucivy_core/tests/bench_dict_compaction.rs`, ignoré : les
fichiers `dict-*` d'un index, en liens symboliques, quatre générations du noyau,
611 Mo, 5,85 M de clés dont 953 k tenues par plusieurs générations, 9,8 M de
textes) : **8,4 s**, toute la passe FST — la passe textes (1,2 s) tourne à côté.

| étape | temps | sortie |
|---|---|---|
| avant | 8,4 s | — |
| plus de tri-dédoublonnage avant l'encodeur (ids disjoints entre parties, chaque partie déjà dédoublonnée, l'encodeur retrie) | 5,3 s | identique à l'octet |
| pipeline union → encodage → écriture, un fil chacun | 5,05 s | identique |
| lectures verbatim déplacées dans l'encodage, deux encodeurs en alternance (ordre gardé) | 4,6 s | identique |
| **arènes par lot** au lieu d'une allocation par clé et par record | **2,6-2,7 s** | identique |

**Sur le noyau entier** : la compaction de 6 générations (5,8 M de clés, 12,5 M de
textes) passe de 10,5 à **3,0 s**, et les replis — même code — de 0,95 à 2,8 s au
lieu de 1,7 à 3,5 ; l'indexation complète **65,6 → 57,2 s** (3,6 s de persistance du
harnais compris, donc ~54 s de moteur, contre 94-97 le matin).

Ce que le profil a appris en route : avec une allocation par clé, les fils passaient
**27 % du temps dans les verrous de malloc** (`__lll_lock_wait/wake` : alloué sur un
fil, libéré sur un autre) — le pipeline ne gagnait rien tant que ça restait. Après
les arènes, les trois étapes sont équilibrées (union 1,0 s, encodage 2,1 s sur deux
fils, écriture 2,0 s) et **l'insertion dans la FST de sortie est le plancher** (5,8 M
de clés, ~0,35 µs chacune). Sur WASM, le chemin reste séquentiel (mêmes fonctions,
appelées à la suite).

Essayé, gardé en variable, pas en défaut : un registre de nœuds plus grand dans le
constructeur de FST (`LUCIVY_FST_REGISTRY`, 10 000 par défaut) — à 4 M, la FST de
cette génération passe de 75 à 43 Mo (−7 % du `.sfx`) mais la passe de 5,0 à 8,0 s ;
la FST n'est qu'un sixième du `.sfx` (les parents en font 364 Mo). À reconsidérer
sous l'angle taille.

## 5 ter. Les fils d'indexation (13 septembre, soir)

Le flux tournait sur 8 fils pour 24 cœurs (`MAX_NUM_THREAD` = 8). Cinq runs du noyau,
pic RSS relevé par `VmHWM`, nombre de segments relevé parce que **la forme des
segments décide du parallélisme à la requête** (remarque de Lucie : moins de segments,
moins de fils au prescan) :

| fils | budgets | temps | segments | pic RSS |
|---|---|---|---|---|
| 8 | 25 Mo + 128 Mo SFX par fil | 58,8 s | 263 | 14,3 Go |
| 12 | SFX total fixe (1 Go) | 52,5 s | 402 | 12,6 Go |
| 16 | SFX total fixe (1 Go) | 52,6 s | 541 | 12,2 Go |
| 12 | par fil constants | 52,7 s | 296 | 14,1 Go |
| **16** | **par fil constants** | **48,5 s** | **308** | **14,2 Go** |

À budget total fixe, plus de fils = segments plus petits : −11 % de temps pour ×2
de segments, un changement de forme, pas un gain. À budgets par fil constants,
16 fils font **−17,5 %** pour +17 % de segments et le même pic. Le panel de
requêtes rejoué sur l'index à 308 segments contre celui à 263 : mêmes comptes,
temps dans le bruit (`mutex_lock` 14,1-14,4 contre 15,3 ms, `de` 598-603 contre
585, regex 222-258 contre 222), +25 Mo (0,5 %).

**Défauts changés** : `MAX_NUM_THREAD` 8 → 16 (le nombre de fils reste
`min(cœurs, 16)`, donc inchangé sur une machine à 8 cœurs ou moins) ; le tas
d'écriture et le budget SFX sont désormais **par fil** (`WRITER_HEAP_PER_THREAD`
25 Mo, 128 Mo de SFX), ce qui garde la forme des segments quand les fils suivent
les cœurs ; WASM inchangé (un fil, 15 Mo, 128 Mo). Noyau avec les défauts : **48,2 s**,
308 segments, aucun repli synchrone. Pourquoi pas linéaire : le flux n'est pas borné
que par les collecteurs (verrous de mintage, un seul fil qui alimente dans le harnais,
arrêt du monde au commit).

## 5 quater. Où en sont les recherches à 16 fils, et la suite

Compteur ajouté (`LUCIVY_VERBOSE`, `parents decoding`) : sur le noyau à 16 fils,
203 s de CPU de recherches dans le dictionnaire, dont marches FST 98 s, **dont
balayage linéaire des groupes de parents 52,7 s** — ~3,3 s de mur, 6 % du total —,
et travail sous le verrou de mintage 42 s (26 à 8 fils ; 37 avec 64 stripes au lieu
de 16 : c'est le travail sous le verrou qui compte, pas l'attente).

**Prochaine pièce, à faire par défaut et sans changer le format 8** : un fichier
dérivé `dict-<g>.<champ>.pidx` — pour chaque record groupé au-delà d'un seuil, une
table à largeur fixe (recouvrement sur 2 octets, offset du groupe, premier ordinal)
qui rend la recherche du groupe binaire au lieu de linéaire. Écrit par les replis et
compactions (l'étape d'encodage voit chaque record), ignoré par un ancien lecteur,
reconstruit en RAM par un nouveau lecteur quand il manque (un index existant marche
tel quel et se convertit au fil de ses compactions). Gain attendu : ~3 s sur 48.
Après lui : le mintage sans `String` par clé, puis le coût par document des
collecteurs eux-mêmes, jamais profilé au-delà de `add_value`.

## 5 quinquies. Mis en valeur (13 septembre, soir)

- **Comparatif régénéré** sur le noyau épinglé, index lucivy rebâtis (Elasticsearch
  et tantivy réutilisés/rebâtis par le script) : les quatre dispositions indexent en
  **46-51 s** (94-112 le matin), tailles et temps de requête inchangés ;
  `docs/compare-engines-2026-09-13.md`, tableau du README, article (« Mine takes
  fifty »), page.
- **Linux 2.6.0** (le tableau « navigateur contre natif ») : natif **9,1 s**
  (23 s le 5 septembre), 896 Mo, requêtes égales ; navigateur (`index linux`,
  un fil, commit tous les 8 Mo) **35 s** (41 s), 1 089 Mo, mêmes comptes ; requêtes à
  chaud 5-21 / 8-9 / 6 / 20-21 / 257-263 / 103-107 ms. Le corpus natif vit dans
  `~/lucivy_bench/linux-2.6.0/linux` (extrait de `playground/corpus-linux-2.6.0.tar.gz`).
- **4.2.0 préparée** : numéro partout, CHANGELOG daté, « What's new in 4.2 » dans
  les cinq README (le README racine est celui de PyPI), architecture (indexation
  4.2, résultats 4.2). Le tag et la PR vers `main` attendent le feu vert.

## 5 sexies. Le `.pidx`, fait (13 septembre, nuit)

La pièce du § 5 quater, mesurée avant d'être dessinée. Sur la plus grosse génération
du noyau (`dict-10.2.sfx`, 527 Mo, table des parents 467 Mo — balayée en 0,43 s par
`measure_grouped_records`, test ignoré de `file_v3.rs`, `SFX_FILE=…`) :

| | records | groupes | parents | dont à sti 0 |
|---|---|---|---|---|
| plats (≤ 32 parents) | 5 614 219 | — | 15,1 M | 6,2 M |
| groupés | **167 556 (2,9 %)** | **15,05 M** | **58,1 M (79 %)** | 6,3 M |
| dont 16-63 groupes | 94 123 | | 5,3 M | 1,5 M |
| dont 64-255 groupes | 57 857 | | 11,6 M | 3,0 M |
| dont 256 groupes et plus | 10 306 | | 40,7 M | 1,9 M |

Un groupe fait 3,9 parents en moyenne : ce n'est pas le décodage du groupe voulu qui
coûte, c'est la lecture des en-têtes de tous ceux d'avant (un record de 1 460 groupes
en moyenne dans la dernière ligne, et ce sont les textes les plus fréquents). La
table « un point par groupe » du § 5 quater aurait fait 15 M × 13 octets = 195 Mo
par génération : refusée. **Le `.pidx` est un index à points de contrôle** : pour
chaque record de plus de 16 groupes, un point tous les 16 groupes (recouvrement,
position de l'en-tête dans le record, premier ordinal du groupe précédent — l'en-tête
porte un delta), 13 octets ; les records indexés par offset de table, triés. Une
recherche : dichotomie sur les records, dichotomie sur les points, puis au plus 16
en-têtes, et **arrêt au premier recouvrement dépassé** (les groupes sont triés — le
chemin sans fichier s'arrête aussi maintenant, `decode_parent_entries_v8_overlap`).
11,5 Mo pour cette génération (2,5 % du `.sfx`), 26 Mo sur les 5 078 de l'index du
noyau (+0,5 %).

Où il vit : `suffix_fst/dictionary_pidx.rs`. Écrit par `merge_sfx` dans l'étape
écrivain (elle voit chaque record et son offset ; seuls les records groupés de plus de
16 groupes lui coûtent une marche de leurs en-têtes : compaction 2,3-2,6 s, inchangée)
et par `write_generation` ; jamais pour les paires `.newsfx` (petites, éphémères).
Lu par `SfxDictionary::open` (`open_parts_indexed`) ; une partie sans fichier — une
génération d'avant, une paire — le **reconstruit en RAM** depuis sa table
(`GroupIndexBuilder::build_from_table`, sans marche FST). Le lookup
(`lookup_with_key`) passe par `SfxFileReaderV3::parents_with_overlap`. Un ancien
lecteur ignore le fichier : le format 8 ne bouge pas, un index 4.3 s'ouvre en 4.2.

**Le cycle de vie du fichier**, la remarque de Lucie : un fichier de plus dans une
génération se déclare partout où la génération est énumérée. Source unique
désormais : `dictionary::GENERATION_EXTENSIONS` = `sfx`, `termtexts`, `pidx` —
`SfxDictionaryMeta::files_of` (donc l'inventaire du GC, `list_files`, et
`dictionary_files` des snapshots), `remove_leftovers`, `generation_bytes` (le choix
des compactions), le banc. Le GC gardait déjà tout `dict-` d'une génération vivante ;
les fichiers d'une génération retirée sont des fichiers gérés hors inventaire, il les
efface, `.pidx` compris. `sync.rs` et `snapshot.rs` tolèrent son absence (une
génération d'avant 4.3).

**Mesuré, même état de machine, binaire 4.2.0 rebâti dans un arbre à part
(`~/lucivy_bench/wt-4.2`, `CARGO_TARGET_DIR=~/lucivy_bench/target-ab`)** :

| | 30 000 fichiers (×2 chacun) | noyau entier |
|---|---|---|
| mur | 12,0-12,1 → 11,8-12,2 s | **48,2 → 47,4 s** |
| CPU de recherches (16 fils) | 32,7-31,5 → 30,0-30,4 s | 196 → 173 s |
| dont marches FST | 9,3 → 7,4 s | 96 → 69 s |
| dont décodage des parents | 3,8 → 2,2 s | **52 → 23 s** |
| segments | 240 | 308 |
| `de` strict / `lock` relâché | | 534 / 77 → 554 / 78 ms, mêmes comptes et spans |

Sortie de compaction identique à l'octet à la 4.2.0 (sha256 des `.sfx` et
`.termtexts` sur les quatre générations du banc). Ce qui reste dans les 23 s : le
chronométrage lui-même (deux `Instant::now` par lookup, 70 M de lookups), les records
plats et ceux de 16 groupes ou moins, le décodage du groupe voulu. Le mur ne gagne
que 0,8 s : les recherches tournent sur les 16 fils, et ce qui borne le mur reste
l'arrêt du monde au commit et la finalisation en ligne (§ 5).

**Navigateur** (la règle : tout changement du chemin d'indexation se rejoue sur 10 000
fichiers dans Chrome ; ici le chemin séquentiel wasm de `merge_sfx` écrit le fichier, et
`SfxDictionary::open` le lit ou le reconstruit) : `?corpus=corpus-kernel-10k.tar.gz&commitmb=2`
(commits rapprochés, 276 replis, compactions) — 10 000 fichiers indexés, 1 051 Mo en
mémoire, pic 2 125 Mo, **12 `.pidx` pour 12 générations dans chacun des 4 shards** de
l'OPFS, `mutex_lock` 968 documents (le compte de l'après-midi), 1 955 spans lus à l'octet
(1 759 `mutex_lock` exacts, le reste des séparateurs relâchés : `mutex *lock`, `mutex
lock`) ; le premier `mutex_lock` a attendu 26 s de fusions de fond, comme noté le soir.
Puis l'index **Linux 2.6.0 écrit par la 4.2.0** (18 générations par shard, aucun `.pidx`)
rouvert par l'onglet : 14 032 documents, `mutex_lock` 88 documents, 145/145 spans, et
un `grep -rliaE 'mutex[^a-z0-9]*lock'` natif sur les mêmes 14 032 fichiers en trouve 87 —
le 88ᵉ (`net/ipv4/ipvs/ip_vs_ctl.c`) est `mutex);\n\n/* lock`, une occurrence à cheval
sur deux lignes que `grep` ne peut pas voir : 88 est le bon compte. Enfin le 10 000 par
défaut (`commitmb` 8) : indexé et rouvert en ~45 s depuis le chargement de la page (37-43 s
d'indexation seule l'après-midi), 1 063 Mo, pic 1 654 Mo, 968 documents.

Vérité : `dictionary_pidx` (index = balayage sur sept formes de records, construction
incrémentale = construction depuis la table, octets étrangers refusés),
`streamed_merge_equals_the_rebuild` (le `.pidx` des deux chemins, égal à une
construction depuis la table), `reopened_without_group_index_rebuilds_it`
(`test_dictionary_index` : `.pidx` supprimés, réouverture, réindexation sans minter,
le repli suivant écrit le sien), `bench_dict_compaction` (`DICT_KEEP=1`, sha256).

## 5 septies. Le chemin sériel du commit (13 septembre, nuit) — 48,2 → 39,9 s

**Le profil, autrement.** 400 échantillons gdb du noyau entier (16 fils d'écriture,
24 fils de pool), lus non plus par fonction mais **par fil et par instant**
(`benches/gdb_timeline.py`) : le pool oscille entre des plateaux à 16-24 fils occupés
(collecte + finalisation) et, à chaque commit, une vallée de 2 à 2,5 s à **un seul fil
occupé**, 45 échantillons sur 191 ; moyenne 12,4 fils occupés sur 24. Le fil du harnais
attend dans `commit` tout du long. Ce que fait le fil seul dans les vallées :
`forget_pending` (le `retain` de la table des textes en attente, à clés `String`, et
la libération de ses millions de `String`) sous `fold_new_texts > handle_commit`, puis
des `drop` de grosses structures. Le compteur `named in` le confirmait : **6,7 s cumulés
par run**, sur le chemin où le monde est arrêté — le `.pidx` n'y avait rien changé
(6,55 s).

**Ce qui a été fait** (`suffix_fst/dictionary.rs`, `indexer/dictionary_commit.rs`,
`indexer/index_writer.rs`) :

- **La table des textes en attente par époque de commit.** `DictionaryShared.epoch`,
  tourné par `SfxDictionary::begin_commit_epoch` dans `prepare_commit`, **avant** que
  les écrivains ne vident leurs segments. Chaque stripe tient ses époques (`PendingEpoch`
  : une arène d'octets pour les clés, une `hashbrown::HashTable` d'entrées de 20 octets)
  ; une recherche parcourt les époques de la plus récente à la plus ancienne ; un
  mintage écrit dans l'époque courante. Quand le commit a nommé ses paires
  (`forget_committed_pending`), **toute époque inférieure à la courante est lâchée
  entière** : deux libérations par stripe, plus de `retain`, plus une `String` par
  texte. Un texte minté avant le tour appartient à un segment que ce commit publie ;
  un texte minté après (un écrivain qui finit ses documents pendant qu'un autre a
  déjà vidé le sien) reste jusqu'au commit suivant — oublié tard, jamais tôt.
- **Plus de `HashSet` des ids repliés** : `fold_new_texts` lisait les `.newtexts` de
  chaque nouveau segment pour en tirer 2,4 M de paires (champ, id) par commit, qui ne
  servaient qu'au `retain`. Il ne lit plus que `num_terms`.
- **Index des groupes des générations sans `.pidx` mis en cache** dans le partagé
  (`built_group_indexes`) : construit une fois par processus, pas à chaque réouverture
  du dictionnaire — et le dictionnaire se rouvre à chaque commit. Les paires n'en ont
  pas (petites, éphémères, balayées).

**Le bug que le compteur a attrapé.** Premier run : 41,6 s, mais `minted` 25,0 M au
lieu de 22,5 M — 2,5 M de textes mintés deux fois. Le hachage de l'insertion (clé
`str` : octets puis 0xFF) et celui du rehash (clé `[u8]` : longueur puis octets)
différaient ; chaque entrée déplacée par un agrandissement de table était perdue.
Une seule fonction `pending_hash` désormais, un `debug_assert` à l'insertion, et le
test `pending_texts_survive_table_growth` (200 000 textes, tous retrouvés). Règle :
**un A/B d'indexation compare aussi `minted` et `pending`, pas seulement le temps.**

**Mesuré, noyau entier, même machine, contre la 4.2.0 (308 segments des deux côtés)** :

| | 4.2.0 | `.pidx` | + époques |
|---|---|---|---|
| mur | 48,2 s | 47,4 s | **39,9 s** |
| `named in` (sériel, cumulé) | 6 731 ms | 6 549 ms | **63 ms** |
| minted / pending | 22 539 376 / 6 570 578 | idem | **idem** |
| marches FST / décodage (CPU) | 96 / 52 s | 69 / 23 s | 69 / 24 s |
| `de` strict / `lock` relâché | 534 / 77 ms | 554 / 78 | 605 / 83, mêmes comptes et spans |

**Ce que le profil dit encore** (à faire ensuite, même branche) : dans les vallées et
sur les plateaux, la libération des sorties du DAG de construction
(`drop_in_place<dyn Any>`, 101 échantillons — des millions de petits `Vec` par jeton)
; dans `add_value`, un `BTreeMap` et un `Vec<usize>` par mot et par valeur (36
échantillons), une entrée `WordStrippedEntry` à deux `String` **par occurrence** de mot
(le constructeur dédoublonne ensuite), une `String` par chunk (`extended`,
`content_overlap` — ce dernier champ de `TokenMetaV3` n'est lu nulle part). Le
`lookup_or_mint` reste 25 % du CPU occupé.

## 5 octies. Le collecteur, sans allocation par occurrence (13 septembre, nuit) — 39 → 35 s

Ce que le même profil disait du plateau (§ 5 septies, dernier paragraphe), fait :

- **Une entrée de mot par ordinal, pas par occurrence.** `word_stripped_entries`
  recevait, pour chaque mot de chaque valeur, une `WordStrippedEntry` à deux `String`
  ; le constructeur de FST les dédoublonnait ensuite. Désormais `mark_ws_entry` : la
  première occurrence d'un ordinal entre, les suivantes non (idem pour l'entrée de
  queue des mots très longs). L'estimation mémoire (`WORD_STRIPPED_OVERHEAD`) compte
  toujours chaque occurrence : le budget qui coupe les segments garde son sens, **308
  segments avant comme après**, seuls la mémoire et le travail partent.
- **Les mots d'une valeur sans `BTreeMap`** : les chunks d'un mot sont consécutifs et
  les mots viennent dans l'ordre, un mot est un intervalle d'indices de chunks
  (`word_ranges`, un tampon réutilisé) — plus un `Vec<usize>` par mot.
- **Tampons réutilisés** (`AddValueScratch`) pour le texte étendu de chaque chunk,
  le contenu du mot, son recouvrement de contenu et sa clé — plus de `format!` ni de
  `to_string` par chunk ou par mot.
- **`TokenMetaV3::content_overlap` supprimé** : une `Option<String>` calculée et
  clonée par chunk, stockée par ordinal, lue nulle part.

**Ce que le contrôle à l'octet a montré.** Un seul fil d'écriture, un seul commit,
3 000 fichiers, ancien binaire contre nouveau : dictionnaire **83/83 fichiers
identiques** ; v3, 81/85 — les quatre `.sfx` du champ contenu diffèrent, et
`diff_two_sfx_files` (test ignoré de `file_v3.rs`, `SFX_A`/`SFX_B`) dit exactement
quoi : mêmes clés, mêmes nombres de parents, mais pour 6 947 clés un parent
mot-dépouillé change de `(own_len, sep_len)` — `0x00000000` suivi de `\n\t\t` (13, 3)
ou d'une espace (11, 1). Un mot suivi de séparateurs différents est **un seul ordinal**
(`intern_shape` ignore le séparateur pour la partition 0x02), et l'ancien code poussait
une entrée par occurrence avec ses propres séparateurs : le record de la FST gardait
celle que le constructeur voyait en dernier, **en désaccord avec `.termtexts`**, qui
enregistre la première. Le record porte désormais la première, comme `.termtexts`.
Test `one_word_entry_per_ordinal_with_the_first_occurrence_shape`. (Un premier jet
coupait aussi la recherche du recouvrement de contenu au premier mot suivant même
sans contenu utilisable — 102 ids de moins sur 10 000 fichiers : la règle de toujours,
« un mot suivant dont le premier caractère ne tient pas dans `overlap` octets ne donne
rien, on passe au mot d'après », est restaurée et commentée.)

**Mesuré.** Noyau entier, alterné deux fois contre le binaire des époques : 39,7 / 38,5
→ **35,6 / 34,9 s**, `minted` et `pending` identiques, 308 segments, finalisations
cumulées 181-184 → 165-168 s, mêmes comptes de requêtes. (Une première mesure isolée
disait 45,3 s : bruit de machine — d'où l'alternance.) Mono-fil, 3 000 fichiers : v3
2,6 → 1,8 s, dictionnaire 2,8 → 2,4 s. Panels de vérité terrain 10/10 dans les trois
dispositions. **Chrome, 10 000 fichiers** : panel de parité de 21 requêtes contre les
rapports du 11 septembre — dictionnaire 20/21 identiques et la 21ᵉ un ex æquo à la
coupure du top-10 (comptes, scores et spans égaux), sans positions **21/21** ; rapports
gardés à côté des références (`parity_10k_{pos,nopos}_collectors.json`).

## 5 octies bis. Les textes en attente avant les parties (13 septembre, nuit)

Le profil du § 5 septies laissait `lookup_or_mint` à 32 % du CPU occupé, dont les
marches FST 69 s. Un texte minté depuis le dernier repli n'est dans aucune partie, et
un lookup marchait **toutes** les parties (jusqu'à 8 générations et 64 paires) avant
d'interroger la table en attente : 6,6 M de lookups du noyau. Désormais, quand le Bloom
dit « peut-être », une sonde de la table sous le verrou de sa stripe d'abord, puis les
parties. Noyau entier : marches FST 68 → 38 s de CPU, décodage 23 → 13, lookups 175 →
157 s, verrou 34 → 42 (une sonde de plus par lookup), mur dans le bruit (34,9 → 34,5 s :
le plateau est borné par les écrivains, le CPU libéré profite aux finalisations).
Le compteur `pending` monte de 6,57 à 7,96 M : il compte maintenant aussi les entrées
périmées qu'un repli n'a pas encore oubliées (même id que la partie).

## 5 nonies. Un défaut de forme attrapé par l'expérience des 24 fils (13 septembre, nuit)

`LUCIVY_WRITER_THREADS=24` sur le noyau : 36,3 s contre 36,1 à 16 (rien à gagner, les
cœurs sont déjà pris par les finalisations), 362 segments — **et `de` strict perd 3
spans sur 7 929 772**, les mêmes sur deux runs, tous des occurrences répétées de
l'aiguille dans un jeton (`0xde|de|de00`, `videodev`). La 4.2.0 publiée, rebâtie à
côté, les perd aussi à 24 fils ; à 12 et 16 fils, rien ; 30 000 fichiers à 24 et 32
fils, rien. Le jeton est bien indexé (`dedede00` strict : 9 spans exacts) : c'est la
requête courte, qui avale des milliers de clés, qui perd.

Cause : `keep_in_segment` (`briques/fst_walk.rs`), la coupe d'une liste de candidats
du shard par le `.gmap` d'un segment, a trois stratégies selon les tailles ; celle
d'un segment qui tient **peu des ids de la liste** galopait chaque id du segment dans
la liste et gardait **un seul** item par id — or une liste tient un item par
(ordinal, suffixe), et un jeton qui contient l'aiguille trois fois, c'est trois items
d'un même id. La branche prise dépend de la taille du segment face à la liste : d'où
la forme. Depuis la coupe au `.gmap` (5 septembre), donc dans 4.0.0 à 4.2.0. Corrigé
(tous les items d'un id répété), test `keep_in_segment_keeps_every_item_of_a_repeated_id`
sur les trois branches (rouge sur l'ancienne), et sur l'index à 24 fils : `de` strict
exact, `de` relâché **miss 33 → 0**, `e` strict 60,5 M de spans exacts.

Restait sur ce même index, **sans rapport avec la forme** : `de` relâché rendait **6
spans en trop**, tous un `d` suivi de séparateurs puis d'un caractère non ASCII
(`D\n,,“`, ``d`` 文``, `d\n\n取`), finissant **dans** le caractère. Reproduit en isolation
(`test_relaxed_multibyte`, les trois formes du noyau et leurs jumelles ASCII, quatre
dispositions) et tracé avec `V3_DIAG_LITERAL=de` : la chaîne `D` → `“underscan”` avec le
`e` pris à l'intérieur du mot suivant. Deux causes :

1. **Le recouvrement de contenu d'un mot** (`content_overlap`, les deux premiers octets
   du mot suivant, ce qui permet à une requête relâchée de franchir la frontière) était
   pris sur le mot **d'après** quand le premier caractère du mot suivant ne tenait pas
   en deux octets (`“` fait trois) — l'entrée `den` affirmait une adjacence `D`/`EN` que
   le texte n'a pas. C'est la règle que j'avais rétablie au § 5 octies pour rester
   identique à l'octet : elle était fausse. Le recouvrement est désormais vide dans ce
   cas, comme pour le dernier mot d'une valeur (`add_value` et le chemin de fusion).
2. **Les candidats ancrés de la partition mot-dépouillé** (`fst_candidates_v3` avec
   `anchor_start`) rendaient tous les parents des clés sous la requête, or ces clés
   tiennent chaque suffixe du contenu d'un mot : un parent à `sti` > 0 dit que la
   requête est *dans* le mot, pas à son début. Une chaîne dont la position précédente
   n'avait pas de recouvrement pour vérifier le reste (le cas 1, mais aussi tout mot en
   fin de valeur) continuait donc dans n'importe quel mot **contenant** le reste
   (`underscan` contient `e`). Filtrés à `sti` 0 (`anchor_stripped`).

Le correctif 2 suffit côté requête sur un index existant ; le 1 corrige ce qui est écrit.
Vérité : `test_relaxed_multibyte`, panels 10/10, `de` relâché exact sur le noyau à 24 fils.

## 6. Vérification

- `cargo test --release --lib` : 1 471 verts (22 ignorés) ; sans features par
  défaut : 1 437 ; `lucivy-core` : toutes les suites vertes (dont `test_positions_off`,
  `test_compat_308`, fédéré, filtré, LUCE) ; C++ : 19 ; clippy : 0 erreur.
- Vérité terrain à 10 000 fichiers du noyau épinglé, panel de 10 requêtes (littérales
  strict et relâché, mot entier, préfixe, fuzzy d1 et d2, Jaro-Winkler, regex) :
  **10/10, spans exacts**, dans les trois modes — dictionnaire, sans positions, v3
  (celui dont l'ordre des ordinaux a changé).
- Navigateur (WASM rebâti) : playground `?corpus=corpus-kernel-10k.tar.gz`,
  dictionnaire par défaut, 10 000 fichiers indexés (index 1 103 Mo en mémoire, pic
  2 003 Mo), `mutex_lock` 968 documents comme avant le chantier, 968/968 spans exacts
  à l'octet sur le contenu rendu, 500 hits sans champs en 16 ms.
- Navigateur, compaction : playground `?corpus=corpus-kernel-10k.tar.gz&commitmb=2`
  (commits tous les 2 Mo, 60 générations, 36 compactions de 6 générations en chemin
  séquentiel WASM, ~500 ms chacune) : 10 000 fichiers indexés, `mutex_lock` 968
  documents, 968/968 spans exacts à l'octet, `sched` 1 910. **À regarder** : la toute
  première recherche après cette indexation a mis 26,9 s (fusions de fond des ~240
  petits segments encore en cours), les suivantes 260 ms ; sans rapport avec la
  compaction, qui est synchrone au commit en WASM et finie avant « indexed ».
- **Les temps de requête ne bougent pas** (question de Lucie) : même binaire, panel
  du comparatif (huit requêtes) sur l'index du noyau bâti le matin et sur celui du
  soir — mêmes comptes, mêmes 12 fichiers de dictionnaire, 5 056 contre 5 053 Mo,
  temps dans le bruit (`mutex_lock` 16,9 → 13,3-13,7 ms, `de` 603 → 577-611,
  regex 225 → 216-218, fuzzy 29,6 → 29,6).
- Le cache ne change aucun octet des fichiers : il ne fait que raccourcir le chemin
  vers un id que la marche aurait trouvé, et un hit non vérifié reprend ce chemin.
