# Architecture de lucivy — état au 5 septembre 2026 au matin

Rappel écrit pour être lu seul, mis à jour après la journée de réduction
d'index et la nuit du dictionnaire partagé (branche `v4`). Il remplace
`docs/28-08-2026/08-architecture.md` pour tout ce qui concerne les
formats ; le reste (crates, DAG, sharding, fédération) y est repris tel
quel parce qu'il n'a pas bougé. Le §2.5 est nouveau : le mode dictionnaire.

---

## 1. Les crates

```
ld-lucivy        le moteur : index, requêtes, scoring, merger, segments, SFX
  ├── lucivy-fst           FST (fork de `fst`), OutputTable
  ├── ld-lucivy-* 0.27.0   fork tantivy vendorisé (columnar, stacker, sstable…)
lucivy-core      ShardedHandle, query builder, tokenizers, snapshot/delta,
  │              blob store, le DAG de recherche
  ├── luciole              acteurs / DAG / scheduler, compatible WASM
  └── lucistore            BlobStore, ShardStorage, snapshot, sync
sparse-vector    index sparse (postings + WAND), crate ami, sur lucistore
```

Cinq crates portent le numéro de version commun (`luciole`, `lucistore`,
`ld-lucivy`, `lucivy-core`, `sparse-vector`), publiés dans cet ordre.
Bindings : Python (PyO3), Node (napi), C++ (cxx), WASM (emscripten),
`lucivy_fts` (bridge rag3db). Publication par tag, en trusted publishing
partout depuis 3.0.6/3.0.8 ; **3.0.8 est la dernière publiée**, et un
binaire 3.0.x ne lit pas un index v4.

---

## 2. Le modèle d'index SFX v3, tel qu'il est écrit ce soir

### 2.1 Tokenisation

Un mot = une suite de caractères de contenu (`is_content_char` : non-ASCII
ou alphanumérique) suivie de ses séparateurs. Chaque mot est découpé en
**chunks de ≤ 8 octets** (`DEFAULT_MAX_TOKEN`), le dernier portant le
séparateur de queue. Le collecteur étend chaque chunk des **2 premiers
octets du chunk suivant** (overlap) : texte étendu = contenu + sep +
overlap ; `own_len` = contenu + sep.

Deux familles d'entrées, dans un seul espace d'ordinaux par segment :
- les **chunks** : partitions FST `0x00` (suffixe à si = 0) et `0x01`
  (si > 0), postings dans `.sfxpost` ;
- les **mots dépouillés** (`0x02`) : contenu du mot entier sans séparateurs
  + overlap de contenu du mot suivant, postings dans `.word_sfxpost`.
  C'est ce qui fait `rag3weaver` ↔ `rag3_weaver` en séparateurs relâchés.

### 2.2 Fichiers par segment et par champ (format du 4 septembre)

| fichier | contenu | encodage actuel |
|---|---|---|
| `.sfx` | FST des suffixes (clés minuscules, préfixe de partition, **coupées à la frontière du token**) + table de parents | conteneur `SFX3`, **version 8** (nuit) : la valeur FST est l'offset du record de la clé, un parent comme cent ; record plat jusqu'à 32 parents (par parent : Δordinal, sti en varint, flags = ws + longueur d'overlap + sep_len sur 3 bits, **octets d'overlap** ; `own_len` dérivé de la longueur de la clé, écrit seulement si la minuscule a changé la longueur), groupé par overlap au-delà (saut par groupe, `decode_parents_where`). Le décodage reçoit la clé. Versions 3 (11 o/parent), 4 (u64 packé), 5 (delta-varint, parent unique inline 63 bits) et 6 (tout en table, overlap dans la clé) encore lues, 7 (intermédiaire) refusée ; `keys_cut_at_boundary()` choisit le chemin de marche |
| `.sfxpost` | postings de chunks : par ordinal, (doc, position, byte_from, byte_to − byte_from) | **`SFP4`** (nuit) : blocs `SFP3` (varints, checkpoints par 8 docs) derrière une table d'offsets par blocs (`block_offsets.rs`, 1-2 o/ordinal) ; `SFP3` (table u32) encore lu |
| `.word_sfxpost` | postings de mots : (doc, first_pos, last_pos, byte_from, to − from) | **`WSP4`** : blocs `WSP3` (varints, checkpoints par 32), table par blocs, offsets relatifs ; `WSP3` encore lu |
| `.termtexts` | ordinal → texte étendu (casse d'origine) + méta | `TTX3` **layout 3** (nuit, section `0x05`) : offsets de texte par blocs, puis 4 octets de méta par ordinal (`u16 own_len`, `u8 sep_len`, `u8 flags` = overlap 4 bits + ws + stripped), puis les textes ; STATS (max word). Layouts 2 (8 o/ordinal, offset + méta) et 1 (TEXTS + META à part) encore lus |
| `.posmap` | (doc, position) → ordinal de chunk | **`PMP3`** : 3 octets par position, vide = 0xFFFFFF ; `PMAP` (4 o) encore lu |
| `.word_pos_map` | (doc, position) → ordinal de mot qui commence là \| span | **`WMP3`** : 4 octets par position, ordinal 28 bits \| span 4 bits (15 = « demander aux postings ») ; `WMP2` (24 \| 8) encore lu |
| `.sibling_v3` | ordinal → ordinaux qui le suivent dans le texte | **`SIB4`** : un varint de delta par lien, table d'offsets par blocs ; `SIB3` (table u32), `SIB2` (avec gap) et v1 encore lus |
| `.store` | docstore LZ4 (16 Ko) | tantivy |
| `.term`, `.idx`, `.pos`, `.fast`, `.fieldnorm` | index inversé tantivy (BM25, `more_like_this`) | tantivy |

Disparu : **`.bytemap`** (la question « ce chunk a-t-il un octet de
contenu ? » est `own_len > sep_len` dans la méta de `.termtexts`,
`TermTextsReaderV3::has_content`). `.gapmap`, `.sepmap`, `.sibling` sont v2.

### 2.5 Le mode dictionnaire (`sfx_version` 4, `suffix_fst/dictionary.rs`)

Un index créé avec `"sfx_version": 4` (`SchemaConfig`) a **un dictionnaire
par shard** au lieu d'un par segment : le moteur v3, les mêmes clés et
formats, sur des **identifiants globaux au shard**.

| fichier | portée | contenu |
|---|---|---|
| `dict-<g>.<champ>.sfx` | shard, génération `g` | FST des suffixes + parents (conteneur 8) des identifiants frappés par cette génération ; ordinaux = identifiants globaux |
| `dict-<g>.<champ>.termtexts` | shard, génération `g` | ses entrées → texte étendu + méta (layout 3) ; section **IDS** (`0x06`, plages) : quel identifiant est chaque entrée |
| `<seg>.<champ>.gmap` | segment | identifiants globaux de ses ordinaux locaux, **triés** (index = ordinal local ; global → local par recherche binaire) |
| `<seg>.<champ>.newtexts` | segment, **hors registre** | textes et méta des identifiants que le segment a frappés ; consommé et supprimé par le commit suivant |
| `.sfxpost`, `.word_sfxpost`, `.posmap`, `.word_pos_map`, `.sibling_v3` | segment | **inchangés**, à ordinaux locaux |

`meta.json` porte `sfx_dictionary { generations (vivantes, croissantes),
next_generation, next_ids (par champ), field_ids }` ; c'est par là que le
GC (`segment_updater::list_files` : les vivantes, plus toute génération
plus récente que la plus récente vivante), le snapshot LUCE, le delta (un
bundle `dict-<g>.` par vivante, préfixe de ses fichiers) et `Index` (qui
tient le dictionnaire ouvert et le rafraîchit à chaque lecture de
`meta.json`) connaissent les générations. Un fichier de shard non
enregistré là serait supprimé au premier GC. Les lecteurs voient les
générations comme un fichier (`SfxFileReaderV3::open_parts`,
`TermTextsReaderV3::open_parts`).

**Indexation** : le collecteur (`SfxCollectorV3::with_dictionary`) cherche
chaque texte nouveau dans la génération courante (clé minuscule sous la
partition, parent à `sti` 0 de même forme, texte exact confirmé — la clé ne
voit pas la casse), puis dans les textes en attente du shard, et frappe
sinon sur le compteur du champ. Compteur et attente sont partagés par les
générations d'un processus (`DictionaryShared`), le collecteur lit la
génération **courante** à chaque recherche (le slot de l'`Index`). Les
ordinaux locaux sont attribués dans l'ordre des identifiants. Le segment
n'écrit ni `.sfx` ni `.termtexts`.

**Commit** (`indexer/dictionary_commit.rs::fold_new_texts`, avant le DAG de
commit) : les `.newtexts` des segments commis → une génération de plus,
**leurs textes seulement** ; au-delà de `LUCIVY_DICT_MAX_GENERATIONS` (8)
vivantes, une compaction les réunit en une, réécrite en entier. Les
identifiants ne bougent jamais : aucun segment n'est touché.

**Fusion** (`merge_segments_dict`) : union triée des `.gmap`, remappage
des locaux, concaténation des postings, fratrie et cartes rebâties. Pas de
texte, pas de FST.

**Requête** : deux phases. **Le plan** (`briques/plan.rs`, appelé au
début de `prescan_segments_more` des trois requêtes v3, donc sur tous les
chemins — index simple, shardé, par lots, fédéré) : par dictionnaire,
toutes les cellules FST que la requête demandera — candidats de la racine
par partition, marches, et pour chaque suffixe de la requête sa marche et
son compte ancré ; comptes des n-grammes et des pièces du fuzzy puis le
générateur choisi (`composite::fuzzy_generator`, la même décision que les
segments) ; littéraux requis de la regex — calculées **en parallèle** (une
tâche par cellule, priorité Critical, une vague par littéral, deux à trois
pour le fuzzy) dans la mémo du lecteur partagé (`FstMemo`, cellules à
trois états, `peek`). Personne n'attend sous ces tâches. **L'exécution**
par segment, en parallèle comme en v3 : le lecteur FST du dictionnaire est
partagé (`SegmentReader::sfx_dictionary_field`), chaque segment en reçoit
une vue (`for_segment(gmap)`) qui coupe les listes mémoïsées à ses
identifiants par **intersection en galop** (`GmapReader::lower_bound_from`
— une marche fusionnée parcourait tout le `.gmap` à chaque coupe, 80 % du
temps par segment). Un reste avalé par la dernière position d'une chaîne
n'est **pas** une liste mais un `Alts::Prefix` testé sur le texte de
`.termtexts` (texte étendu = octets propres + overlap, ce qu'une clé SI=0
couvre) ; les cinq lecteurs traduisent global ↔ local (`with_gmap`). Une
cellule non planifiée est calculée en ligne : le plan est une
optimisation, pas une condition d'exactitude (`V3_PLAN=0` le coupe).
Mesuré ([11](11-journal-chantier-plan-fst.md) §4) sur 30 000 fichiers à
froid, même binaire, min de 3 passes : ×0,8 à ×1,9 par rapport à v3
(exactes 2,5-5,3 ms contre 1,7-3,3 ; fuzzy plus rapide) ; ×2-22 le matin
même. Le mode reste optionnel tant que ×1,5 n'est pas tenu partout (cinq
requêtes sur dix le tiennent).

Mesuré : référence 10 000 fichiers 508 → 390 Mo (4 générations vivantes ;
387 en une seule), construction 19 s (8 en v3) ; 30 000 : 1 659 →
1 327 Mo ; noyau entier : 11,06 → 5,98 Go (×6,7 le texte), 255 s, 2
générations ; comptes et spans identiques partout. Vérité :
`lucivy_core/tests/test_dictionary_index.rs`.

### 2.3 Ce que le builder enregistre

Pour un chunk étendu : une clé par suffixe commençant **dans les octets
propres** (si < own_len), **arrêtée à `own_len`** — les deux octets
d'overlap sont dans le record du parent, plus dans la clé (soirée du
4 septembre, conteneur 8 ; jusqu'au conteneur 6 la clé les portait et une
clé « marqueur » tronquée à la frontière rendait le nœud final). Deux
chunks au même texte propre et à overlap différent partagent une clé.
Environ 6 clés par chunk distinct. Pour un mot : une clé par suffixe du
contenu (≤ 256), l'overlap de contenu dans le record.

Bornes : ordinal < 2²⁸ par segment depuis la nuit du 4 septembre
(`SuffixFstBuilderV3::MAX_ORDINAL`, la borne du slot de `.word_pos_map` ;
refus explicite du builder et de la fusion — le noyau entier, 25,6 M de
textes, tient seize fois), sti ≤ 255 en pratique (4 095 par l'encodage),
own_len < 16384.

### 2.4 Répartition ce soir (noyau, 93 983 fichiers, 253 segments, 10,6 Go SFX)

`.sfx` 49 % (FST 2,8 Go, parents 2,45 Go) · `.sfxpost` 13 % ·
`.word_sfxpost` 12 % · `.termtexts` 10 % · `.word_pos_map` 6 % ·
`.sibling_v3` 5 % · `.posmap` 5 %. Le dictionnaire (sfx + termtexts +
sibling) est 71 % et se répète ×2,2 entre segments → chantier suivant
([06](06-chantier-dictionnaire-partage-rapport.md)).

---

## 3. Le chemin d'une requête

```
drain → flush → [prescan par segment …] → merge_prescan → build_weight
                                                              ↓
                                    [search_shard …] → merge → output
```

**Le prescan crée un nœud par segment** (`lucivy_core/src/search_dag.rs`) :
c'est tout le parallélisme — et il le reste en mode dictionnaire, où le
plan (§2.5) a rempli la mémo du lecteur partagé avant le scatter et où
chaque segment résout ses postings. Par segment, `contains_query_v3` / `fuzzy_query_v3` /
`regex_query_v3` chargent les sidecars en `OwnedBytes` (zéro copie sur mmap)
dans un `BriquesContext`, puis `briques/` :

- `fst_walk::fst_candidates_v3` : scan de plage sur le préfixe de la
  requête (partitions selon `anchor_start` / `strict_separators`) →
  candidats mono-token ;
- `falling_walk_chunks` / `falling_walk_words` : marche octet par octet,
  décodage des parents à chaque nœud final, coupe à la frontière
  (`check_split`), `overlap_lookahead` quand la requête finit dans l'overlap ;
- chaînes cross-token : `cross_token_chain_v3` (FST) et `sibling_chain_dfs`
  (fratrie + textes, pas de la méta pour la longueur de pas) ;
- `resolve.rs` : postings, adjacence stricte (posmap : position + 1) ou
  relâchée (posmap + `has_content` pour sauter les chunks purement
  séparateurs), chaînes de mots via `word_posmap` ;
- fuzzy (`composite.rs`) : pigeonhole de trigrammes, générateur `pieces`
  ou `pivot` (`auto` ; `pivot` interdit en relâché depuis 3.0.7), fenêtre
  reconstruite via posmap + termtexts, vérification Levenshtein
  (`fuzzy_spans`, tous les spans) ou Jaro-Winkler (`best_window`, un span) ;
- regex : littéraux extraits, candidats, validation sur fenêtre.

Bornes mémoire : `LUCIVY_HIGHLIGHT_SPAN_CAP` (4 M / 1 M wasm) et
`LUCIVY_MAX_MATCHES_PER_SEGMENT` (4 M / 20 k wasm), la troncature est
signalée (`last_search_truncated`).

**Scoring** : BM25 (stats tantivy), fuzzy par paliers `tier × 1000 + bm25`
(tier = distance vérifiée), correct cross-shard.

---

## 4. Sharding, filtre, fédération, persistance (inchangés)

- `ShardedHandle` : N shards, routage `balance_weight` (0,2 effectif :
  token-aware ; 1,0 = round-robin), un `LucivyHandle` par shard.
- Recherche filtrée (`allowed_ids`) : vrai pré-filtre jusqu'au prescan,
  scores « comme si l'index était le sous-ensemble ».
- Fédération : `export_stats` (`ExportableStats`, indexé par texte de
  terme, jamais par ordinal) → `merge` → `search_with_global_stats`, via
  le DAG. Union de nœuds = index unique, mêmes scores.
- Persistance : `StdFsDirectory` (natif + OPFS, I/O différée au
  `terminate()`), `RamDirectory`, `BlobDirectory` (ACID). Formats LUCE
  (snapshot), LUCID (delta 1 shard), LUCIDS (delta N shards :
  `ShardVersion { shard_id, version, segment_ids }`). En mode dictionnaire
  la génération voyage comme un bundle `dict-<g>.` (LUCE l'emballe par nom,
  le delta la joint aux `segment_ids`, `apply_delta` retire l'ancienne par
  préfixe). Pas encore comptée par `index_bytes` / `preload` / `residency`.
- Blob store ACID exposé en Python / Node / C++.
- WASM : jamais de `thread::spawn`, I/O au `terminate()` seulement,
  `LazyFsHandle` charge un fichier entier au-delà de 64 Ko dans un LRU de
  768 Mo — chaque octet gagné sur disque est un octet de RAM.

---

## 5. La fusion

`merge_segments_v3` réinterne les textes de `.termtexts`, remappe postings
et fratrie, reconstruit tout par le DAG de création. Bornée par
`MAX_ORDINAL` = 2²⁸ − 1 depuis la nuit (24 bits avant : le harnais n'avait
pas pu compacter le noyau vers 10 segments ; non relancé depuis). En mode
dictionnaire, `merge_segments_dict` ne réinterne rien (§2.5).

---

## 6. Ce que je ne peux pas affirmer

Le détail du merger tantivy et de la politique tiered, le docstore, la
politique de fusion de `ShardedHandle` (`wait_merges_quiet`, `compact`),
l'intérieur de `sparse_vector` au-delà de son design. Pour ceux-là :
`docs/28-08-2026/08-architecture.md`, `docs/25-08-2026/06-architecture.md`,
`ARCHITECTURE.md`, avec la réserve d'usage.
