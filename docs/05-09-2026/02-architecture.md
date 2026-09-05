# Architecture de lucivy — état au 5 septembre 2026 au soir

Rappel écrit pour être lu seul, sur la branche `v4`. Il remplace
[`../04-09-2026/07-architecture.md`](../04-09-2026/07-architecture.md)
(état du matin) pour tout ce qui concerne le mode dictionnaire et la
requête ; les formats par segment, le sharding, la fédération et la
persistance n'ont pas bougé depuis et sont repris tels quels.

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
`lucivy_fts` (bridge rag3db). **3.0.8 est la dernière publiée** ; un
binaire 3.0.x ne lit pas un index v4 (le `CHANGELOG` a une section
« Unreleased » qui liste ce qui change).

---

## 2. Le modèle d'index SFX v3

### 2.1 Tokenisation

Un mot = une suite de caractères de contenu (`is_content_char` : non-ASCII
ou alphanumérique) suivie de ses séparateurs. Chaque mot est découpé en
**chunks de ≤ 8 octets** (`DEFAULT_MAX_TOKEN`), le dernier portant le
séparateur de queue. Le collecteur étend chaque chunk des **premiers
octets du chunk suivant** (overlap, ≤ 4) : texte étendu = contenu + sep +
overlap ; `own_len` = contenu + sep.

Deux familles d'entrées, dans un seul espace d'ordinaux :
- les **chunks** : partitions FST `0x00` (suffixe à si = 0) et `0x01`
  (si > 0), postings dans `.sfxpost` ;
- les **mots dépouillés** (`0x02`) : contenu du mot entier sans séparateurs
  + overlap de contenu du mot suivant, postings dans `.word_sfxpost`.
  C'est ce qui fait `rag3weaver` ↔ `rag3_weaver` en séparateurs relâchés.

### 2.2 Fichiers par segment et par champ

| fichier | contenu | encodage |
|---|---|---|
| `.sfx` | FST des suffixes (clés minuscules, préfixe de partition, coupées à la frontière du token) + table de parents | conteneur `SFX3` **version 8** : valeur FST = offset du record ; record plat ≤ 32 parents (Δordinal, sti varint, flags = ws + overlap + sep_len, octets d'overlap ; `own_len` dérivé de la clé), groupé par overlap au-delà (`decode_parents_where` saute les groupes). Versions 3-6 encore lues, 7 refusée |
| `.sfxpost` | postings de chunks (doc, position) — **plus de span d'octets** | `SFP5` (5 septembre au soir) : un varint par entrée derrière l'en-tête par document de `SFP3`, table d'offsets par blocs (`block_offsets.rs`) ; `SFP2`-`SFP4` lus, leurs spans encore servis |
| `.word_sfxpost` | postings de mots (doc, first_pos, last_pos, décalage de queue) | `WSP5` : `d_doc`, `d_first`, `(last − first) << 1 \| a_un_décalage`, `[décalage]` (les seules entrées de queue des mots > 264 octets) ; `WSP2`-`WSP4` lus |
| `.termtexts` | ordinal → texte étendu (casse d'origine) + méta | `TTX3` layout 3 : offsets par blocs, 4 octets de méta par ordinal, textes ; STATS (max word) ; section IDS pour les générations ; layouts 1-2 lus |
| `.posmap` | (doc, position) → ordinal de chunk, **et l'offset d'octet d'une position sur 16** | `PMP4` : par document `u32 n`, `⌈n/16⌉` points de contrôle `u32`, puis les cases de 3 octets ; `PMP3`, `PMAP` lus (sans points de contrôle : les spans viennent alors des postings) |
| `.word_pos_map` | (doc, position) → ordinal de mot qui commence là \| span | `WMP3` : 28 \| 4 bits ; `WMP2` lu |
| `.sibling_v3` | ordinal → ordinaux qui le suivent | `SIB4` (varints, table par blocs) ; `SIB3`, `SIB2`, v1 lus |
| `.store`, `.term`, `.idx`, `.pos`, `.fast`, `.fieldnorm` | docstore, index inversé tantivy | tantivy |

Plus de `.bytemap` (« ce chunk a-t-il du contenu ? » = `own_len > sep_len`
dans la méta). Bornes : ordinal < 2²⁸ (`MAX_ORDINAL`), sti ≤ 4 095,
own_len < 16 384.

### 2.3 Ce que le builder enregistre

Pour un chunk étendu : une clé par suffixe commençant dans les octets
propres, arrêtée à `own_len`, l'overlap dans le record. Deux chunks au même
texte propre et à overlap différent partagent une clé. Pour un mot : une
clé par suffixe du contenu (≤ 256, `WORD_SUFFIX_CAP`), l'overlap de contenu
dans le record.

---

## 3. Le mode dictionnaire (`sfx_version` 4, option `shared_dictionary`)

### 3.1 Ce que c'est

Un index créé avec `"shared_dictionary": true` (ou `"sfx_version": 4`) a
**un dictionnaire par shard** au lieu d'un par segment : le même moteur,
les mêmes clés et formats, sur des **identifiants globaux au shard**.
Mesuré : 10 000 fichiers 508 → 390 Mo, 30 000 : 1 659 → 1 327 Mo (−20 %),
noyau entier **7,3 → 5,6 Go** (−23 %) à format égal ; comptes, spans et
scores identiques partout. **Pas le défaut** (décision du 5 septembre) :
construction 2,5 fois plus lente (19 s contre 8 sur 10 000), requêtes
×0,8 à ×1,6 à froid, un format de plus à porter.

| fichier | portée | contenu |
|---|---|---|
| `dict-<g>.<champ>.sfx` | shard, génération `g` | FST + parents (conteneur 8) des identifiants frappés par cette génération |
| `dict-<g>.<champ>.termtexts` | shard, génération `g` | ses entrées → texte étendu + méta (layout 3) ; section IDS (plages) : quel identifiant est chaque entrée |
| `<seg>.<champ>.gmap` | segment | identifiants globaux de ses ordinaux locaux, triés — **layout 2 `GMP2`** : en-tête (n, plus long mot dépouillé du segment, 0xFFFF = inconnu), les ids, puis la tête de chaque bloc de 64 ; `GMAP` (layout 1) lu |
| `<seg>.<champ>.newtexts` | segment, hors registre | textes et méta des identifiants que le segment a frappés ; consommé et supprimé par le commit suivant |
| postings, cartes, fratrie | segment | inchangés, à ordinaux locaux |

`meta.json` porte `sfx_dictionary { generations (vivantes), next_generation,
next_ids (par champ), field_ids }` ; le GC, le snapshot LUCE, le delta (un
bundle `dict-<g>.` par vivante) et `Index` (qui tient le dictionnaire
ouvert, `refresh_sfx_dictionary`) le lisent. Les lecteurs voient les
générations comme un fichier (`SfxFileReaderV3::open_parts`,
`TermTextsReaderV3::open_parts`, `may_have_long_words` sur toutes).

### 3.2 Indexation, commit, fusion

**Collecte** (`SfxCollectorV3::with_dictionary`) : chaque texte nouveau est
cherché dans la génération courante, puis dans les textes en attente du
shard (`DictionaryShared`), sinon frappé sur le compteur du champ ; ordinaux
locaux dans l'ordre des identifiants ; le segment écrit son `.gmap` (avec
sa statistique « mots longs », calculée sur ses métas) et son `.newtexts`,
ni `.sfx` ni `.termtexts`.

**Commit** (`indexer/dictionary_commit.rs::fold_new_texts`) : les `.newtexts`
des segments commis → une génération de plus, **leurs textes seulement**.
Au-delà de `LUCIVY_DICT_MAX_GENERATIONS` (8) vivantes, **compaction en
fusion de flux** (`suffix_fst/dictionary_compact.rs`,
[01](01-journal-session-5-septembre.md) §13) : les plus petites
générations — assez pour ramener le compte à la moitié du maximum
(`choose_compaction`) — fusionnent en une seule : union des FST dans
l'ordre des clés, record copié tel quel quand une seule génération tient
la clé, parents fusionnés et ré-encodés sinon, FST et table des parents
écrites en flux (fichiers temporaires `dict-<g>.<champ>.sfx.*.tmp`, puis
le conteneur assemblé) ; `.termtexts` par un tas sur les curseurs, trois
passes, seule la table des offsets en RAM (`termtexts_v3::write_merged`).
Résultat identique octet pour octet à une reconstruction ; noyau : 19 s
et 229 Mo au lieu de 48 s et 12,8 Go. Les identifiants ne bougent
jamais ; `remove_leftovers` efface ce qu'un commit planté a laissé sous
le numéro réutilisé.

**Fusion** (`merge_segments_dict`) : union triée des `.gmap` (statistique
« mots longs » = max des entrées), remappage des locaux, concaténation des
postings, fratrie et cartes rebâties. Pas de texte, pas de FST.

### 3.3 La requête : plan puis exécution

**Le plan** (`briques/plan.rs`), au début de `prescan_segments_more` des
trois requêtes v3 — donc sur tous les chemins : index simple, shardé, par
lots, fédéré. Par dictionnaire (les segments sont regroupés par identité de
`DictionaryField`), il énumère les cellules FST que la requête demandera et
les calcule **en parallèle** (une tâche par cellule, priorité Critical)
dans la mémo du lecteur partagé (`FstMemo` : cellules à trois états,
`RwLock`, `peek`) avant le scatter. Un littéral = **une vague** : ses
candidats par partition, ses marches (chunk si strict ou mots longs
possibles dans un segment du shard ; mot si relâché), et pour **chaque
suffixe** de la requête minuscule — les restes qu'une chaîne peut
atteindre en sont tous — sa marche et son compte ancré (pas pour un reste
de un ou deux octets, présumé présent), plus la liste SI0 des racines
ancrées sur le second token en strict. Le fuzzy : les comptes de ses
n-grammes et de toutes ses pièces, puis le générateur
(`composite::fuzzy_generator`, la même décision que les segments), puis
les littéraux des pièces ou les listes des n-grammes gardés. La regex :
ses littéraux requis. **Personne n'attend sous les tâches** (une cellule ne
touche pas la mémo) ; une cellule non prévue est calculée en ligne par le
premier segment : le plan est une optimisation, jamais une condition
d'exactitude (`V3_PLAN=0`). Coût : 0,3 à 1,7 ms par requête sur 30 000.

**L'exécution** par segment, en parallèle comme en v3 (un nœud par
segment dans le DAG, ou le scatter de `prescan_segments_more`). Le lecteur
FST partagé (`SegmentReader::sfx_dictionary_field`) donne à chaque segment
une vue `for_segment(gmap)` qui coupe les listes mémoïsées à ses
identifiants — `keep_in_segment`, **intersection en galop** depuis le côté
le plus petit, têtes de blocs du `.gmap` (`GmapReader::lower_bound_from`,
`local`). Un reste avalé par la dernière position d'une chaîne est un
**`Alts::Prefix`** testé sur le texte étendu de `.termtexts` (octets
propres + overlap = ce qu'une clé SI=0 couvre), pas une liste ; la
première position d'une chaîne reste explicite. Les cinq lecteurs
traduisent global ↔ local (`with_gmap`) ; `BriquesContext::segment_long_words`
(du `.gmap`) décide par segment si les chaînes chunk sont marchées en
relâché. Puis les briques d'aujourd'hui : postings, posmap, fratrie,
fenêtres, vérification.

**Mesuré** (30 000 fichiers, même binaire, min de 3 passes, à froid) :
×0,8 à ×1,6 par rapport à v3, le ×1,5 tenu sur neuf requêtes sur dix, la
regex à ×1,6, le fuzzy plus rapide ; noyau entier : même profil. Vérité :
`test_dictionary_index`, les variantes `sfx_version 4` de fédéré, filtré,
roundtrip LUCE, et le panel du noyau (demo 9/9, contains 15/15, cohérence
31/31).

### 3.4 L'option dans les interfaces

`SchemaConfig::shared_dictionary: Option<bool>` (alias de `sfx_version` 4,
`effective_sfx_version()`, contradiction refusée). Python
`Index.create(path, fields, shards=None, shared_dictionary=False)` et
`create_with_blob_store` ; Node `Index.create(path, fields, shards?,
sharedDictionary?)`, `BlobIndexOptions.sharedDictionary` ; C++
`lucivy_create(path, fields_json | schéma complet, shards)` ; emscripten
`IndexConfig.shared_dictionary` ; rag3db : le JSON de schéma. Description
partout : *environ 20 % de moins sur disque et en RAM, requêtes un peu
plus lentes à froid (×1,2 à ×1,6 sur les exactes, fuzzy plus rapide),
mêmes réponses, fixée à la création*.

---

## 4. Le chemin d'une requête (v3 et dictionnaire)

```
drain → flush → [prescan par segment …] → merge_prescan → build_weight
                                                              ↓
                                    [search_shard …] → merge → output
```

Le prescan crée un nœud par segment (`lucivy_core/src/search_dag.rs`) ;
en mode dictionnaire le plan (§3.3) a rempli la mémo avant. Par segment,
`contains_query_v3` / `fuzzy_query_v3` / `regex_query_v3` chargent les
sidecars en `OwnedBytes` dans un `BriquesContext`, puis `briques/` :

- `fst_walk::fst_candidates_v3` : scan de plage sur la requête (partitions
  selon `anchor_start` / `strict_separators`) → candidats mono-token ;
  `fst_candidates_count_v3` compte sans décoder ;
- `falling_walk_chunks` / `falling_walk_words` : marche octet par octet,
  parents décodés aux nœuds finaux, coupe à la frontière ;
- chaînes cross-token : `build_chains_from_splits` (FST, `Alts`) et
  `sibling_chain_dfs` (fratrie + textes) ; en strict, les têtes courtes
  sont ancrées sur le second token (`second_token_anchored_v3`) ;
- `resolve.rs` : postings, adjacence stricte (posmap : position + 1) ou
  relâchée (posmap + `has_content`), chaînes de mots via `word_posmap`,
  `Alts::contains` pour l'appartenance ;
- fuzzy (`composite.rs`) : pigeonhole de trigrammes, générateur `pieces`
  ou `pivot` (`auto` ; `pivot` interdit en relâché), fenêtre reconstruite,
  vérification Levenshtein ou Jaro-Winkler ;
- regex : littéraux extraits, candidats, validation sur fenêtre.

Bornes mémoire : `LUCIVY_HIGHLIGHT_SPAN_CAP`, `LUCIVY_MAX_MATCHES_PER_SEGMENT`,
troncature signalée (`last_search_truncated`). Scoring : BM25, fuzzy par
paliers `tier × 1000 + bm25`, correct cross-shard.

---

## 5. Sharding, filtre, fédération, persistance (inchangés)

- `ShardedHandle` : N shards, routage `balance_weight` (0,2 effectif).
- Recherche filtrée (`allowed_ids`) : vrai pré-filtre jusqu'au prescan,
  scores « comme si l'index était le sous-ensemble ».
- Fédération : `export_stats` → `merge` → `search_with_global_stats`, via
  le DAG ; union de nœuds = index unique, mêmes scores — vérifié aussi
  avec des nœuds dictionnaire contre un index v3.
- Persistance : `StdFsDirectory`, `RamDirectory`, `BlobDirectory` ; LUCE,
  LUCID, LUCIDS. Une génération de dictionnaire voyage comme un bundle
  `dict-<g>.` ; comptée par `preload` et `shard_bytes_and_files`
  (`dictionary_files()`), pas encore par `index_bytes` natif.
- WASM : jamais de `thread::spawn`, I/O au `terminate()` seulement.

## 6. La fusion

`merge_segments_v3` réinterne les textes, remappe postings et fratrie,
reconstruit tout ; bornée par `MAX_ORDINAL` = 2²⁸ − 1. En mode
dictionnaire, `merge_segments_dict` ne réinterne rien (§3.2). Les fusions
de fond sont des tâches du scheduler (`segment_updater_actor::start_merges`,
une tâche par fusion, priorité High) sous un **permis** global
(`merge_permits.rs`, `LUCIVY_MERGE_CONCURRENCY`) : illimité en natif, 1
sur wasm32 (une fusion v3 rebâtit la FST en RAM ; quatre à la fois ont tué
le navigateur le 24 août), **2 pour un index à dictionnaire dans le
navigateur** (posé par le binding, mesuré le 5 septembre : pic mémoire
inchangé). L'attente d'un permis fait tourner d'autre travail sur le thread
(pas un sémaphore bloquant : quatre threads et quatre attentes = interblocage).
`LUCIVY_VERBOSE` trace chaque fusion (`[merge] N segments: waited … ran …`).

## 6 bis. Ce que les postings portent, et d'où viennent les octets

Depuis le 5 septembre au soir les postings ne portent **plus de span
d'octets** : une entrée de `.sfxpost` est `(doc, position)`, une entrée de
`.word_sfxpost` `(doc, first, last[, tail_off])`. L'offset d'octet d'une
position se **dérive** : le `.posmap` (`PMP4`) garde l'offset d'une
position sur seize par document ; `byte_at(doc, p)` lit le point de
contrôle qui précède et additionne les `own_len` (méta des textes) des
positions intermédiaires — une case vide remet à zéro (frontière de
valeur : le collecteur repart à l'octet 0 à chaque valeur, une position de
séparation entre deux). La longueur d'un chunk est son `own_len`, celle
d'un mot `own_len − sep_len` de son ordinal. Vérifié entrée par entrée sur
167 M chunks et 137 M mots du noyau avant de retirer les spans
(`postings_measure::byte_spans_are_derivable`, 0 désaccord). L'exception :
l'**entrée de queue** d'un mot de plus de 264 octets (ses 8 derniers
octets, pour les requêtes en fin de mot) part au milieu d'un chunk ; elle
seule porte son décalage (`tail_off`, un varint, un drapeau dans
`last − first`).

**Le service** : `BriquesContext::byte_at(doc, position)`, deux dos —
`PMP4` par dérivation ; un segment ancien (`PMP3` + postings à spans) par
`posmap.ordinal_at` puis `resolve_doc_at(...).byte_from`. Aucune brique ne
lit `e.byte_from` d'un posting ; `PostingResolver` rend des positions
(`positions`, `positions_filtered`, `positions_for_doc`, `has_position`)
et dit `has_byte_spans()`.

**La requête travaille en positions de bout en bout.** `MatchV3` sort des
résolveurs *non placé* : `(doc, position, span, sti, ordinaux)` et ses
entrées de placement — `first_off` (sti + décalage de queue du premier
jeton), `last_start_pos` (la position où **commence le texte du dernier
jeton** : la dernière position pour un chunk, le **premier chunk** du mot
pour un mot, dont `last_position` est la fin du span et non le début de
ses octets — l'oubli de cette distinction a donné `[5441..5456]` pour
`mutex lock` le temps d'une passe de vérité), `last_off`, `last_consumed`
(octets consommés depuis le début du texte du dernier jeton, non bornés).
`orchestrator::place_spans` place les matches gardés, triés par document
: `byte_from = byte_at(position) + first_off`, `token_end = byte_at(
last_start_pos) + last_off + contenu(last_ordinal)`, `byte_to = début du
dernier jeton + last_consumed`, borné au contenu pour un mot (l'excès dans
`overlap_overflow`, placé après les séparateurs par l'orchestrateur). Le
dédoublonnage `(doc, position, byte_from)` se fait après placement ; la
vérification (`verify_literal`) lit des spans placés. Les hits fuzzy et
regex se regroupent **par positions** (une position ≥ 1 octet : le même
seuil numérique regroupe un sur-ensemble ; le jeu de séparateurs du fuzzy
est passé de 32 octets à 5 positions, ce que 32 octets de séparateurs
occupent au plus) ; les fenêtres reconstruites (`rebuild_window_opts`)
s'ancrent par un `byte_at` au lieu de deux lookups de postings, puis
dérivent de position en position comme avant ; les spans finaux viennent
de la carte arrière de la fenêtre.

Ce que ça pèse : 30 000 fichiers, dictionnaire, 1 131,7 → 977,9 Mo de
fichiers SFX (−13,6 %), postings −35 à −38 %, posmap +8 % ; v3 −9,6 % ;
noyau entier, dictionnaire, 5 717 → **4 938 Mo** sur disque (×5,8 le texte).

Trois fichiers sont des **dérivés** des postings : `.posmap` (inverse de
`.sfxpost`, plus les points de contrôle), `.word_pos_map` (inverse de
`.word_sfxpost`), `.sibling_v3` (positions consécutives d'une valeur et
mots consécutifs d'une valeur) — 32 % de l'index du noyau après les
postings sans octets. **Option `derived_in_ram`** (5 septembre au soir,
`IndexSettings`, fixée à la création, jamais le défaut) : l'index ne les
écrit pas, et `SegmentReader::open` les rebâtit depuis les postings et la
méta **à l'ouverture** (`suffix_fst::derived::rebuild` ; les lecteurs
s'ouvrent en parallèle par le DAG de rechargement ; un cache par segment
sur l'`Index`, `derived_cache`, épargne aux rechargements les segments
qu'ils avaient déjà, élagué aux segments vivants), **octet pour octet** les
fichiers qu'il aurait écrits : les lecteurs ne font pas la différence, et
aucune requête ne paie quoi que ce soit — décision de Lucie, une première
requête plus lente tromperait sur la vitesse du moteur. Noyau 4 938 →
3 344 Mo ; l'ouverture paie le rebâti (noyau : 1 791 ms pour 253 segments contre 43 ms avec les fichiers ; 30 000 : 286 contre 21) ; structures résidentes
(1,6 Go) là où un fichier mappé ne coûte que ce qu'on touche.

## 6 ter. Le navigateur (emscripten)

- **Résidence** : `residency()` tient l'index entier en mémoire sous
  `LUCIVY_RAM_INDEX_MAX` (3 Go sur wasm32, calibré le 25 août sur des index
  40 % plus gros qu'aujourd'hui), le streame par lots au-dessus. Le compte
  d'octets par shard est mémoïsé contre la liste des segments
  (`shard_bytes_and_files_cached`) ; sans cache il ouvre chaque fichier sur
  OPFS — une seconde par appel, ce que `memory_status` payait après chaque
  frappe jusqu'au 5 septembre.
- **`preload`** attend d'abord les fusions de fond (`wait_merges_quiet`)
  puis lit tous les fichiers ; le binding rend `merge_wait_ms` à part
  (70 s d'attente pour 4 s de lecture après seize commits de 1 000).
- **`memory_status`** : `index_bytes`, `in_memory`, `num_docs`,
  `last_search_truncated` (plafond `LUCIVY_MAX_MATCHES_PER_SEGMENT`,
  20 000 sur wasm : une requête tronquée le dit), `warnings`, par shard, et
  `heap_bytes` = taille de la mémoire linéaire WASM, qui ne fait que
  croître : le pic. Plancher mesuré en indexation : 1,5 Go avant tout index.
- Réglages : scheduler `available_parallelism` borné à 2..8, un thread
  d'indexation, `LUCIVY_MAX_PENDING_FINALIZE` 2, `LUCIVY_MAX_MERGED_DOCS`
  2 000, fusions 1 (v3) / 2 (dictionnaire). Drapeaux `--…` de
  `Module.arguments`, options du constructeur `Lucivy` — **la liste de
  `lucivy.js` filtre** ce qui passe au worker.
- Bornes : 4 Go d'adresses (wasm32 ; Memory64 existe, 10 à 20 % plus lent,
  pas essayé) ; un seul répertoire OPFS `user_index` par origine, deux
  onglets qui indexent s'y télescopent.

## 7. Ce que je ne peux pas affirmer

Le détail du merger tantivy et de la politique tiered, le docstore, la
politique de fusion de `ShardedHandle`, l'intérieur de `sparse_vector`.
Pour ceux-là : `docs/28-08-2026/08-architecture.md`, `ARCHITECTURE.md`,
avec la réserve d'usage.
