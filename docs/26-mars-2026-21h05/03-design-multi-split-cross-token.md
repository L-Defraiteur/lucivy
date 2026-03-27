# Doc 03 — Design : multi-split cross_token_search (exact)

Date : 26 mars 2026
Branche : `feature/cross-token-search`

## Contexte

Le `cross_token_search` actuel gère 1 split (2 tokens adjacents).
Pour les queries qui traversent 3+ tokens (e.g. "rag3dbfromcore"), on a besoin de multi-split.

Le fuzzy cross-token est géré par `RegexContinuationQuery` (DFA continu via gap bytes).
Ce design concerne uniquement le **cross-token exact**, qui est plus rapide (O(L) falling walk
vs DFA search sur le FST entier).

## Algorithme : worklist + pipeline de HashMaps

### Principe

Au lieu de recurser, on utilise un **worklist** de splits en attente.
Chaque entrée du worklist représente une chaîne partielle de tokens.

```
Worklist entry:
  - chain: Vec<SplitCandidate>   // splits trouvés jusqu'ici
  - remainder: &str              // portion de la query pas encore matchée
  - query_offset: usize          // position dans la query originale
```

### Pseudo-code

```rust
fn cross_token_search_multi(query, resolver, max_depth=4):
    let mut worklist = vec![];
    let mut results = vec![];

    // Seed : falling_walk sur la query complète
    for cand in falling_walk(query):
        let remainder = query[cand.prefix_len..]
        worklist.push(Chain { splits: vec![cand], remainder, depth: 1 })

    while let Some(chain) = worklist.pop():
        if chain.remainder.is_empty():
            continue  // split exact, pas de remainder

        // Essayer de résoudre le remainder comme début de token
        let right_walks = prefix_walk_si0(chain.remainder)
        if !right_walks.is_empty():
            // Résultat complet : chaîne de splits + remainder terminal
            results.push(CompleteChain { splits: chain.splits, right: right_walks })
            continue

        // Remainder ne matche pas en single-token → chercher un nouveau split
        if chain.depth < max_depth:
            for sub_cand in falling_walk(chain.remainder):
                let sub_remainder = chain.remainder[sub_cand.prefix_len..]
                let mut new_splits = chain.splits.clone()
                new_splits.push(sub_cand)
                worklist.push(Chain {
                    splits: new_splits,
                    remainder: sub_remainder,
                    depth: chain.depth + 1,
                })
```

### Adjacency check : pipeline de HashMaps

Pour une chaîne de N splits + 1 remainder terminal, il faut vérifier
que les tokens sont **consécutifs** dans le même document.

```
Chaîne : [split_0, split_1, ..., split_{N-1}] + right_terminal

Tokens impliqués :
  T0 (left du split_0)
  T1 (left du split_1) = right du split_0
  ...
  T_N (right terminal)

Condition : pour chaque doc_id, T_i.position + 1 == T_{i+1}.position
```

**Pipeline :**

```
1. Résoudre le côté le plus sélectif de la chaîne (pivot)
   → pivot_doc_ids

2. Pour chaque paire adjacente (Ti, Ti+1) dans la chaîne :
   - Construire right_index: HashMap<(doc_id, position), ...>
   - Pour chaque posting de Ti, lookup (doc_id, position+1) dans right_index
   - Les doc_ids survivants passent au niveau suivant

3. Propager les contraintes : chaque étape filtre les doc_ids
   par adjacency avec l'étape précédente.
```

Variante optimisée : **forward chain**

```rust
// Start from leftmost split, propagate forward
let mut active: HashMap<(DocId, u32), Vec<MatchState>> = HashMap::new();

// Seed with split_0 left postings
for posting in resolve(split_0.parent):
    active.insert((posting.doc_id, posting.position), vec![MatchState { byte_from: ... }]);

// For each split boundary i = 0..N-1:
let mut next_active = HashMap::new();
for ((doc, pos), states) in &active:
    // Right side of split_i = left side of split_{i+1}
    let right_postings = resolve(split_i.right_parent);  // ou remainder walk
    for rp in right_postings:
        if rp.doc_id == doc && rp.position == pos + 1:
            next_active.insert((doc, rp.position), updated_states);
active = next_active;

// Final: match remainder terminal against active set
```

### Caches

Les opérations coûteuses sont cachées par remainder string :

| Opération | Clé cache | Coût sans cache |
|-----------|-----------|-----------------|
| `falling_walk(remainder)` | remainder string | O(L) FST walk |
| `prefix_walk_si0(remainder)` | remainder string | O(L) FST walk |
| `resolve(ordinal)` | raw_ordinal | disk/mmap read |

Le cache `ordinal → postings` est déjà en place.
Ajouter un cache `remainder → falling_walk_result` et `remainder → prefix_walk_result`.

### Limites

- **max_depth = 4** : 5 tokens max. Couvre tous les cas réalistes
  (CamelCaseSplit min=4 → un identifiant de 20 chars = max 5 chunks).
- **Pas de fuzzy** : le multi-split exact est rapide. Le fuzzy multi-token
  est géré par RegexContinuationQuery (DFA continu).
- **Complexité worst-case** : O(D × C × L) où D=depth, C=candidates par level,
  L=query length. En pratique C est petit (2-5 candidates) car le falling_walk
  est très sélectif sur les token boundaries.

### Exemple concret

Query : "rag3dbfromcore" (14 bytes)
Tokens indexés : "rag3" (4), "db" (2), "from" (4), "core" (4)

```
falling_walk("rag3dbfromcore"):
  → "rag3" si=0+4==4 ✓ → remainder "dbfromcore" (prefix_len=4)
  → (pas d'autres splits utiles)

prefix_walk_si0("dbfromcore") → vide

falling_walk("dbfromcore"):
  → "db" si=0+2==2 ✓ → remainder "fromcore" (prefix_len=2)

prefix_walk_si0("fromcore") → vide

falling_walk("fromcore"):
  → "from" si=0+4==4 ✓ → remainder "core" (prefix_len=4)

prefix_walk_si0("core") → trouvé! ✓

Chaîne complète : "rag3" | "db" | "from" | "core" (4 splits, 4 tokens)
Adjacency : pos(rag3)+1 == pos(db), pos(db)+1 == pos(from), pos(from)+1 == pos(core)
```

### Impact sur l'API

`cross_token_search` garde la même signature.
Le changement est interne : worklist au lieu d'un single falling_walk.

```rust
pub fn cross_token_search<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: &F,
    _fuzzy_distance: u8,  // ignoré, exact seulement
) -> Vec<SuffixContainsMatch>
```
