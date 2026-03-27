# Doc 01 — État des lieux : cross-token search + sibling links

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Ce qui fonctionne

### Sibling links (nouveau)
- Chaque token indexé stocke ses successeurs (ordinal + gap_len) dans une sibling table
- gap_len=0 → contigu (CamelCaseSplit) → cross-token viable
- gap_len>0 → séparateur (espace, ponctuation) → multi-token viable
- Stocké dans le .sfx, reconstruit par le merger
- Coût : ~6 bytes/paire, ~30KB pour 10K tokens

### Cross-token exact (single-token path)
- `falling_walk(query)` → premier split (any SI)
- Sibling chain walk → O(1) par step via pointer chase
- `ord_to_term` → texte du token suivant depuis le term dict
- Byte continuity check (`byte_to == byte_from`) → pas de faux positifs
- Adjacency via Vec trié + partition_point (pas de HashMap)
- **Performance** : ~15ms sur 5k docs, ~0.5ms sur 846 docs

### Multi-token + cross-token unifié
- Chaque sous-token résolu via `cross_token_resolve_for_multi()`
- Falling walk + sibling chain pour chaque sous-token (pas seulement le dernier)
- `MultiTokenPosting` avec span (nombre de positions occupées)
- Adjacency : `token_index + span == next.token_index`
- `ord_to_term` propagé depuis `run_sfx_walk` → prescan + scorer
- "use rag3weaver", "getElementById function" → fonctionnent

### Highlight fix
- Bug UTF-16 surrogate pairs dans le playground JS : 4-byte UTF-8 = 2 JS chars
- Fix : `charIdx += (len === 4) ? 2 : 1` dans le byteToChar mapping
- 24/24 highlights corrects sur le doc de test

## Ce qui ne fonctionne PAS

### Fuzzy cross-token
- 3 tests multi-token fuzzy échouent
- `cross_token_resolve_for_multi` ne gère pas `fuzzy_distance > 0`
- Le falling_walk est exact → les splits sont exact
- Le prefix_walk est exact → le terminal est exact
- La sibling chain check est exact (`starts_with` / `==`)

### Fuzzy single-token cross-token
- `suffix_contains_single_token_fuzzy` fait le fallback sur `cross_token_search`
- Mais `cross_token_search` passe `fuzzy_distance` au remainder walk (prefix_walk_si0)
- Ce path ne passe PAS par les sibling links → tombe sur l'ancien code (falling_walk only)

### Regex cross-token
- `RegexContinuationQuery` existe mais est lent (FST search complet avec DFA)
- Pas intégré avec les sibling links

### startsWith
- Séparé du contains : utilise `prefix_only=true`
- Pourrait être unifié : contains avec SI=0 forcé

## Architecture actuelle

```
Query "rag3weaver" (single token, d=0)
  → suffix_contains_single_token_with_terms()
    → suffix_contains_single_token_inner() → SFX walk → match? return
    → cross_token_search_with_terms() → falling_walk + sibling chain → return

Query "use rag3weaver" (multi-token, d=0)
  → run_sfx_walk() avec ord_to_term
    → suffix_contains_multi_token_impl_pub()
      → cross_token_resolve_for_multi("use", is_first=true, is_last=false)
        → resolve_suffix + falling_walk + sibling chain
      → cross_token_resolve_for_multi("rag3weaver", is_first=false, is_last=true)
        → prefix_walk + falling_walk + sibling chain
      → pivot + adjacency avec spans

Query "rag3weavr" (single token, d=1) — CASSÉ
  → suffix_contains_single_token_fuzzy()
    → fuzzy_inner → SFX fuzzy walk → peut trouver "rag3weavr"≈"rag3" mais pas le cross-token
    → cross_token_search() → falling_walk exact → sibling chain exact → pas de fuzzy terminal
```

## Fichiers clés

| Fichier | Rôle |
|---------|------|
| `src/suffix_fst/sibling_table.rs` | SiblingTableWriter/Reader |
| `src/suffix_fst/collector.rs` | Collecte paires sibling dans end_value() |
| `src/suffix_fst/file.rs` | SfxFileWriter/Reader avec sibling data |
| `src/indexer/sfx_merge.rs` | merge_sibling_links() |
| `src/indexer/sfx_dag.rs` | MergeSiblingLinksNode dans le DAG |
| `src/query/phrase_query/suffix_contains.rs` | cross_token_search_with_terms, cross_token_resolve_for_multi |
| `src/query/phrase_query/suffix_contains_query.rs` | run_sfx_walk avec ord_to_term |
| `playground/index.html` | buildSnippets byteToChar mapping |

## Tests

- 1169 tests ld-lucivy OK (3 fuzzy multi-token FAIL attendu)
- 88 tests lucivy-core OK
- Test diag .luce : 24/24 highlights corrects
- Test Node.js : mapping byte→char vérifié
