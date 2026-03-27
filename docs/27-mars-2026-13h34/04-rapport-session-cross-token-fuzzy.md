# Doc 04 — Rapport session : cross-token + fuzzy via sibling links

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Résumé

Session marathon qui a abouti aux **sibling links** : chaque token stocke
ses successeurs dans le SFX. Le cross-token search suit les pointeurs O(1)
au lieu de reconstruire les relations au query time.

## Résultats finaux

### Performance exact (d=0)

| Corpus | Query | Temps |
|--------|-------|-------|
| .luce (862 docs) | rag3weaver | < 1ms |
| .luce (862 docs) | getElementById | < 1ms |
| rag3db (5k docs) | rag3weaver | ~15ms |
| rag3db (5k docs) | "osait la question : un se" | ~45ms |

### Performance fuzzy (d=1)

| Corpus | Query | Temps |
|--------|-------|-------|
| .luce (862 docs) | rak3weaver | < 100ms |
| rag3db (5k docs) | rak3weaver | ~291ms |
| 90k docs (estimé) | ? | à benchmarker |

### Correctness fuzzy (test natif, 7/8)

| Query | d | Résultat |
|-------|---|---------|
| weaver | 0 | ✓ |
| rag3weaver | 0 | ✓ |
| weavr | 1 | ✓ (deletion) |
| weavxr | 1 | ✓ (substitution) |
| rag3weavr | 1 | ✓ (right typo) |
| rak3weaver | 1 | ✓ (left typo) |
| rag3we4ver | 1 | ✓ (mid typo) |
| rag3weaverr | 1 | ✗ (insertion fin — edge case) |

## Architecture implémentée

```
Indexation:
  SfxCollector → détecte tokens contigus → SiblingTableWriter
  SfxFileWriter → écrit sibling table dans le .sfx
  Merger → merge_sibling_links avec ordinal remapping

Query exact (d=0):
  falling_walk → sibling chain (O(1)/step) → resolve → adjacency

Query fuzzy (d=1+):
  fuzzy_falling_walk (DFA × SFX) → filter par sibling présence (700→24)
  → sibling chain avec fuzzy terminal → resolve → adjacency

Multi-token:
  Chaque sous-token → cross_token_resolve_for_multi
  → falling_walk + sibling + span → pivot + adjacency

Highlights:
  Playground JS fix: surrogate pairs (4-byte UTF-8 = 2 JS chars)
```

## Fichiers modifiés (session complète)

### Nouveaux
- `src/suffix_fst/sibling_table.rs` — SiblingTableWriter/Reader
- `docs/26-mars-2026-21h05/03-13` — docs design + diagnostics
- `docs/27-mars-2026-13h34/01-04` — docs session courante
- `playground/test_highlight_mapping.mjs` — test highlight JS
- `playground/test_multibyte.mjs` — diagnostic multi-byte

### Modifiés (core)
- `src/suffix_fst/collector.rs` — collecte paires sibling
- `src/suffix_fst/file.rs` — sibling table dans .sfx + fuzzy_falling_walk fst_depth
- `src/suffix_fst/mod.rs` — export sibling_table
- `src/indexer/sfx_merge.rs` — merge_sibling_links
- `src/indexer/sfx_dag.rs` — MergeSiblingLinksNode
- `src/query/phrase_query/suffix_contains.rs` — cross_token_search_with_terms, cross_token_resolve_for_multi, levenshtein_prefix_match, MultiTokenPosting
- `src/query/phrase_query/suffix_contains_query.rs` — ord_to_term plumbing
- `src/suffix_fst/stress_tests.rs` — byte continuity corrections
- `lucivy_core/src/handle.rs` — tests diag + fuzzy
- `lucivy_core/src/query.rs` — build_contains_query
- `lucivy_core/src/search_dag.rs` — run_sfx_walk ord_to_term
- `playground/index.html` — highlight surrogate pair fix

## Prochaines étapes

### Priorité 1 : Regex cross-token via sibling links
Les sibling links donnent les successeurs de chaque token. Le regex DFA
peut suivre la chaîne au lieu de scanner tout le FST.

### Priorité 2 : Bench 90k docs
Benchmark avec LUCIVY_VERIFY sur le corpus Linux kernel.
Vérifier que exact et fuzzy sont dans les limites acceptables.

### Priorité 3 : Optimisation fuzzy — trigrams fantômes
Piste 6 du doc 03 : utiliser les suffixes du SFX comme trigrams naturels
pour filtrer les candidats AVANT le DFA walk. Potentiellement O(query_len)
au lieu de O(FST_size).

### Priorité 4 : Release
Merge feature/cross-token-search → main après bench + validation.

## Tests

- 1173 tests ld-lucivy OK (+ 7 ignored)
- 89 tests lucivy-core OK
- Test fuzzy diagnostic : 7/8 variants OK
- Test highlight : 24/24 corrects
