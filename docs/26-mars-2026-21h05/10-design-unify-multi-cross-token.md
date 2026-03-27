# Doc 10 — Unification multi-token et cross-token search

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Problème constaté

"planInsertClau" → marche (cross-token via sibling links)
"void Planner planInsertClau" → marche PAS

Le multi-token search tokenize par espaces : ["void", "planner", "planinsertclau"].
Chaque token est cherché via `suffix_contains_single_token`. Pour "planinsertclau",
ça appelle le single-token search qui ne trouve rien en single-token, puis le
cross-token fallback. Mais le multi-token path (`suffix_contains_multi_token`)
ne passe PAS par le cross-token fallback — il traite chaque token comme un
single-token exact.

## Observation clé

Avec les sibling links, la distinction entre "multi-token" et "cross-token"
est artificielle. Les deux sont des chaînes de tokens adjacents — la seule
différence c'est le gap_len :

- **Cross-token** : gap_len == 0 (tokens contigus, CamelCaseSplit)
- **Multi-token** : gap_len > 0 (tokens séparés par espace/ponctuation)

Les sibling links stockent les DEUX cas. Un seul algorithme pourrait tout gérer.

## Idée de Lucie : multi-token = cross-token avec gap autorisé

Au lieu de deux paths séparés, un seul algorithme :

```
1. falling_walk(query) → premier token (any SI)
2. Suivre les sibling links :
   - gap_len == 0 → cross-token (pas de séparateur dans la query)
   - gap_len > 0 → vérifier que la query a un séparateur au même endroit
3. Continuer la chaîne
```

Le paramètre `strict_separators` détermine si on vérifie le contenu exact
du séparateur ou juste sa présence.

### Exemple

Query : "void Planner planInsertClau"

```
falling_walk("void planner planinsertclau")
→ "void" (SI=0, token_len=4, prefix_len=4)

sibling_table[void] → [("planner", gap=1)]  // espace entre void et Planner
query[4] == ' ' et gap_len == 1 → match!
remainder = "planner planinsertclau"

sibling_table[planner] → [("plan", gap=1)]  // espace entre Planner et plan
query[4+8] == ' ' et gap_len == 1 → match!
remainder = "planinsertclau"

// Maintenant on est dans la partie cross-token (gap=0) :
sibling_table[plan] → [("insert", gap=0)]
"insertclau".starts_with("insert") → match!
remainder = "clau"

sibling_table[insert] → [("clau...", gap=0)]
"clau...".starts_with("clau") → terminal match!
```

### Avantages

- **Un seul algorithme** au lieu de deux paths (single_token, multi_token, cross_token)
- **Les sibling links gèrent tout** : cross-token ET multi-token
- **Pas de tokenization de la query** — on n'a plus besoin de `tokenize_query()`
  pour splitter par espaces
- **Le séparateur est vérifié structurellement** via gap_len, pas via GapMap

### Changement nécessaire dans la chaîne sibling

Actuellement `contiguous_siblings()` filtre à gap_len == 0. Il faudrait
une méthode plus flexible :

```rust
/// Get siblings where the gap matches a portion of the query.
fn matching_siblings(&self, ordinal: u32, query_remainder: &[u8]) -> Vec<(u32, usize)> {
    // Pour chaque sibling:
    //   gap_len == 0 → match si le texte du token suivant commence le remainder
    //   gap_len > 0 → match si remainder[0..gap_len] correspond au séparateur
    //                  ET le texte du token suivant commence remainder[gap_len..]
}
```

## Solution alternative : multi-token appelle cross-token

Plus simple à implémenter dans un premier temps :

Dans `suffix_contains_multi_token`, pour chaque token de la query,
au lieu d'appeler `suffix_contains_single_token_inner` (exact match),
appeler `suffix_contains_single_token_with_terms` (qui fait le cross-token
fallback via sibling links).

Ça permettrait à "planInsertClau" d'être résolu en cross-token dans le
contexte multi-token, sans changer l'architecture.

## Recommandation

**Court terme** : solution alternative — multi-token appelle cross-token
pour chaque sous-token. Simple, pas de refactoring.

**Moyen terme** : unification complète. Un seul algorithme basé sur les
sibling links qui gère les deux cas (gap=0 et gap>0).

**Long terme** : supprimer `tokenize_query()`, `suffix_contains_multi_token()`,
et le concept de "multi-token" vs "cross-token". Tout est juste "sibling chain
search with gap awareness".
