# Arsenal lucivy — structures d'indexation disponibles à la recherche

Inventaire de tout ce qui est construit pendant l'indexation et accessible au query time.

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

### `.sfxpost` — Posting index (ordinal → documents)

Index inversé ordinal → postings avec doc_ids triés pour binary search.

| Méthode | Complexité | Description |
|---|---|---|
| `resolve(ordinal)` | O(n) | Tous les postings : `Vec<PostingEntry { doc_id, position, byte_from, byte_to }>` |
| `resolve_filtered(ordinal, &doc_ids)` | O(k log n) | Seulement les docs demandés — skip le décodage VInt des autres |
| `has_doc(ordinal, doc_id)` | O(log n) | Existence d'un doc pour cet ordinal. **Zéro décodage payload.** |
| `doc_freq(ordinal)` | O(1) | Nombre de docs uniques. Juste lire le header. |

### `.posmap` — Position-to-ordinal map (NOUVEAU)

Reverse du posting index. Pour chaque (doc_id, position) → ordinal du token.

| Méthode | Complexité | Description |
|---|---|---|
| `ordinal_at(doc_id, position)` | O(1) | Quel ordinal est à cette position dans ce doc |
| `ordinals_range(doc_id, pos_from, pos_to)` | O(distance) | Tous les ordinals dans une plage de positions |
| `num_tokens(doc_id)` | O(1) | Nombre de tokens dans le doc |

**Usage principal** : regex cross-token — au lieu d'explorer les siblings à l'aveugle (Phase 3c), lire les ordinals entre deux positions connues et feeder le DFA. O(distance) au lieu de O(64 × siblings).

### `.bytemap` — Byte presence bitmap (NOUVEAU)

256 bits (32 bytes) par ordinal : quels byte values apparaissent dans le texte du token.

| Méthode | Complexité | Description |
|---|---|---|
| `bitmap(ordinal)` | O(1) | Le bitmap brut (32 bytes) |
| `contains_byte(ordinal, byte)` | O(1) | Le token contient-il ce byte ? |
| `all_bytes_in_range(ordinal, lo, hi)` | O(popcount) | Tous les bytes du token sont dans [lo, hi] ? |
| `contains_all_bytes(ordinal, &[bytes])` | O(k) | Le token contient tous ces bytes ? |

**Usage principal** : pré-filtre regex — avant de feeder un token au DFA, vérifier que ses bytes sont compatibles avec le pattern. Ex: `[a-z]+` → `all_bytes_in_range(ord, b'a', b'z')`.

### `.termtexts` — Token texts par ordinal SFX (PLANIFIÉ, PAS ENCORE IMPLÉMENTÉ)

Lookup O(1) ordinal SFX → texte du token. **Nécessaire** car les ordinals
SFX ≠ ordinals du term dict tantivy (voir bug critique doc 08).

| Méthode | Complexité | Description |
|---|---|---|
| `text(ordinal)` | O(1) | Texte du token à cet ordinal SFX |
| `num_terms()` | O(1) | Nombre de tokens |

Format : `TTXT` header + offset table u32 + textes concaténés UTF-8.

**⚠ SANS CE FICHIER, toutes les fonctions cross-token qui utilisent
`ord_to_term()` du term dict tantivy sont CASSÉES** (ordinal mismatch).

### Fichiers standard (hérités tantivy)

| Fichier | Description |
|---|---|
| `.term` | Term dictionary tantivy — **⚠ SES ORDINALS ≠ ORDINALS SFX**. Ne PAS utiliser `ord_to_term()` avec des ordinals SFX. |
| `.pos` | Positions dans les postings standard |
| `.store` | Stored fields (texte original complet) |
| `.fast` | Fast fields (valeurs numériques) |

## Combinaisons clés pour la recherche

### Exact contains (single token)
```
prefix_walk(query) → parents → resolve → matches
```
**Requiert** : SuffixFst, SuffixPost

### Exact contains (cross-token via sibling links)
```
1. prefix_walk(query) → essaie single-token d'abord
2. Si 0 résultat → falling_walk(query) → split candidates
3. sibling_table[ordinal] → successeurs contigus (gap=0)
4. ord_to_term(next_ord) → texte du token suivant    ⚠ CASSÉ (ordinal mismatch)
5. remainder.starts_with(next_text) → chaîner ou terminal
6. Resolve les ordinals de la chaîne valide
7. Adjacency check via byte continuity
```
**Requiert** : SuffixFst, SuffixPost, SiblingTable, TermTexts (⚠ pas encore implémenté)

### Fuzzy contains d>0 (via trigram pigeonhole)
```
1. generate_ngrams(query, distance) → bigrammes/trigrammes
2. find_literal(ngram) → matches per-doc via SFX cross-token
3. intersect_trigrams_with_threshold → candidats filtrés
4. Build concat text (tokens via PosMap + ord_to_term)    ⚠ CASSÉ (ordinal mismatch)
5. DFA Levenshtein validation sur le texte concaténé
6. Highlight via token mapping
```
**Requiert** : SuffixFst, SuffixPost, PosMap, TermTexts (⚠ pas encore implémenté)

### Regex contains
```
1. extract_all_literals(pattern) → littéraux du regex
2. find_literal(lit) → matches via SFX cross-token
3. Multi-literal intersection + position ordering
4. PosMap: lire ordinals entre les positions
5. ord_to_term + GapMap: reconstruire le texte    ⚠ CASSÉ (ordinal mismatch)
6. Feed DFA regex sur le texte reconstruit
7. ByteBitmap: pré-filtre rapide sur chaque token
```
**Requiert** : SuffixFst, SuffixPost, PosMap, TermTexts (⚠ pas encore implémenté), ByteMap

### BM25 prescan (DAG)
```
PrescanShardNode → run_regex_prescan / run_fuzzy_prescan par segment
MergePrescanNode → fusionne caches + freqs
BuildWeightNode → injecte dans le Weight avant compilation
```
Cache : `CachedRegexResult { doc_tf, highlights }` per segment.

## ⚠ Bug critique : ordinal mismatch (doc 08)

**TOUS les paths cross-token qui utilisent `ord_to_term()` du term dict
tantivy sont CASSÉS.** Les ordinals SFX ≠ ordinals term dict.

Le fix planifié : fichier `.termtexts` (voir ci-dessus) qui stocke les
textes dans l'espace d'ordinals SFX.

**Fonctions impactées** : `cross_token_search_with_terms`,
`find_literal`, `validate_path`, `fuzzy_contains_via_trigram`,
`regex_contains_via_literal`, `run_sfx_walk`, `run_regex_prescan`,
`run_fuzzy_prescan`.

## Tailles estimées

| Structure | 862 docs | 5K docs | 90K docs |
|---|---|---|---|
| SFX FST | ~500 KB | ~2 MB | ~30 MB |
| SfxPost | ~300 KB | ~1.5 MB | ~20 MB |
| Sibling table | ~20 KB | ~100 KB | ~1 MB |
| GapMap | ~200 KB | ~1 MB | ~15 MB |
| PosMap | ~344 KB | ~2 MB | ~72 MB |
| ByteBitmap | ~160 KB | ~800 KB | ~5 MB |
| **Total SFX** | **~1.5 MB** | **~7.4 MB** | **~143 MB** |
