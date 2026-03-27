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

### Fichiers standard (hérités)

| Fichier | Description |
|---|---|
| `.term` | Term dictionary — `ord_to_term(ord)` → texte du token, `term_ord(text)` → ordinal |
| `.pos` | Positions dans les postings standard |
| `.store` | Stored fields (texte original complet) |
| `.fast` | Fast fields (valeurs numériques) |

## Combinaisons clés pour la recherche

### Exact contains
```
prefix_walk(query) → parents → resolve → matches
  + sibling chain (gap=0) pour cross-token
```

### Fuzzy contains
```
fuzzy_falling_walk(query, d) → split candidates
  + sibling chain → resolve-last
```

### Regex contains (actuel)
```
extract_all_literals(pattern) → pick best by doc_freq
  prefix_walk(literal) → DFA validate ordinal-level
  multi-literal intersection via has_doc O(log n)
  position ordering filter (byte offsets)
  Phase 3c gap>0 continuation (LENT pour .*)
```

### Regex contains (cible avec PosMap)
```
extract_all_literals → resolve chaque → intersection + position ordering
  PosMap: lire ordinals entre les deux positions
  ord_to_term + GapMap: reconstruire le texte
  Feed DFA sur le texte reconstruit → O(distance)
  ByteBitmap: pré-filtre rapide sur chaque token
```

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
