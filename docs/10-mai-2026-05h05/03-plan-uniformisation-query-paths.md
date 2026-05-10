# Plan : uniformisation des chemins query + nettoyage dead code fuzzy

## État actuel des chemins (v2, SFX toujours activé)

### Chemins vivants

```
contains d=0
  → SuffixContainsQuery → run_sfx_walk
    → single token: suffix_contains_single_token_with_terms
      → prefix_walk (tous SI) → resolve postings
    → multi token: suffix_contains_multi_token_impl_pub
      → suffix_contains_single_token_inner(anchor_start) par token

startsWith d=0
  → SuffixContainsQuery + anchor_start → run_sfx_walk
    → suffix_contains_single_token_prefix (FIXÉ 10 mai 2026)
      → Path 1: single_token_inner(anchor_start=true) → prefix_walk_si0
      → Path 2: cross_token_search_with_terms → falling_walk + sibling chains, filter si=0

contains d>0 (fuzzy)
  → RegexContinuationQuery::new().with_fuzzy_distance(d)
    → run_fuzzy_prescan → fuzzy_contains::fuzzy_contains
      → generate_trigrams (pigeonhole)
      → fst_candidates (prefix_walk) + cross_token_falling_walk
      → resolve + threshold + find_matches + highlights

regex
  → RegexContinuationQuery::from_regex()
    → run_regex_prescan → continuation_score (DFA × FST walk)
    → OU regex_contains_via_literal (literal extraction + prefix_walk + DFA validation)

term (compat)  → contains d=0 + anchor_start + exact_match
fuzzy (compat) → contains d=1
phrase (compat) → contains (multi-token)
```

### Dead code fuzzy (jamais atteint en v2)

Ces fonctions ne sont jamais appelées car `distance > 0` route vers
`RegexContinuationQuery` dans `build_contains_query` (query.rs:464-476)
avant d'atteindre `SuffixContainsQuery`.

#### Dans suffix_contains.rs
- `suffix_contains_single_token_fuzzy()` — pub, 0 callers
- `suffix_contains_single_token_fuzzy_prefix()` — appelé dans run_sfx_walk:337 mais chemin mort
- `suffix_contains_single_token_fuzzy_inner()` — backing des deux ci-dessus
- `suffix_contains_multi_token_fuzzy()` — pub, 0 callers
- `suffix_contains_multi_token_fuzzy_prefix()` — pub, 0 callers

#### Dans suffix_contains_query.rs (run_sfx_walk)
- Lignes 336-341 : branches `fuzzy_distance > 0` — jamais atteintes car SuffixContainsQuery
  est toujours construit avec `fuzzy_distance=0` en v2 (le fuzzy passe par RegexContinuationQuery)

#### Dans file.rs (SFX FST reader)
- `fuzzy_walk()` — utilisé uniquement par suffix_contains_single_token_fuzzy_inner
- `fuzzy_walk_si0()` — idem
- `fuzzy_falling_walk()` — utilisé par cross_token_search_with_terms quand distance>0,
  mais ce path n'est jamais atteint en v2

#### Dans query.rs
- `build_fuzzy_query()` — FuzzyTermQuery, fallback sfx:false (obsolète en v2)

### À retirer

1. **suffix_contains.rs** : supprimer les 5 fonctions fuzzy listées ci-dessus (~200 lignes)
2. **suffix_contains_query.rs** : supprimer les branches `fuzzy_distance > 0` dans run_sfx_walk,
   retirer le champ `fuzzy_distance` de `SuffixContainsQuery` et `SuffixContainsWeight`
3. **file.rs** : supprimer `fuzzy_walk`, `fuzzy_walk_si0`, `fuzzy_falling_walk` (~150 lignes)
4. **query.rs** : supprimer `build_fuzzy_query`, simplifier le dispatch "fuzzy" (juste route vers contains d=1)

### Risques

- **sfx:false mode** : `build_fuzzy_query` (FuzzyTermQuery) est le seul chemin fuzzy sans SFX.
  Si on retire le mode sfx:false en v2, on peut tout retirer. Sinon garder uniquement `build_fuzzy_query`.
- **Tests** : certains tests unitaires dans suffix_contains.rs testent les fonctions fuzzy.
  Les retirer avec le code.
- **Bindings** : vérifier qu'aucun binding n'appelle `SuffixContainsQuery::with_fuzzy_distance(d>0)` directement.

## Uniformisation prefix_walk

### Situation actuelle

Trois fonctions font essentiellement la même chose (FST prefix walk + resolve) mais avec des variantes :

| Fonction | Walk | Filtre SI | Cross-token | Utilisé par |
|----------|------|-----------|-------------|-------------|
| `fst_candidates` (literal_pipeline) | `prefix_walk` | tous | non | fuzzy trigrams, regex literals |
| `suffix_contains_single_token_inner` | `prefix_walk` ou `prefix_walk_si0` | selon anchor_start | non | contains d=0, startsWith |
| `suffix_contains_single_token_prefix` | inner + cross_token_search | si=0 | oui | startsWith |

### Proposition : `resolve_literal` unifiée

```rust
/// Unified literal resolution: FST prefix walk + optional cross-token chains.
pub fn resolve_literal(
    sfx_reader: &SfxFileReader,
    literal: &str,
    anchor_start: bool,       // si=0 only
    include_cross_token: bool, // add cross-token falling walk chains
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
) -> (Vec<FstCandidate>, Vec<CrossTokenChain>)
```

Avantages :
- Un seul point d'entrée pour résoudre un littéral dans le SFX
- `fst_candidates` = `resolve_literal(anchor_start=false, cross_token=false)`
- `suffix_contains_single_token_prefix` = `resolve_literal(anchor_start=true, cross_token=true)`
- Pas de duplication de logique

### Priorité

Phase 1 (maintenant) : retirer le dead code fuzzy — réduction de surface, clarté.
Phase 2 (plus tard) : uniformiser en `resolve_literal` — refactor optionnel, pas bloquant pour v2.
