# Optimisation startsWith — SI=0 filter dans SuffixContainsQuery

Date : 17 mars 2026

## Problème

startsWith est 1.8x à 3.2x plus lent que contains sur les mêmes termes (release, 5K docs) :

```
contains 'segment'  TA-4sh:  61ms
startsWith 'segment' TA-4sh: 196ms  (3.2x plus lent)

contains 'rag3db'   TA-4sh:  69ms
startsWith 'rag3db'  TA-4sh: 123ms  (1.8x plus lent)
```

Cause : startsWith passe par `RegexContinuationQuery` (prefix DFA + GapMap cross-token), qui est conçu pour le fuzzy/regex cross-token. Pour un simple prefix match, c'est overkill.

## Solution : SI=0 filter dans SuffixContainsQuery

Le suffix FST stocke chaque suffixe d'un token avec un `si` (suffix index) :
- `si = 0` : le suffixe est le token complet (match depuis le début)
- `si > 0` : le suffixe est un substring (match milieu/fin)

**Prefix match = contains + filtre SI=0.**

Un token qui "commence par X" c'est exactement : un suffixe X dans le suffix FST avec SI=0.

### Changement dans SuffixContainsQuery

```rust
pub struct SuffixContainsQuery {
    field: Field,
    pattern: String,
    fuzzy_distance: u8,
    prefix_only: bool,  // NEW — si true, filtre SI=0
    highlight_sink: Option<Arc<HighlightSink>>,
    // ...
}

impl SuffixContainsQuery {
    pub fn with_prefix_only(mut self) -> Self {
        self.prefix_only = true;
        self
    }
}
```

Dans le scorer, lors du walk du suffix FST :

```rust
for (suffix_term, parent_entries) in &walk_results {
    for parent in parent_entries {
        if self.prefix_only && parent.si > 0 {
            continue;  // skip substring matches
        }
        // ... resolve postings, score, collect
    }
}
```

### Changement dans build_starts_with_query (lucivy_core/query.rs)

```rust
// Avant
fn build_starts_with_query(...) {
    let query = RegexContinuationQuery::new(field, value, ContinuationMode::StartsWith)
        .with_prefix()
        .with_fuzzy_distance(distance);
    // ...
}

// Après
fn build_starts_with_query(...) {
    let query = SuffixContainsQuery::new(field, value.to_lowercase())
        .with_prefix_only()
        .with_fuzzy_distance(distance);
    // ...
}
```

### Multi-token startsWith

Pour "programming lang" en startsWith :
- Le SuffixContainsQuery split par whitespace (comme contains_split)
- Premier token "programming" : contains avec SI=0 → exact token match
- Dernier token "lang" : contains avec SI=0 → prefix match "lang*"

Ou plus simple : on utilise `startsWith_split` qui split et crée N sub-queries startsWith. Chaque sub-query est un `SuffixContainsQuery` avec `prefix_only`.

### Ce qui ne change PAS

- `contains` → SuffixContainsQuery sans prefix_only (inchangé)
- `contains` regex → RegexContinuationQuery (inchangé)
- `regex` → RegexContinuationQuery (inchangé)
- `fuzzy` → RegexContinuationQuery (inchangé)
- Le suffix FST walk lui-même (inchangé, on filtre juste les résultats)

### Gain attendu

Sur "segment" (très fréquent) :
- Avant : DFA construction + walk toutes les suffixes + GapMap = 196ms
- Après : suffix FST walk + filtre SI=0 → ~50 tokens au lieu de centaines = <20ms estimé

Le gain sera proportionnel au nombre de suffixes filtrés. Plus le terme est fréquent, plus le gain est grand.

## Fichiers à modifier

1. `src/query/phrase_query/suffix_contains_query.rs` — ajouter `prefix_only` field + filtre dans le scorer
2. `lucivy_core/src/query.rs` — `build_starts_with_query` → `SuffixContainsQuery::new().with_prefix_only()`

## Estimation

~15 lignes modifiées. Re-run bench pour valider le gain.
