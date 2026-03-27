# Doc 13 — Design : multi-token avec falling_walk + spans

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Problème (rappel doc 12)

"void Planner planInsertClau" → ne marche pas car le multi-token fait
`resolve_suffix`/`prefix_walk` pour chaque sous-token. "planinsertclau"
n'est pas un token indexé unique → échec → abandon.

## Solution : falling_walk + sibling chain pour chaque sous-token

### Principe

Dans `suffix_contains_multi_token_impl`, step 1 (walk des tokens),
chaque sous-token est résolu via :

1. D'abord le walk normal (resolve_suffix / prefix_walk) — rapide, O(1)
2. Si ça échoue, **falling_walk + sibling chain walk** sur ce sous-token
3. Le résultat est un ensemble de postings + un **span** (nombre de positions
   indexées que ce sous-token consomme)

### Span

Un sous-token normal occupe 1 position indexée :
- "void" → 1 token indexé → span = 1

Un sous-token cross-token occupe N positions :
- "planinsertclau" → "plan" + "insert" + "clau..." = 3 tokens → span = 3

Le span est nécessaire pour l'adjacency check.

### Adjacency avec spans

Actuellement :
```rust
let expected_ti = first_ti + step as u32;
```
Assume span = 1 pour chaque sous-token.

Avec spans :
```rust
// spans[i] = nombre de positions indexées du sous-token i
// cumulative_offset[i] = sum(spans[0..i])
let mut cumulative = vec![0u32; n];
for i in 1..n {
    cumulative[i] = cumulative[i-1] + spans[i-1];
}

// Dans l'adjacency check :
let expected_ti = first_ti + cumulative[step];
```

Exemple : ["void"(span=1), "planner"(span=1), "planinsertclau"(span=3)]
- cumulative = [0, 1, 2]
- void → position first_ti + 0
- planner → position first_ti + 1
- planinsertclau → positions first_ti + 2, first_ti + 3, first_ti + 4

### Postings pour un sous-token cross-token

Le falling_walk + sibling chain walk produit des chaînes d'ordinals.
Pour chaque chaîne validée (byte continuity), on obtient :
- doc_id
- token_index du PREMIER token de la chaîne
- byte_from du premier token
- byte_to du dernier token
- span = nombre de tokens dans la chaîne

Ces postings sont stockés dans `per_token_postings[i]` avec :
- `token_index` = position du premier token (pour l'adjacency avec le précédent)
- L'adjacency avec le suivant utilise `token_index + span` au lieu de `token_index + 1`

### Séparateur validation

Pour les séparateurs entre sous-tokens, on vérifie le gap entre :
- Le DERNIER token du sous-token i (position = first_ti + cumulative[i] + spans[i] - 1)
- Le PREMIER token du sous-token i+1 (position = first_ti + cumulative[i+1])

Le GapMap donne le séparateur exact entre ces deux positions.

### Implémentation step by step

#### 1. Ajouter `spans: Vec<u32>` au pipeline

```rust
let mut per_token_walks: Vec<Vec<(String, Vec<ParentEntry>)>> = Vec::with_capacity(n);
let mut spans: Vec<u32> = Vec::with_capacity(n);
```

#### 2. Pour chaque sous-token : walk normal, fallback cross-token

```rust
for (i, &token) in query_tokens.iter().enumerate() {
    let query_lower = token.to_lowercase();

    // Walk normal (comme avant)
    let walk_results = /* resolve_suffix ou prefix_walk */;

    if !walk_results.is_empty() {
        per_token_walks.push(walk_results);
        spans.push(1);
        continue;
    }

    // Fallback: cross-token via falling_walk + sibling links
    let (ct_walks, ct_span) = cross_token_walk_for_multi(
        sfx_reader, &query_lower, ord_to_term,
    );

    if ct_walks.is_empty() {
        return Vec::new(); // Vraiment aucun match
    }

    per_token_walks.push(ct_walks);
    spans.push(ct_span);
}
```

#### 3. `cross_token_walk_for_multi` — nouvelle fonction

Fait le falling_walk + sibling chain walk et retourne des résultats
au format `Vec<(String, Vec<ParentEntry>)>` compatible avec le multi-token.

Pour chaque chaîne valide, crée un "virtual ParentEntry" dont le raw_ordinal
est le premier ordinal de la chaîne. Les postings résolues ultérieurement
via le raw_ordinal_resolver donneront les postings du premier token.

Le span est retourné pour l'adjacency.

**Problème** : le resolver ne sait pas que c'est un virtual ordinal.
Il va résoudre les postings du premier token seulement, pas de la chaîne.

**Solution** : pré-résoudre les chaînes cross-token et injecter les résultats
directement dans `per_token_postings` (step 4) au lieu de passer par
`per_token_walks` + resolver.

#### 4. Modifier step 4 (resolve) pour les cross-token sub-tokens

```rust
for i in 0..n {
    if i == pivot_idx {
        per_token_postings.push(pivot_postings.clone());
        continue;
    }

    if is_cross_token[i] {
        // Déjà résolu — injecter directement
        per_token_postings.push(pre_resolved_ct_postings[i].clone());
        continue;
    }

    // Normal resolve via raw_ordinal_resolver (comme avant)
    ...
}
```

#### 5. Modifier l'adjacency (step 5) pour utiliser les spans

```rust
let mut cumulative = vec![0u32; n];
for i in 1..n {
    cumulative[i] = cumulative[i-1] + spans[i-1];
}

// Dans le chain building :
let expected_ti = first_ti + cumulative[step];

// Pour le séparateur entre step i et step i+1 :
let ti_a = first_ti + cumulative[i] + spans[i] - 1; // dernière position du sous-token i
let ti_b = first_ti + cumulative[i + 1];             // première position du sous-token i+1
```

### Pivot avec cross-token sub-tokens

Le pivot choisit le sous-token le plus sélectif. Un sous-token cross-token
a déjà ses postings pré-résolues (pas de walk_results avec ParentEntry).
On peut compter directement le nombre de postings pour le pivot selection.

```rust
let pivot_idx = (0..n).min_by_key(|&i| {
    if is_cross_token[i] {
        pre_resolved_ct_postings[i].len()
    } else {
        per_token_walks[i].iter().map(|(_, p)| p.len()).sum::<usize>()
    }
}).unwrap_or(0);
```

### Complexité

- Walk normal : O(1) par sous-token (comme avant)
- Cross-token fallback : O(falling_walk) + O(sibling chain) = O(L + splits)
- Adjacency : O(N) avec spans cumulés
- Total : dominé par le resolve de postings (comme avant)

### Cas couverts

| Query | Sous-tokens | Spans |
|-------|------------|-------|
| "void Planner planInsertClau" | [void, planner, planinsertclau] | [1, 1, 3] |
| "rag3weaver" | [rag3weaver] (single, cross-token) | N/A (single token path) |
| "import getElementById" | [import, getelementbyid] | [1, 4] |
| "class Foo extends Bar" | [class, foo, extends, bar] | [1, 1, 1, 1] |
