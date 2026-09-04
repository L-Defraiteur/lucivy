# Prochain chantier : le dictionnaire partagé par shard — rapport complet

**État au 5 septembre au matin : fait, en v1.** La refonte du `.sfx` (§2.2)
est allée plus loin que prévu (conteneurs 6, 7, 8 ; ordinaux 28 bits ;
tables d'offsets par blocs) et le dictionnaire partagé est implémenté
comme le §2.3 l'ordonnait — une génération réécrite à chaque commit,
justesse prouvée. Le journal [09](09-journal-chantier-dictionnaire.md)
dit ce qui a divergé (`.newtexts` hors registre, compteurs par champ,
mémo du lecteur partagé) et ce qui reste (générations incrémentales,
calcul froid parallèle). Le texte ci-dessous est le plan tel qu'écrit.

Écrit le 4 septembre 2026 au soir pour la session suivante, qui repart sans
l'historique. Il rassemble ce qu'on propose, ce qui est déjà su du code
concerné, et où regarder pour le reste. Le document court qui a fixé la
décision est [05](05-piste-dictionnaire-partage-par-shard.md) ; celui-ci
le développe et ne le contredit pas.

---

## 1. Pourquoi ce chantier, en trois chiffres

1. **L'index fait encore ×12,3 le texte** sur le noyau (11,06 Go pour
   898 Mo, 93 983 fichiers, 253 segments), après les −36 % de la journée
   ([04](04-recap-journee-et-a-faire.md)). Tantivy en trigrammes fait ×0,8,
   Elasticsearch ×3,6. La promotion reste en pause tant qu'on est là.
2. **Le dictionnaire est 71 % de l'index** : `.sfx` (FST + parents) 49 %,
   `.termtexts` 10 %, `.sibling_v3` 5 %, plus les tables d'offsets par
   ordinal des postings. Les occurrences (`.sfxpost`, `.word_sfxpost`,
   `.posmap`, `.word_pos_map`, docstore) sont le reste.
3. **Le dictionnaire est répété ×2,2 entre segments** : sur l'index de
   référence de 10 000 fichiers (160 segments), 5,17 M d'ordinaux pour
   2,34 M de textes distincts ; sur 30 000 fichiers (120 segments), 14,8 M
   pour 6,48 M. Partagé, il tiendrait dans 44 % de l'actuel.

Gain attendu : **−35 à −40 % de l'index**, en plus des −36 % acquis.
735 → ~450 Mo sur la référence, ×12,3 → ~×7 sur le noyau. Et une requête
marche une FST par génération de dictionnaire au lieu d'une par segment.

---

## 2. Ce qu'on propose

### 2.1 Le modèle cible

- **Un dictionnaire par shard**, générationnel. Une génération = un
  ensemble de fichiers immuables : FST des suffixes (clés inchangées :
  partitions 0x00 / 0x01 / 0x02, minuscules, chunks de ≤ 8 octets + overlap
  de 2), table de parents, textes + méta, table de fratrie. Les identifiants
  de tokens sont **globaux au shard** et attribués une fois pour toutes.
- **Des segments d'occurrences** qui ne portent plus de dictionnaire :
  `.sfxpost`, `.word_sfxpost`, `.posmap`, `.word_pos_map` indexés par
  identifiant global, plus le docstore et les fichiers tantivy.
- **Un commit** : les textes nouveaux (jamais vus dans le shard) forment une
  nouvelle génération ; les occurrences forment un segment. Les deux sont
  écrits atomiquement.
- **Une requête** : prescan sur chaque génération vivante (N petit), union
  des candidats par identifiant global, puis postings de chaque segment.
- **Une fusion de segments** : concaténation des postings par identifiant
  global, sans remappage. **Une compaction de dictionnaire** : fusion de
  tables triées de générations, sans remappage non plus, puisque les
  identifiants ne bougent pas — les vieilles générations deviennent
  inutiles quand plus aucun segment vivant ne les référence… en fait elles
  restent toutes nécessaires tant que leurs termes existent : la compaction
  ne supprime rien, elle réunit.

C'est le design de `sparse_vector` du 27 août (dimension = token id
global, table triée par segment, `merge_segments` qui marche les tables
ensemble, commit atomique temporaire + `rename` + `sync`, tombstones).
Design : `docs/27-08-2026/01-design-sparse-segments-dimension-globale.md`.

### 2.2 Ce qui doit changer avec, et pourquoi c'est une seule refonte

**L'ordinal sur 24 bits.** Le mot de parent (`builder_v3.rs`, en-tête
« V3 encoding layout ») encode `ordinal(24) | sti(12) | own_len(14) |
sep_len(8) | overlap(4) | ws(1)` dans 63 bits, pour le cas « un seul
parent » stocké inline dans la valeur FST. Un dictionnaire de shard sur le
noyau entier a 15 à 20 M de textes distincts : **ça ne rentre pas**. Ce
soir la fusion l'a prouvé deux fois : `merge_segments_v3` refuse au-delà de
16 777 216 (« 17 898 500 distinct terms across 4 segments »).

**La voie propre** : tous les parents en table (même le parent unique), la
valeur FST n'est plus qu'un offset dans la table. Le record delta de
l'étape 8 est déjà à taille variable ; un ordinal en varint y prend ce
qu'il faut. Coût : un déréférencement de plus pour les clés à parent
unique (aujourd'hui 8 octets inline) — à mesurer, mais la FST perd 8 octets
par clé finale (`output_pack_size`, `lucivy-fst/src/raw/node.rs`) au
profit d'un offset plus court.

**L'overlap dans la valeur.** La même table à taille variable peut porter
les deux octets d'overlap par parent, ce qui permet d'arrêter les clés à
`own_len` et de supprimer les marqueurs (40 % des clés de chunks). C'est
la piste 5b abandonnée ce matin pour une raison de vitesse : sans
marqueur, la frontière `_` obligerait à décoder 54 747 parents **et** lire
`.termtexts` pour chacun. Avec l'overlap dans le record, le filtre est un
compare de deux octets en séquentiel dans une liste déjà décodée. Ce qui
reste risqué : le scan de plage (`fst_candidates_v3`) d'un trigramme qui
enjambe une frontière ne le trouve plus par une clé directe ; il faut
combiner scan et marche. À concevoir avec un compte des cas.

Donc **une** refonte du `.sfx` : ordinaux larges, parents tous en table,
overlap en valeur, clés sans marqueur. Elle est indépendante du partage par
shard dans son code, mais le partage l'exige.

### 2.3 Ordre proposé

1. **Mesurer** (§4) avant de coder quoi que ce soit.
2. **Refonte du `.sfx`** seule, par segment comme aujourd'hui, mesurée
   avec le protocole du jour (taille, panel vérifié, A/B au même binaire
   puis par commit). Elle rapporte déjà : parents uniques en table, plus de
   marqueurs.
3. **Dictionnaire par shard**, d'abord **une seule génération** rebâtie à
   chaque commit (simple, lent, juste) pour prouver la justesse et mesurer
   le gain de taille ; puis les générations ; puis la compaction.
4. Deltas et snapshot (§3.5), bindings, WASM.

---

## 3. Ce que je sais déjà du code concerné

### 3.1 Le collecteur — `src/suffix_fst/collector_v3.rs`

- `SfxCollectorV3` : `begin_doc` / `add_value` / `end_doc` / `into_data`.
  Tokenise par `segment_and_chunk` (`src/tokenizer/equal_chunk.rs`,
  `DEFAULT_MAX_TOKEN = 8`, `is_content_char` = non-ASCII ou alphanumérique),
  étend chaque chunk des 2 premiers octets du suivant (`DEFAULT_OVERLAP`).
- **Internement local au segment** : `token_intern` (clé = texte + forme),
  `token_texts`, `token_meta` (`own_len`, `sep_len`, `overlap_len`,
  `is_word_start`, `is_word_stripped`), `intern_to_final`. C'est **ici** que
  le dictionnaire de shard s'insère : l'internement doit consulter le shard.
- Deux espaces d'ordinaux dans un seul numérique (`into_data`) : chunks
  (partitions 0x00/0x01, postings `.sfxpost`) et mots dépouillés (0x02,
  `.word_sfxpost`). L'audit ([02](02-audit-taille-index-sfx-v3.md) §1)
  détaille.
- `sibling_pairs: Vec<(u32, u32)>` depuis l'étape 4 (plus de longueur).
- `mem_usage()` contre `LUCIVY_SFX_HEAP` (1 Go natif / 128 Mo wasm)
  coupe un segment. Le pic du builder : ≈ 56 octets par entrée FST
  (`raw_ordinal` est un `u64`, `entries` + `keyed` pendant le tri).

### 3.2 Le builder et le fichier — `builder_v3.rs`, `file_v3.rs`

- `SuffixFstBuilderV3::add_token` : une clé par suffixe `si < own_len`
  (étape 5a), plus un **marqueur** tronqué à `own_len − si` pour chaque
  suffixe qui déborde dans l'overlap. `add_word_stripped` : suffixes du
  contenu du mot + overlap de contenu (≤ 256).
- `build()` : tri sur préfixe 8 octets puis clé, dédoublonnage, un record
  par clé multi-parents (`encode_parent_entries_v3`, version 5 : varint
  count, Δordinal, sti, own_len, sep_len, flags), parent unique inline.
  Refus explicite au-delà de 24 bits d'ordinal, 12 de sti, 14 d'own_len.
- `SfxFileReaderV3` : conteneur de sections (`section_file.rs`), magic
  `SFX3`, octet de version 3 / 4 / 5, `decode_parents` dispatche. Lecture
  zéro copie sur `OwnedBytes`.
- `measure_parents_by_key_length` (test ignoré, `SFX_FILE=…`) : parents par
  longueur de clé. Sur un segment de 30 000 fichiers : clés d'un octet,
  1 549 parents en moyenne, 54 747 au maximum ; deux octets, 57 / 6 726.

### 3.3 Le DAG de construction et la fusion — `src/indexer/sfx_dag_v3.rs`

- `build_initial_sfx_dag_v3` : collecte → `BuildFstV3Node` → postings →
  `AssembleV3Node` (termtexts via `TermTextsWriterV3::from_collector_v3`,
  index dérivés via `build_derived_indexes_v3` = posmap seulement depuis
  l'étape 2, plus `word_pos_map`, `word_sfxpost`, `sibling_v3`).
- `merge_segments_v3` : **réinterne** les textes de `.termtexts` des
  sources (arène + table ouverte, clé = forme + texte), remappe les
  postings et la fratrie, puis reconstruit tout par le même DAG. Ne
  re-tokenise pas. Deux textes identiques de forme différente gardent deux
  ordinaux. Bornée par les 24 bits (message d'erreur explicite).
- Avec un dictionnaire de shard, la fusion devient : concaténer les
  postings par identifiant global, dériver `posmap` / `word_pos_map`. Le
  réinternement disparaît.

### 3.4 La requête — `src/suffix_fst/briques/`

- `BriquesContext` (`context.rs`) : `reader` (FST), `resolver` (postings),
  `posmap`, `word_sfxpost`, `sibling_v3`, `termtexts`, `word_posmap`. Plus
  de `bytemap` depuis l'étape 2 ; `has_word_pipeline` exige `termtexts`.
- Chargé par `contains_query_v3.rs`, `fuzzy_query_v3.rs`,
  `regex_query_v3.rs` (`load("posmap")` etc.) **par segment** : c'est là
  qu'un dictionnaire par shard change la structure — le contexte porterait
  N lecteurs de génération et un résolveur par segment.
- `fst_walk.rs` : `fst_candidates_v3` (scan de plage sur préfixe),
  `walk_partition` + `check_split` + `overlap_lookahead` (marche octet par
  octet, décode les parents à chaque nœud final, coupe à `own_len − sti`),
  `sibling_chain_dfs` (lit `termtexts.meta()` pour le pas, étape 4).
- `resolve.rs` : adjacence stricte (posmap) et relâchée (posmap +
  `termtexts.has_content`), chaînes de mots via `word_posmap`.
- `composite.rs` : fuzzy (générateurs `pieces` / `pivot`, `auto` exclut
  `pivot` en séparateurs relâchés depuis 3.0.7), fenêtre reconstruite via
  posmap + termtexts, `V3_DIAG_FUZZY` (corrigé ce soir : coupe sur
  frontière de caractère).
- `lucivy_core/src/search_dag.rs` : le prescan crée **un nœud par
  segment**. 253 segments = 253 marches. C'est le gain de requête du
  chantier.

### 3.5 Persistance, deltas, fédération

- `lucistore/src/delta_sharded.rs` : `ShardVersion { shard_id, version,
  segment_ids }`, `ShardedDelta { shard_deltas: Vec<(usize, IndexDelta)>,
  shard_config, num_shards }`. Un delta = segments que le client n'a pas.
  Il faudra y ajouter les générations, envoyées avant les segments qui les
  citent, import atomique sur le couple.
- `lucivy_core/src/bm25_global.rs` : `ExportableStats` indexé par octets du
  terme / texte de requête / motif — **aucun ordinal ne sort d'un shard**,
  la fédération ne voit rien.
- `ShardedHandle` (`lucivy_core/src/sharded_handle.rs`) : routage par
  `balance_weight` (0,2 effectif), un `LucivyHandle` par shard. Le
  dictionnaire vit au niveau du shard, donc du handle.
- Snapshot LUCE / delta LUCID / LUCIDS : `lucistore`. Le WASM
  (`bindings/emscripten`) importe LUCE et, depuis 3.0.8, applique LUCIDS.

---

## 4. Ce qu'il faut mesurer avant de coder

| mesure | comment | ce qu'elle décide |
|---|---|---|
| répétition sur le noyau entier | le script Python de [05](05-piste-dictionnaire-partage-par-shard.md) §1 sur `idx90k-v4` (253 segments, 67 M d'ordinaux ; passer par des hachés 64 bits, pas des `set` de chaînes) | le plafond réel du gain |
| coût d'une génération | temps de `build()` pour *k* nouveaux termes (10 k, 100 k, 1 M) — `V3_PROFILE=1` imprime `[fst]` | la granularité des commits |
| coût de N générations à la requête | panel avec N FST à marcher (simulable : N segments d'un même shard aujourd'hui) | quand compacter |
| identifiant sur 4 octets dans `.posmap` | +25 % sur 4,7 % de l'index | négligeable, à écrire |
| mémoire d'internement | une table de hachage de 6 M d'entrées en RAM pendant le commit, ou consultation de la FST ? | WASM (128 Mo) |
| parents uniques en table | A/B au même binaire, requêtes exactes | le coût du déréférencement |

---

## 5. Où regarder d'abord, dans l'ordre

1. `docs/27-08-2026/01-design-sparse-segments-dimension-globale.md` — le
   précédent, et `sparse_vector/src/segments.rs` pour le code.
2. `src/suffix_fst/collector_v3.rs` `into_data` (attribution des ordinaux)
   et `src/indexer/sfx_dag_v3.rs` `merge_segments_v3` (réinternement).
3. `src/suffix_fst/builder_v3.rs` (en-tête « V3 encoding layout », `build()`)
   et `file_v3.rs`.
4. `lucivy_core/src/search_dag.rs` (un nœud de prescan par segment) et
   `src/query/contains_query_v3.rs` (`BriquesContext` chargé par segment).
5. `lucistore/src/delta_sharded.rs`, puis `lucivy_core/src/handle.rs`
   (ouverture, `detect_sfx_version_of`) et `sharded_handle.rs`.
6. [02](02-audit-taille-index-sfx-v3.md) pour l'inventaire exact de chaque
   fichier, et [03](03-journal-des-etapes.md) pour chaque format écrit
   aujourd'hui.
