# Knowledge Dump — SFX Engine Implementation Complete

**Date** : 16 mai 2026  
**Objectif** : référence complète de l'implémentation actuelle (v2) pour le prototype v3.

---

## 1. SFX Builder (src/suffix_fst/builder.rs)

### Output u64 encoding

```
Single parent (bit 63 = 0) :
  [55..40] token_len   16 bits (max 65535)
  [39..24] si           16 bits (suffix index)
  [23..0]  raw_ordinal  24 bits (~16M tokens)
  [62..56] LIBRES       7 bits

Multi parent (bit 63 = 1) :
  [62..0]  offset       63 bits dans l'OutputTable
```

### Multi-parent OutputTable format

```
[u16 LE]  num_parents
[u32 LE]  raw_ordinal  ×
[u16 LE]  si           × num_parents (8 bytes par entrée)
[u16 LE]  token_len    ×
```

Trié par si (SI=0 en premier pour early exit).

### Constantes

- `SI0_PREFIX = 0x00` — partition début de token
- `SI_REST_PREFIX = 0x01` — partition substring
- `MAX_CHUNK_BYTES = 256` — profondeur max suffixe
- `RAW_ORDINAL_MASK = 0x00FF_FFFF` — 24 bits
- `min_suffix_len` : configurable via `LUCIVY_MIN_SUFFIX_LEN` env var (défaut 1)

### Algorithme add_token

1. Lowercase le token
2. Pour si = 0 à min(token.len(), 256) :
   - Skip si pas sur une frontière char UTF-8
   - Skip si suffix < min_suffix_len (sauf si=0)
   - Prefix byte : 0x00 si si=0, 0x01 si si>0
   - Écrit `[prefix_byte][suffix_bytes]` dans un buffer contigu (key_buf)
   - Enregistre (key_start, key_len, ParentEntry{ordinal, si, token_len})

### Algorithme build

1. Sort entries par (clé suffix, ordinal, si)
2. Dedup les triples identiques
3. Group par clé suffix :
   - 1 parent → encode_single_parent inline dans le u64
   - 2+ parents → encode_parent_entries dans OutputTable, encode_multi_parent(offset) dans le u64
4. MapBuilder du FST consomme les groupes triés
5. Retourne (fst_bytes, output_table_bytes)

---

## 2. SFX File Format (src/suffix_fst/file.rs)

### Header v1 (53 bytes)

```
[0..4]    magic "SFX1"
[4]       version = 1
[5..9]    num_docs: u32
[9..13]   num_suffix_terms: u32
[13..21]  fst_offset: u64
[21..29]  fst_length: u64
[29..37]  parent_list_offset: u64
[37..45]  parent_list_length: u64
[45..53]  gapmap_offset: u64
```

### Sections

- **A** : FST (fst_offset..fst_offset+fst_length)
- **B** : Parent lists / OutputTable (parent_list_offset..parent_list_offset+parent_list_length)
- **C** : Sibling table (parent_list_end..gapmap_offset) — optionnel
- **D** : GapMap (gapmap_offset..EOF)

### Falling walk (lines 388-430)

```
Pour chaque partition (0x00, 0x01) :
  node = racine FST
  Suivre le prefix byte → transition
  Pour chaque byte[i] de la query :
    Chercher transition pour byte[i] dans node
    Si pas trouvé : break
    Avancer au noeud suivant, accumuler output
    Si node.is_final() :
      Décoder parents depuis output
      Pour chaque parent :
        Si parent.si + (i+1) == parent.token_len :
          → SPLIT POINT : ajouter SplitCandidate(prefix_len=i+1, parent)
  Trier candidats par prefix_len décroissant
```

**Coût** : O(query_len) par partition = O(2 × query_len) total.

### Fuzzy falling walk (lines 440-502)

```
Si distance=0, fallback sur falling_walk exact.
Sinon :
  Construire Levenshtein DFA pour la query
  Pour chaque partition :
    DFS avec pile : (fst_addr, fst_output, dfa_state, fst_depth)
    Pour chaque noeud :
      Si final ET fst_depth > 0 :
        Décoder parents, vérifier si + fst_depth == token_len
        → SplitCandidate(prefix_len=fst_depth)
      Pruning : si DFA.can_match(state) == false, skip
      Explorer toutes les transitions FST :
        next_dfa = dfa.accept(state, byte)
        Push (next_addr, next_output, next_dfa, fst_depth+1)
  Trier par prefix_len décroissant
  Dedup par (prefix_len, raw_ordinal)
```

**Point critique** : utilise `fst_depth` (bytes dans le FST) comme split point, PAS la position dans la query (car les edits décalent les positions).

### Prefix walk

Pour startsWith / range scan :
- Construit ge_key = [prefix_byte] + prefix
- Construit lt_key = increment_prefix(ge_key)
- Range scan FST : ge(ge_key).lt(lt_key)
- Strip prefix byte des résultats

---

## 3. GapMap (src/suffix_fst/gapmap.rs)

### Format per-document

```
[u16 LE]  num_tokens
[u8]      num_values (1 = single, >1 = multi)
[opt]     value_offsets table (si multi) : (seq_start: u16, ti_start: u32) × num_values
[...]     gaps encodés séquentiellement
```

### Encoding des gaps

```
len = 0..253  : [u8 len][bytes × len]        normal
len = 254     : [u8 254]                      VALUE_BOUNDARY (pas de bytes)
len ≥ 254     : [u8 255][u16 LE ext_len][bytes × ext_len]  extended
```

### Layout single-value

Pour N tokens : N+1 gaps
```
gap[0] = prefix avant token 0
gap[1..N] = séparateurs entre tokens
gap[N] = suffix après dernier token
```

### Layout multi-value

Pour V values avec T_i tokens chacune :
```
value 0 : prefix, sep[0..T_0-1], suffix  (T_0+1 gaps)
VALUE_BOUNDARY
value 1 : prefix, sep[0..T_1-1], suffix  (T_1+1 gaps)
...
Total gaps = num_tokens + num_values
```

### read_separator(doc_id, ti_a, ti_b)

- ti_b doit être ti_a + 1 (consécutifs)
- Retourne None si pas consécutifs ou si VALUE_BOUNDARY
- Retourne les bytes séparateurs entre les deux tokens

---

## 4. SepMap (src/suffix_fst/sepmap.rs)

### Format

```
[4 bytes]   magic "SMAP"
[u32 LE]    num_ordinals
[32 bytes × num_ordinals]  bitmaps (256 bits chacun)
```

Bit N = byte value N observé comme séparateur après cet ordinal.
Bit 0 (CONTIGUOUS_FLAG) = token observé avec gap=0 (contigu).

### Méthodes clés

- `sep_bytes_in_ranges(ordinal, ranges)` : tous les bytes sep de cet ordinal sont dans les ranges ?
- `has_contiguous(ordinal)` : bit 0 set ?
- `only_contiguous(ordinal)` : SEUL bit 0 set (jamais de séparateur réel) ?

---

## 5. Sibling Table (src/suffix_fst/sibling_table.rs)

### Format

```
[u32 LE]    num_ordinals
[u32 LE × (num_ordinals+1)]  offset table
[...]       entries data : [u32 next_ordinal][u16 gap_len] × N
```

6 bytes par SiblingEntry. Trié par (ordinal, next_ordinal, gap_len), dédupliqué.

### Lookup

- `siblings(ordinal)` → Vec<SiblingEntry> (tous les successeurs)
- `contiguous_siblings(ordinal)` → Vec<u32> (seulement gap_len=0)

---

## 6. SFX Collector (src/suffix_fst/collector.rs)

### Flow par document

```
begin_doc()
  Pour chaque value :
    begin_value(text)
    add_token(text, offset_from, offset_to) × N
    end_value()
      → compute gaps
      → record sibling pairs
      → record sepmap bytes
end_doc()
  → write gapmap
```

### end_value détails

1. Collecter siblings : pour chaque paire consécutive (i, i+1), gap_len = tokens[i+1].from - tokens[i].to
2. Calculer gaps : prefix, séparateurs, suffix = N+1 gaps
3. Enregistrer dans sepmap : chaque byte de chaque séparateur
4. Avancer le compteur Ti : +num_tokens + 1 (le +1 = VALUE_BOUNDARY slot)

### into_data (remapping pour DAG)

1. Trier les textes tokens → BTreeSet order
2. Construire mapping intern_ordinal → final_ordinal
3. Remapper sepmap bitmaps (OR-merge)
4. Retourner SfxCollectorData pour le DAG de merge

---

## 7. Fuzzy Contains (src/query/phrase_query/fuzzy_contains.rs)

### Pipeline complet

```
1. concat_query(query) → strip non-alphanum, lowercase
2. count_words(query) → nombre de segments alphanum
3. boundary_positions(query) → positions des frontières dans concat
4. generate_trigrams(concat, distance) → (ngrams, positions, n_size)
     n=2 si len ≤ 3*(d+1), sinon n=3
5. boundary_trigram_indices(trigrams, boundaries) → indices des trigrams cross-boundary

Phase A — Estimation sélectivité (PAS de resolve postings) :
6. Pour chaque trigram :
     fst_candidates(sfx, trigram) → count
     cross_token_falling_walk(sfx, trigram, ...) → chains count
     selectivity = candidates + chains
7. Trier trigrams par sélectivité croissante (plus rare = premier)

Phase B — Résolution :
8. B1 : résoudre les trigrams les plus rares SANS filtre → doc_filter (union des doc_ids)
9. B2 : résoudre les trigrams restants AVEC doc_filter

10. build_hits_by_doc(matches) → HashMap<DocId, HashMap<position, Vec<TrigramHit>>>
11. find_matches(hits, threshold, max_span) → Vec<FuzzyMatch>
      Sliding window par doc, two-pointer
      miss_count = scorable_total - matched_non_boundary_trigrams
12. Scoring : -(miss_count as f32) dans doc_coverage
      → scorer fait miss_penalty * 1000 + bm25
```

### Constantes clés

- threshold = max(ngrams.len() - n*distance - broken_by_boundaries, min_threshold)
- broken_by_boundaries = (n-1) * num_word_boundaries
- max_span = max(num_words, concat_len/4 + 1) + distance
- min_threshold = 1 (au moins 1 trigram doit matcher)

---

## 8. Suffix Contains (src/query/phrase_query/suffix_contains.rs)

### Single-token exact

1. prefix_walk (ou prefix_walk_si0 si anchor_start) sur la query
2. Pour chaque parent, résoudre les postings via raw_term_resolver
3. Ajuster byte_from += parent.si

### Cross-token (sibling chain DFS)

1. falling_walk / fuzzy_falling_walk → SplitCandidates
2. Pour chaque candidat, vérifier existence de contiguous siblings
3. DFS avec worklist : (current_ord, remainder, chain, depth)
   - Max depth = 8
   - Fast skip : premier byte du sibling ≠ premier byte du remainder
   - Match types : exact (terminal), prefix (terminal), partial (continue DFS)
4. Vérification adjacence : pour chaque chaîne validée :
   - Active set : (doc_id, next_position, byte_to_prev, byte_from_match, first_pos)
   - Pour chaque ordinal suivant : trouver posting avec (doc_id, position+1) ET byte_from == byte_to_prev

### Multi-token

1. Résoudre chaque sous-token indépendamment (prefix_walk ou fuzzy_walk)
2. Pivot optimization : le sous-token le plus sélectif en premier
3. Construction bidirectionnelle : backward puis forward depuis le pivot
4. Validation séparateurs via GapMap

---

## 9. Regex Continuation (src/query/phrase_query/regex_continuation_query.rs)

### Routing

- d=0, regex=false → SuffixContainsQuery (exact)
- d>0, regex=false → fuzzy_contains via run_fuzzy_prescan
- d=0, regex=true → regex via run_regex_prescan
- d>0, regex=true → Levenshtein DFA

### Regex pipeline

1. extract_literals_with_gaps(pattern) → (literals, gap_kinds)
2. Pour chaque literal : fst_candidates + cross_token_falling_walk → sélectivité
3. Résoudre sélectivement (plus rare en premier)
4. Pour chaque match : alimenter le DFA byte par byte
   - Littéral → DFA advance
   - Gap → si AcceptAnything, skip. Si NeedsValidation, walk le DFA à travers les bytes de gap
5. Si DFA accepte → match

### Fuzzy pipeline

1. fuzzy_contains() → (doc_tf, highlights, doc_coverage)
2. doc_coverage = -(miss_count as f32) pour tiering par qualité

### search_continuation (dans term_dictionary.rs)

Pour le regex cross-token :
1. Wrap automaton avec PrefixByteContinuationAutomaton (can_match = is_match)
2. Stream FST.search(wrapped)
3. Pour chaque résultat : re-walk le DFA pour obtenir l'end_state
4. Retourne ContinuationMatch{ordinal, si, end_state, is_accepting}
5. Le caller alimente le gap, avance le DFA, rappelle search_continuation

---

## 10. Literal Pipeline (src/query/phrase_query/literal_pipeline.rs)

### Briques composables

| Brique | Input | Output | Coût |
|--------|-------|--------|------|
| fst_candidates | sfx + literal | Vec<FstCandidate> | O(FST scan) ~0.01ms |
| cross_token_falling_walk | sfx + literal + distance | Vec<CrossTokenChain> | O(falling walk + sibling DFS) ~1-50ms |
| resolve_candidates | candidates + resolver | Vec<LiteralMatch> | O(posting I/O) ~1-100ms |
| resolve_chains | chains + resolver | Vec<LiteralMatch> | O(posting I/O × chain_len) ~1-100ms |

### Pattern de résolution sélective

```
1. Estimer sélectivité pour TOUS les trigrams (cheap : fst_candidates + falling walk counts)
2. Trier par sélectivité croissante
3. Résoudre le plus rare SANS filtre → seed doc_filter
4. Résoudre les suivants AVEC doc_filter → intersection progressive
```

Ce pattern est utilisé identiquement par fuzzy_contains, regex_contains, et multi-token search.

### Cross-token falling walk variantes

- `cross_token_falling_walk()` : allow_gaps=false → contiguous_siblings only (gap=0)
- `cross_token_falling_walk_any_gap()` : allow_gaps=true → tous les siblings

---

## 11. Fichiers par segment (.sfx index)

| Fichier | Contenu | Taille typique |
|---------|---------|----------------|
| `.sfx` | FST + parent lists + sibling table + GapMap | ~60% de l'index |
| `.sfxpost` | Posting lists (ordinal → doc_ids) | ~25% |
| `.termtexts` | Token texts pour cross-token resolution | ~10% |
| `.gapmap` | Séparateurs per-doc per-token | ~3% |
| `.sepmap` | Bitmap separator bytes per ordinal | ~2% |

---

## 12. Ce qui change en v3 (résumé des docs 04/05)

| Composant | v2 actuel | v3 proposé |
|-----------|-----------|------------|
| Tokenizer | Split séparateurs + CamelCase | Split séparateurs + **max-len** |
| Trailing sep | Jetés | **Absorbés** dans le token |
| Overlap | Aucun | **2 bytes** du token suivant dans le SFX builder |
| SI encoding | si (position dans token) | **STI** (position dans token étendu) |
| is_word_start | N/A | **1 bit** dans output u64 |
| Sibling table | DFS sur (ordinal, next_ord, gap_len) | **Supprimée** → next_token=TI+1, next_word table |
| GapMap | Stocke séparateurs per-doc | **Supprimé** (sep dans les tokens) |
| SepMap | Bitmap per-ordinal | **Supprimé** (sep dans le FST) |
| Fuzzy | concat_query + boundary trigrams + sibling DFS | **Query telle quelle**, 0 boundary, overlap couvre tout |
| Falling walk | Split → sibling DFS | Split → **TI+1** (trivial) |
| Word map | N/A | **Nouveau** : token↔word mapping pour BM25 |
| strict_separators | Implicite (gap validation) | **Post-FST filtering** |
