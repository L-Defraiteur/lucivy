# Architecture de lucivy — état au 4 septembre 2026 au soir

Rappel écrit pour être lu seul, mis à jour après la journée de réduction
d'index (branche `v4`). Il remplace `docs/28-08-2026/08-architecture.md`
pour tout ce qui concerne les formats ; le reste (crates, DAG, sharding,
fédération) y est repris tel quel parce qu'il n'a pas bougé.

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
| `.sfx` | FST des suffixes (clés minuscules, préfixe de partition) + table de parents | conteneur `SFX3`, **version 5** : parent unique inline dans la valeur FST (63 bits : ordinal 24, sti 12, own_len 14, sep 8, overlap 4, ws 1) ; record multi-parents = varint count + par parent (Δordinal, sti, own_len en varint, sep_len, flags), ≈ 5 octets. Versions 3 (11 o/parent) et 4 (u64 packé) encore lues |
| `.sfxpost` | postings de chunks : par ordinal, (doc, position, byte_from, byte_to − byte_from) | `SFP3`, varints, checkpoints par 8 docs, table d'offsets u32 |
| `.word_sfxpost` | postings de mots : (doc, first_pos, last_pos, byte_from, to − from) | `WSP3`, varints, checkpoints par 32 |
| `.termtexts` | ordinal → texte étendu (casse d'origine) + méta | `TTX3` **layout 2** : une section ENTRIES, 8 octets par ordinal (`u32 offset`, `u16 own_len`, `u8 sep_len`, `u8 flags` = overlap 4 bits + ws + stripped), puis les textes ; STATS (max word). Layout 1 (TEXTS + META à part) encore lu |
| `.posmap` | (doc, position) → ordinal de chunk | **`PMP3`** : 3 octets par position, vide = 0xFFFFFF ; `PMAP` (4 o) encore lu |
| `.word_pos_map` | (doc, position) → ordinal de mot qui commence là \| span | `WMP2`, 4 octets par position |
| `.sibling_v3` | ordinal → ordinaux qui le suivent dans le texte | **`SIB3`** : un varint de delta par lien ; `SIB2` (avec gap) encore lu ; v1 aussi |
| `.store` | docstore LZ4 (16 Ko) | tantivy |
| `.term`, `.idx`, `.pos`, `.fast`, `.fieldnorm` | index inversé tantivy (BM25, `more_like_this`) | tantivy |

Disparu : **`.bytemap`** (la question « ce chunk a-t-il un octet de
contenu ? » est `own_len > sep_len` dans la méta de `.termtexts`,
`TermTextsReaderV3::has_content`). `.gapmap`, `.sepmap`, `.sibling` sont v2.

### 2.3 Ce que le builder enregistre

Pour un chunk étendu : une clé par suffixe commençant **dans les octets
propres** (si < own_len ; les suffixes commençant dans l'overlap ont été
retirés le 4 septembre, ils étaient les clés d'un et deux octets aux listes
géantes), plus une **clé marqueur** tronquée à `own_len − si` pour chaque
suffixe qui déborde dans l'overlap, qui rend final le nœud de la frontière
pour que la marche puisse couper là. Environ 6 clés + 6 marqueurs par
chunk distinct. Pour un mot : une clé par suffixe du contenu (≤ 256).

Bornes : ordinal < 2²⁴ par segment (refus explicite du builder et de la
fusion — ~50 000 fichiers de noyau par segment au plus), sti < 4096,
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
c'est tout le parallélisme, et c'est ce qu'un dictionnaire par shard
réduira. Par segment, `contains_query_v3` / `fuzzy_query_v3` /
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
  `ShardVersion { shard_id, version, segment_ids }`).
- Blob store ACID exposé en Python / Node / C++.
- WASM : jamais de `thread::spawn`, I/O au `terminate()` seulement,
  `LazyFsHandle` charge un fichier entier au-delà de 64 Ko dans un LRU de
  768 Mo — chaque octet gagné sur disque est un octet de RAM.

---

## 5. La fusion

`merge_segments_v3` réinterne les textes de `.termtexts`, remappe postings
et fratrie, reconstruit tout par le DAG de création. Bornée par les 24
bits d'ordinal : le harnais n'a pas pu compacter le noyau vers 10 segments
ce soir. L'index de bench du 28 août à 10 segments avait été obtenu par
`bench_sharding` (autre chemin) — à élucider.

---

## 6. Ce que je ne peux pas affirmer

Le détail du merger tantivy et de la politique tiered, le docstore, la
politique de fusion de `ShardedHandle` (`wait_merges_quiet`, `compact`),
l'intérieur de `sparse_vector` au-delà de son design. Pour ceux-là :
`docs/28-08-2026/08-architecture.md`, `docs/25-08-2026/06-architecture.md`,
`ARCHITECTURE.md`, avec la réserve d'usage.
