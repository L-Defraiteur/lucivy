# 04 — Rapport : fixes appliqués session 4 avril

Date : 4 avril 2026

---

## Bugs corrigés

### 1. Greedy chain builder — mots répétés (doc 02)

**Fichier** : `src/query/phrase_query/literal_resolve.rs` (intersect_trigrams_with_threshold)

**Problème** : quand la query contient des mots répétés ("WASM" × 2), un même byte_from dans le contenu produit des entries avec plusieurs tri_idx. Le greedy scan sautait au mauvais tri_idx et cassait la chaîne.

**Fix** : group-by byte_from — traiter les entries au même bf comme un slot, pick le plus petit tri_idx qui continue la chaîne. Si aucun ne continue, check la chaîne courante et restart.

### 2. Separator-agnostic chain building (doc 03)

**Fichier** : `src/query/phrase_query/literal_resolve.rs` (intersect_trigrams_with_threshold)

**Problème** : le span_diff global comparait les byte spans bruts incluant les séparateurs. Un séparateur `---\n#` (7 bytes) dans le contenu vs ` ` (1 byte) dans la query → span_diff = 6 > d=1 → rejeté.

**Fix** :
- `generate_ngrams` retourne maintenant un `word_ids: Vec<usize>` — chaque trigram sait de quel mot il vient
- `check_chain` : span_diff global relaxé avec tolérance +64 par transition cross-word. Proven check uniquement intra-word.

### 3. DFA gap normalization (doc 03)

**Fichiers** :
- `src/query/phrase_query/regex_continuation_query.rs` (fuzzy_contains_via_trigram)
- `src/query/phrase_query/literal_resolve.rs` (validate_path)

**Problème** : le DFA Levenshtein était feedé les vrais gap bytes du contenu. Des gaps différents de la query comptaient comme edits.

**Fix** :
- Query normalisée : tous les runs non-alphanum → single space (`normalize_query_separators`)
- Concat DFA : tout gap → single space (au lieu de extend_from_slice des vrais gap bytes)
- validate_path : tout gap → single space
- max_feed/qlen basés sur la query normalisée

## Résultats

### Query longue (le bug original)
```
"Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native" d=1
Avant : 0 résultats
Après : 1 résultat (proven=1) ✓
```

### Non-régression single-word
```
"rag3weaver" d=1 : 20 results, 31ms (inchangé)
"rak3weaver" d=1 : 20 results, 46ms (inchangé)
"rag3db" d=1     : 20 results, 69ms (inchangé)
```

### Ground truth
- Highlights : 378 → 390 (+12 matches trouvés grâce à la normalisation)
- Misses pré-existants : 3 (inchangés, liés au cross-token CamelCase — voir ci-dessous)

## Misses pré-existants (à investiguer)

3 docs pas trouvés, tous liés au même problème : **query single-word, contenu cross-token avec gap**.

| Doc | Query | Contenu | Gap | span_diff |
|-----|-------|---------|-----|-----------|
| [3] test_regex_ground_truth.rs | "rag3weaver" d=0 | tokens "rag3"+"weaver", gap ".*" | 2 bytes | 2 |
| [3] test_regex_ground_truth.rs | "rag3weaver" d=1 | tokens "rag3"+"weaver", gap ".*" | 2 bytes | 2 |
| [524] 05-todo-verif-wasm.md | "rak3weaver" d=1 | contexte markdown | ? | ? |

Le problème : pour les queries single-word (word_id=0 pour tous les trigrams), cross_word_count=0, donc la tolérance = distance seule. Mais le contenu a un gap entre tokens CamelCase qui gonfle le span_diff au-delà de distance.

La DFA validation accepterait ces cas (levenshtein("rag3weaver", "rag3 weaver") = 1 ≤ d=1), mais le chain builder les rejette avant.
