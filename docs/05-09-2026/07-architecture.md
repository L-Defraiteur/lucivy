# Architecture de lucivy — état au 5 septembre 2026, fin de soirée (4.0.0, branche `v4`)

Rappel écrit pour être lu seul. Il remplace [02](02-architecture.md) (état du
soir, avant les postings sans octets) pour les formats, la requête, les
dérivés et les options ; le sharding, la fédération et la persistance n'ont
pas bougé et sont repris tels quels.

---

## 1. Les crates et le numéro

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

Cinq crates au même numéro, **4.0.0 depuis le 5 septembre au soir, non
publié** (la 3.0.8 est la dernière sur PyPI, npm et crates.io). Bindings :
Python (PyO3), Node (napi), C++ (cxx), WASM (emscripten), `lucivy_fts`
(bridge rag3db). **Contrat de 4.0** (vérifié par `test_compat_308`) : 4.0
ouvre un index 3.0.x et rend ce que 3.0.x rendait ; 3.0.x n'ouvre pas 4.0 ;
le premier commit en 4.0 convertit sans retour.

---

## 2. Le modèle d'index SFX v3

### 2.1 Tokenisation

Un mot = une suite de caractères de contenu (`is_content_char`) suivie de
ses séparateurs, découpé en **chunks de ≤ 8 octets**, le dernier portant
les séparateurs de queue (qui débordent dans des chunks à eux au-delà). Le
collecteur étend chaque chunk des premiers octets du suivant (overlap ≤ 4) ;
`own_len` = contenu + sep. Deux familles d'entrées dans un espace
d'ordinaux : les **chunks** (partitions FST `0x00` si = 0, `0x01` si > 0,
postings `.sfxpost`) et les **mots dépouillés** (`0x02`, contenu du mot
entier + overlap de contenu du suivant, postings `.word_sfxpost`) ; un mot
de plus de 264 octets a en plus une **entrée de queue** (ses 8 derniers
octets), qui part au milieu d'un chunk. Les positions repartent à zéro à
chaque valeur d'un champ multi-valué, avec **une position vide** entre
deux valeurs ; les offsets d'octets repartent à zéro aussi.

### 2.2 Fichiers par segment et par champ

| fichier | contenu | encodage courant |
|---|---|---|
| `.sfx` | FST des suffixes (clés minuscules, préfixe de partition, coupées à la frontière du token) + table de parents | conteneur `SFX3` **version 8** ; versions 3-6 lues, 7 refusée |
| `.sfxpost` | postings de chunks : `(doc, position)` — **plus de span d'octets** | `SFP5` : un varint par entrée derrière l'en-tête par document de `SFP3`, table d'offsets par blocs ; `SFP2`-`SFP4` lus, leurs spans encore servis |
| `.word_sfxpost` | postings de mots : `(doc, first, last[, tail_off])` | `WSP5` : `d_doc`, `d_first`, `(last − first) << 1 \| drapeau`, décalage des seules queues ; `WSP2`-`WSP4` lus |
| `.termtexts` | ordinal → texte étendu (casse d'origine) + méta `[u16 own_len][u8 sep_len][u8 flags]` | `TTX3` layout 3 ; layouts 1-2 lus |
| `.posmap` | (doc, position) → ordinal, **et l'offset d'octet d'une position sur 16** | `PMP4` : par document `u32 n`, `⌈n/16⌉` points de contrôle, cases de 3 octets ; `PMP3`, `PMAP` lus |
| `.word_pos_map` | (doc, position) → mot qui y commence \| span | `WMP3` (28 \| 4 bits) ; `WMP2` lu |
| `.sibling_v3` | ordinal → ordinaux qui le suivent (chunks consécutifs d'une valeur, mots consécutifs d'une valeur) | `SIB4` ; `SIB3`, `SIB2`, v1 lus |
| `.store`, `.term`, `.idx`, `.pos`, `.fast`, `.fieldnorm` | docstore, index inversé tantivy | tantivy |

Les trois derniers fichiers SFX (`.posmap`, `.word_pos_map`, `.sibling_v3`)
sont des **dérivés** : ils ne portent rien que les postings et la méta
n'aient pas (§6). Bornes : ordinal < 2²⁸, sti ≤ 4 095, own_len < 16 384,
requête ≤ 2 048 octets.

### 2.3 Ce que le builder enregistre

Pour un chunk étendu : une clé par suffixe commençant dans les octets
propres, arrêtée à `own_len`, l'overlap dans le record. Pour un mot : une
clé par suffixe du contenu (≤ 256), l'overlap de contenu dans le record. Les
tokens sont internés **par forme** (texte, `own_len`, `sep_len`, mot ou
non) : `"0"+"ui"` et `"0u"+"i"` sont deux ordinaux, et la méta d'un ordinal
dit la longueur de contenu de toutes ses occurrences (vérifié sur 137 M
postings de mots).

---

## 3. Le mode dictionnaire (`sfx_version` 4, option `shared_dictionary`)

Inchangé depuis [02](02-architecture.md) §3 : **un dictionnaire par shard**
(`dict-<g>.<champ>.sfx/.termtexts`, une génération par commit avec ses seuls
nouveaux textes, `.gmap` `GMP2` par segment, compaction en fusion de flux
au-delà de 8 générations), le plan par shard avant le scatter par segment,
`Alts::Prefix`, coupe en galop. Pas le défaut. À la fusion, la méta du
dictionnaire fournit les `own_len` du `.posmap` (`PMP4`).

---

## 4. Le chemin d'une requête (v3 et dictionnaire)

```
drain → flush → [prescan par segment …] → merge_prescan → build_weight
                                                              ↓
                                    [search_shard …] → merge → output
```

Par segment, `contains_query_v3` / `fuzzy_query_v3` / `regex_query_v3`
chargent les sidecars dans un `BriquesContext`, puis `briques/` :
`fst_walk` (candidats, marches, chaînes), `resolve.rs` (postings, adjacence
stricte par `.posmap` ou relâchée par `.posmap` + `has_content`, chaînes de
mots par `.word_pos_map`), `composite.rs` (fuzzy : pièces ou pivot,
régions, fenêtres, alignement ; regex : littéraux, régions, fenêtres,
`find_iter`), `orchestrator.rs` (`contains_v3` : placement, débordement
d'overlap, dédoublonnage, vérification sur le texte reconstruit).

**La requête travaille en positions de bout en bout.** Un `MatchV3` sort
des résolveurs non placé : `(doc, position, span, sti, ordinaux)` et ses
entrées de placement — `first_off` (sti + décalage de queue du premier
jeton), **`last_start_pos`** (la position où commence le *texte* du dernier
jeton : la dernière position pour un chunk, le **premier** chunk pour un
mot, dont `last_position` n'est que la fin du span), `last_off`,
`last_consumed`. `orchestrator::place_spans` dérive les octets des matches
gardés : `byte_from = byte_at(position) + first_off`, `token_end =
byte_at(last_start_pos) + last_off + contenu(last_ordinal)`, `byte_to`
borné au contenu pour un mot (excès dans `overlap_overflow`, placé après
les séparateurs). `BriquesContext::byte_at(doc, p)` a deux dos : `PMP4`
(point de contrôle + `own_len` des positions intermédiaires, case vide =
zéro) ou, sur un segment ancien, le posting du chunk. Les hits fuzzy et
regex se regroupent par positions (jeu de séparateurs du fuzzy : 5
positions, ce que 32 octets occupent au plus) ; les fenêtres
(`rebuild_window_opts`) s'ancrent par un `byte_at` et dérivent de position
en position ; les spans finaux viennent de la carte arrière de la fenêtre.

Bornes mémoire : `LUCIVY_HIGHLIGHT_SPAN_CAP`, `LUCIVY_MAX_MATCHES_PER_SEGMENT`,
troncature signalée. Scoring : BM25, fuzzy par paliers, correct cross-shard.

**Ce que la fuzzy à deux éditions coûte** (`regsiter`, 30 000 fichiers,
141 ms) : 2,57 M de hits (pièces de deux ou trois lettres), 463 000
régions dont 91 % rejetées par l'alignement après reconstruction de leur
fenêtre. Le coût est la marche des positions et des textes ; les deux passes
(texte seul, carte arrière ensuite) ont été mesurées sans gain et retirées.

---

## 5. Sharding, filtre, fédération, persistance (inchangés)

- `ShardedHandle` : N shards, routage `balance_weight` (0,2 effectif).
- Recherche filtrée : vrai pré-filtre jusqu'au prescan.
- Fédération : `export_stats` → `merge` → `search_with_global_stats`, via
  le DAG ; union de nœuds = index unique, mêmes scores.
- Persistance : `StdFsDirectory`, `RamDirectory`, `BlobDirectory` ; LUCE,
  LUCID, LUCIDS. Un fichier géré finit par le **pied** du répertoire (CRC,
  version, ~93 octets) que les lecteurs ne voient pas.
- WASM : jamais de `thread::spawn`, I/O au `terminate()` seulement.

## 6. Les dérivés, et l'option `derived_in_ram`

`.posmap` est l'inverse de `.sfxpost` (plus ses points de contrôle),
`.word_pos_map` celui de `.word_sfxpost`, `.sibling_v3` les liens des
positions consécutives et des mots consécutifs d'une valeur — 32 % de
l'index du noyau après les postings sans octets. `suffix_fst::derived::
rebuild(sfxpost, word_sfxpost, own_len)` les reproduit **octet pour octet**
depuis les postings et la méta (les mêmes écrivains, nourris dans le même
ordre ; les entrées de queue exclues des mots consécutifs : de deux entrées
finissant à la même position, le mot est celle qui commence en premier).

**Option `derived_in_ram`** (`IndexSettings`, fixée à la création, jamais
le défaut) : l'index n'écrit pas les trois fichiers ; `SegmentReader::open`
les rebâtit **à l'ouverture** — les lecteurs s'ouvrent en parallèle par le
DAG de rechargement ; `Index::derived_cache` garde le résultat par
(segment, champ), élagué aux segments vivants à chaque rechargement, pour
ne refaire que les segments nouveaux. `SegmentReader::sfx_index_file` sert
les tranches rebâties là où il servirait les fichiers ; `list_files_for(
sfx_version, derived_in_ram)` ne les nomme pas. Décision de Lucie : jamais
à la première requête — une requête plus lente que les autres tromperait.
Noyau 4 938 → 3 344 Mo ; ouverture 43 ms → 1,8 s (253 segments, 43 s de
CPU répartis) ; structures résidentes (1,6 Go).

## 7. La fusion

`merge_segments_v3` réinterne les textes, remappe postings (positions) et
fratrie, reconstruit tout ; un segment source écrit avec des spans
(`WSP2`-`WSP4`) voit ses entrées de queue converties (`tail_off_from_spans`
par son `.posmap` et ses postings). `merge_segments_dict` ne réinterne
rien ; il reçoit les `own_len` par la méta du shard. Fusions de fond sous
permis (`LUCIVY_MERGE_CONCURRENCY` : illimité en natif, 1 sur wasm, 2 pour
un index à dictionnaire dans le navigateur). Un index créé en 3.0.8 se
convertit ainsi segment par segment ; `compact` force la fusion de tous.

## 8. Le navigateur (emscripten)

Inchangé depuis [02](02-architecture.md) §6 ter (résidence, `preload` qui
attend les fusions, `memory_status` et `heap_bytes`, plancher de 1,5 Go en
indexation, 4 Go d'adresses). `derived_in_ram` y est accepté par
`IndexConfig` (typé dans `lucivy.d.ts`) ; le playground le prend par `?ram`.
Non mesuré sur les 15 440 fichiers du noyau : temps d'ouverture depuis
l'OPFS et pic mémoire avec l'option.

## 9. Ce que je ne peux pas affirmer

Le détail du merger tantivy et de la politique tiered, le docstore, la
politique de fusion de `ShardedHandle`, l'intérieur de `sparse_vector`.
Pour ceux-là : `docs/28-08-2026/08-architecture.md`, `ARCHITECTURE.md`.
