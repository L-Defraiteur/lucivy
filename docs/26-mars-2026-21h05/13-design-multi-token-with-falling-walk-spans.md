# Doc 13 — Design : multi-token via falling_walk + sibling links + spans

Date : 27 mars 2026 (v2)
Branche : `feature/cross-token-search`

## Problème (rappel doc 12)

"void Planner planInsertClau" → ne marche pas car le multi-token fait
`resolve_suffix`/`prefix_walk` pour chaque sous-token. "planinsertclau"
n'est pas un token indexé unique → échec → abandon.

## Insight clé

Le falling_walk + sibling chain walk est un **superset** du walk normal :
- Si "planinsert" est un token indexé unique → falling_walk le trouve comme
  un candidat terminal (prefix_len == remainder.len(), span=1)
- Si "planinsert" = "plan" + "insert" → sibling links trouvent la chaîne (span=2)

Pas besoin de deux paths (walk normal + fallback). Un seul algo via
falling_walk couvre tout. Pas de fallback, pas d'union, pas de résultats perdus.

## Design

### Step 1 : falling_walk + sibling chain pour chaque sous-token

Pour chaque sous-token du multi-token, on appelle `cross_token_search_for_multi()`.
Cette fonction :
1. Fait le falling_walk sur le sous-token
2. Suit les sibling links pour les chaînes
3. Résout les postings des chaînes valides
4. Retourne des `MultiTokenPosting` avec span

```rust
struct MultiTokenPosting {
    doc_id: u32,
    token_index: u32,  // position du PREMIER token de la chaîne
    span: u32,         // nombre de positions occupées (1 = single token, N = chaîne)
    byte_from: u32,
    byte_to: u32,
}
```

### Step 2 : pivot selection

Le pivot est le sous-token avec le moins de postings (comme avant).
Les postings cross-token sont déjà résolues → on compte directement.

### Step 3 : adjacency avec spans

Chaque posting a son propre span. L'adjacency vérifie :

```
posting[i].token_index + posting[i].span == posting[i+1].token_index
```

Au lieu de l'ancien :
```
posting[i].token_index + 1 == posting[i+1].token_index
```

C'est la seule modification dans l'adjacency check. Le span=1 pour les
tokens normaux donne exactement le même comportement qu'avant.

### Step 4 : séparateur validation

Le séparateur entre sous-token i et i+1 se vérifie entre :
- La DERNIÈRE position du sous-token i : `posting[i].token_index + posting[i].span - 1`
- La PREMIÈRE position du sous-token i+1 : `posting[i+1].token_index`

Le GapMap donne le séparateur exact entre ces deux positions.

## Exemple

Query : "void Planner planInsertClau"
Sous-tokens : ["void", "planner", "planinsertclau"]

### Step 1 résultats

| Sous-token | Résultat | Span |
|------------|----------|------|
| "void" | falling_walk → "void" direct, terminal | 1 |
| "planner" | falling_walk → "planner" direct, terminal | 1 |
| "planinsertclau" | falling_walk → "plan" (split), sibling → "insert", sibling → "clau..." (terminal) | 3 |

### Step 3 adjacency

```
doc X: void(pos=5), planner(pos=6), plan(pos=7), insert(pos=8), clause(pos=9)

void posting:     token_index=5, span=1
planner posting:  token_index=6, span=1
planinsertclau:   token_index=7, span=3

Check: 5 + 1 == 6 ✓ (void → planner)
Check: 6 + 1 == 7 ✓ (planner → planinsertclau)
→ MATCH!
```

## Fonction `cross_token_resolve_for_multi`

Nouvelle fonction qui remplace le walk normal dans le multi-token pipeline.
Retourne `Vec<MultiTokenPosting>` pour un sous-token donné.

```rust
fn cross_token_resolve_for_multi(
    sfx_reader: &SfxFileReader,
    sub_token: &str,
    raw_term_resolver: &F,
    ord_to_term: Option<&dyn Fn(u64) -> Option<String>>,
    is_first: bool,   // any SI pour le premier, SI=0 pour les autres
    is_last: bool,    // prefix match pour le dernier, exact pour les autres
) -> Vec<MultiTokenPosting>
```

Pour chaque candidat du falling_walk :
1. Si le remainder est vide ou matche un token SI=0 en prefix → span=1, terminal
2. Sinon, sibling chain walk → span=N, chaîne complète
3. Résoudre les postings de la chaîne, vérifier byte continuity

## Impact sur le code existant

| Fichier | Changement |
|---------|-----------|
| `suffix_contains.rs` | Ajouter `MultiTokenPosting`, `cross_token_resolve_for_multi()` |
| `suffix_contains.rs` | Modifier `suffix_contains_multi_token_impl` step 1 : utiliser falling_walk |
| `suffix_contains.rs` | Modifier adjacency check : `token_index + span` au lieu de `+ 1` |
| `suffix_contains.rs` | Modifier séparateur check : positions ajustées par span |
| `suffix_contains_query.rs` | Passer `ord_to_term` au multi-token path |

## Ce qui ne change PAS

- Le pivot selection (même logique, comptage de postings)
- Le GapMap (toujours utilisé pour strict separators)
- Le single-token path (inchangé, pas de multi-token)
- Les sibling links (inchangés, déjà en place)
