# Arsenal lucivy — structures d'indexation disponibles à la recherche

Inventaire de tout ce qui est construit pendant l'indexation et accessible au query time.

Mis à jour : 29 mars 2026

## Fichiers par segment

### `.sfx` — Suffix FST + métadonnées

Fichier principal du SFX. Contient 4 sections :

| Section | Description | API |
|---|---|---|
| **Suffix FST** | FST de toutes les suffixes de tous les tokens. Partitionné SI0 (token complet) + SI_REST (sous-chaînes). | `prefix_walk(query)` — range scan, retourne entries + parents |
| | | `prefix_walk_si0(query)` — SI=0 seulement (startsWith) |
| | | `falling_walk(query)` — walk ciblé, split candidates aux frontières de token |
| | | `fuzzy_falling_walk(query, d)` — idem avec Levenshtein DFA |
| | | `fst.search(automaton)` — intersection DFA × FST |
| **Parent list** | Pour chaque entrée SFX : quel token (ordinal), à quel SI, quelle longueur. Inline pour single-parent (bit 63=0), OutputTable pour multi-parent. | `decode_parents(val)` → `Vec<ParentEntry { raw_ordinal, si, token_len }>` |
| **Sibling table** | Graphe d'adjacence token→successeur observé pendant l'indexation. Chaque entry = (next_ordinal, gap_len). | `siblings(ordinal)` → tous les successeurs (gap=0 et gap>0) |
| | | `contiguous_siblings(ordinal)` → gap=0 seulement |
| **GapMap** | Bytes de séparation entre chaque paire de tokens consécutifs, par document. Supporte multi-value avec VALUE_BOUNDARY. | `read_separator(doc_id, ti_a, ti_b)` → `Option<&[u8]>` |
| | | `num_tokens(doc_id)` → u32 |
| | | Retourne `None` si VALUE_BOUNDARY (cross-value) |

**Note** : gapmap et sibling sont aussi écrits en fichiers séparés via le
registre (`.gapmap`, `.sibling`). Le .sfx les contient encore inline pour
backward compat. Migration à terme : les queries liront les fichiers séparés
et le .sfx ne contiendra plus que FST + parent list.

### `.sfxpost` — Posting index (ordinal → documents)

Index inversé ordinal → postings avec doc_ids triés pour binary search.

| Méthode | Complexité | Description |
|---|---|---|
| `resolve(ordinal)` | O(n) | Tous les postings : `Vec<PostingEntry { doc_id, position, byte_from, byte_to }>` |
| `resolve_filtered(ordinal, &doc_ids)` | O(k log n) | Seulement les docs demandés — skip le décodage VInt des autres |
| `has_doc(ordinal, doc_id)` | O(log n) | Existence d'un doc pour cet ordinal. **Zéro décodage payload.** |
| `doc_freq(ordinal)` | O(1) | Nombre de docs uniques. Juste lire le header. |

### `.posmap` — Position-to-ordinal map

Reverse du posting index. Pour chaque (doc_id, position) → ordinal du token.

| Méthode | Complexité | Description |
|---|---|---|
| `ordinal_at(doc_id, position)` | O(1) | Quel ordinal est à cette position dans ce doc |
| `ordinals_range(doc_id, pos_from, pos_to)` | O(distance) | Tous les ordinals dans une plage de positions |
| `num_tokens(doc_id)` | O(1) | Nombre de tokens dans le doc |

**Usage** : fuzzy DFA validation (concat tokens autour du match candidat),
regex cross-token validation (lire ordinals entre deux positions).

### `.bytemap` — Byte presence bitmap

256 bits (32 bytes) par ordinal : quels byte values apparaissent dans le texte du token.

| Méthode | Complexité | Description |
|---|---|---|
| `bitmap(ordinal)` | O(1) | Le bitmap brut (32 bytes) |
| `contains_byte(ordinal, byte)` | O(1) | Le token contient-il ce byte ? |
| `all_bytes_in_range(ordinal, lo, hi)` | O(popcount) | Tous les bytes du token sont dans [lo, hi] ? |
| `contains_all_bytes(ordinal, &[bytes])` | O(k) | Le token contient tous ces bytes ? |

**Usage** : pré-filtre regex — avant de feeder un token au DFA, vérifier que
ses bytes sont compatibles avec le pattern. Ex: `[a-z]+` →
`all_bytes_in_range(ord, b'a', b'z')`.

**Pas encore utilisé** par le regex path actuel. Optimisation à implémenter.

### `.termtexts` — Token texts par ordinal SFX

Lookup O(1) ordinal SFX → texte du token. **Requis** pour toutes les fonctions
cross-token (contient les textes dans l'espace d'ordinals SFX, pas tantivy).

| Méthode | Complexité | Description |
|---|---|---|
| `text(ordinal)` | O(1) | Texte du token à cet ordinal SFX |
| `num_terms()` | O(1) | Nombre de tokens |

Format : `TTXT` header + offset table u32 + textes concaténés UTF-8.

**Si absent** : erreur explicite. Pas de fallback sur le term dict tantivy
(ordinal mismatch → résultats incorrects).

### `.gapmap` / `.sibling` — Fichiers séparés du registre

Copies des données gapmap et sibling du .sfx, écrites comme fichiers séparés
via le registre. Pas encore lues par les queries (qui utilisent encore
`sfx_reader.gapmap()` et `sfx_reader.sibling_table()`).

**Migration prévue** : les queries liront `sfx_index_file("gapmap", field)` et
`sfx_index_file("sibling", field)` au lieu de passer par le .sfx.

### Fichiers standard (hérités)

| Fichier | Description |
|---|---|
| `.term` | Term dictionary — **ses ordinals ≠ ordinals SFX**. Ne PAS utiliser `ord_to_term()` avec des ordinals SFX. |
| `.pos` | Positions dans les postings standard |
| `.store` | Stored fields (texte original complet) |
| `.fast` | Fast fields (valeurs numériques) |

## Combinaisons clés pour la recherche

### Exact contains (single token)
```
prefix_walk(query) → parents → resolve → matches
```
**Requiert** : SuffixFst, SfxPost

### Exact contains (cross-token via sibling links)
```
1. prefix_walk(query) → essaie single-token d'abord
2. Si 0 résultat → falling_walk(query) → split candidates
3. sibling_table[ordinal] → successeurs contigus (gap=0)
4. DFS worklist : explore TOUTES les branches de siblings
5. ord_to_term(next_ord) via TermTexts → texte du token suivant
6. remainder.starts_with(next_text) → chaîner ou terminal
7. Resolve les ordinals de la chaîne valide
8. Adjacency check via byte continuity
```
**Requiert** : SuffixFst, SfxPost, SiblingTable, TermTexts

### Fuzzy contains d>0 (via trigram pigeonhole)
```
1. generate_ngrams(query, distance) → bigrammes/trigrammes
2. find_literal(ngram) → matches per-doc via SFX cross-token
3. intersect_trigrams_with_threshold → candidats filtrés (+ si propagé)
4. Build concat text (tokens via PosMap + ord_to_term via TermTexts)
5. DFA Levenshtein sliding window sur le concat
6. Byte-exact highlight mapping via content_byte_starts table
   (ancre = first_bf - first_si = token start dans le content)
```
**Requiert** : SuffixFst, SfxPost, PosMap, TermTexts, GapMap

### Regex contains
```
1. extract_all_literals(pattern) → littéraux du regex
2. find_literal(lit) → matches via SFX cross-token
3. Multi-literal intersection + position ordering
4. PosMap: lire ordinals entre les positions
5. ord_to_term via TermTexts + GapMap: reconstruire le texte
6. Feed DFA regex sur le texte reconstruit
7. ByteBitmap: pré-filtre rapide sur chaque token (PAS ENCORE UTILISÉ)
```
**Requiert** : SuffixFst, SfxPost, PosMap, TermTexts, GapMap
**Optionnel** : ByteMap (pré-filtre, pas encore câblé)

### BM25 prescan (DAG)
```
PrescanShardNode → run_regex_prescan / run_fuzzy_prescan par segment
MergePrescanNode → fusionne caches + freqs
BuildWeightNode → injecte dans le Weight avant compilation
```
Cache : `CachedRegexResult { doc_tf, highlights }` per segment.

## Registre SfxIndexFile

Toutes les structures ci-dessus (sauf .sfx lui-même) sont gérées par le
registre (`all_indexes()` dans `index_registry.rs`). Ajouter un nouveau
fichier d'index = 1 struct + impl `SfxIndexFile`.

Le registre garantit :
- **Build** : appelé automatiquement par `SfxCollector::build()`
- **Merge** : doit être fait dans TOUS les chemins de merge (segment_writer,
  merger.rs N-way, merger.rs fallback, sfx_dag.rs WriteSfxNode)
- **GC** : `all_components()` protège automatiquement les fichiers
- **Load** : `load_sfx_files()` charge automatiquement via `open_read_custom`

## Optimisations possibles

### 1. ByteMap pré-filtre pour regex (pas encore câblé)

Le `.bytemap` est construit et stocké mais jamais utilisé par le regex path.
Avant de feeder chaque token au DFA regex, on pourrait vérifier que ses bytes
sont compatibles avec le pattern. Pour `[a-z]+`, ça éliminerait tous les tokens
avec des chiffres ou ponctuation sans même lancer le DFA.

**Impact estimé** : réduction 30-50% du temps DFA pour les regex restrictifs.

### 2. PosMap pré-filtre pour fuzzy

Le fuzzy actuel construit un concat de tokens autour du candidat trigram et
lance un DFA sliding window sur tout le concat. On pourrait d'abord vérifier
via PosMap que les tokens dans la fenêtre contiennent les bytes attendus
(via ByteMap) avant de construire le concat.

### 3. Threshold adaptatif pour queries courtes

Bug E de doc 12 : `threshold = max(2, computed)`. Pour queries ≤ 4 chars
avec d=1, le threshold est trop haut (2 bigrams doivent matcher, mais 1 peut
être cassé par l'edit). Fix : `threshold = max(1, computed)` pour queries
courtes.

### 4. Anchor tie-breaking pour fuzzy DFA

Bug A de doc 12 : le DFA sliding window prend le match avec le plus petit
`global_best_diff`. Mais si deux positions ont le même diff, il prend la
première (itération séquentielle). Pas de préférence pour la position la plus
proche du trigram anchor. Impact faible car le concat est petit (~8 tokens).

### 5. Migrer gapmap/sibling vers fichiers séparés

Les queries lisent encore `sfx_reader.gapmap()` et `sfx_reader.sibling_table()`
(inline dans le .sfx). Migrer vers `sfx_index_file("gapmap", field)` et
`sfx_index_file("sibling", field)`. Ensuite supprimer gapmap/sibling du .sfx
pour réduire sa taille.

### 6. Supprimer les méthodes legacy du serializer

`write_sfxpost()`, `write_posmap()`, `write_bytemap()` dans
`segment_serializer.rs` ne sont plus utilisées (tout passe par
`write_custom_index`). À supprimer dans un cleanup.

### 7. Unifier les chemins de merge via le registre

Les 3 chemins de merge (merger.rs N-way, merger.rs fallback, sfx_dag.rs)
reconstruisent chacun les fichiers manuellement. Idéalement ils utiliseraient
`SfxIndexFile::merge()` du registre. Refactor futur.

## Tailles estimées

| Structure | 862 docs | 5K docs | 90K docs |
|---|---|---|---|
| SFX FST | ~500 KB | ~2 MB | ~30 MB |
| SfxPost | ~300 KB | ~1.5 MB | ~20 MB |
| Sibling table | ~20 KB | ~100 KB | ~1 MB |
| GapMap | ~200 KB | ~1 MB | ~15 MB |
| PosMap | ~344 KB | ~2 MB | ~72 MB |
| ByteBitmap | ~160 KB | ~800 KB | ~5 MB |
| TermTexts | ~50 KB | ~250 KB | ~3 MB |
| **Total SFX** | **~1.6 MB** | **~7.7 MB** | **~146 MB** |
