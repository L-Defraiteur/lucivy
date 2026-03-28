# 10 — Plan : regex via réutilisation du contains exact

## Constat

Le regex actuel a encore des cas lents :
- `incremental.sync` (1217ms) — single-literal, pas de PosMap, Phase 3c gap>0
- `.*getElementById` (bloqué) — littéral "getelementbyid" cross-token, `prefix_walk` vide → fallback scan FST
- `.*weaver` (481ms) — fallback sur certains segments

## Problème fondamental

On réinvente la roue. Le contains exact (`suffix_contains_single_token_with_terms`) sait DÉJÀ :
- Trouver un token dans le SFX
- Gérer le cross-token via falling_walk + sibling chain
- Résoudre les postings en resolve-last
- Retourner (doc_id, position, byte_from, byte_to)

Le regex ne réutilise PAS ce code. Il fait `prefix_walk` (qui ne gère pas le cross-token) puis fallback vers un scan FST complet quand ça marche pas.

## Architecture cible

```
regex_contains_via_literal(pattern):

  1. extract_all_literals(pattern) → ["incremental", "sync"]

  2. Pour chaque littéral :
     résultats = contains_exact(literal)    // RÉUTILISE le code contains
       → suffix_contains_single_token_with_terms(literal)
       → inclut cross-token via falling_walk + sibling chain
       → retourne Vec<(doc_id, position, byte_from, byte_to)>

  3. Intersection par doc_id
     → has_doc() pour O(log n) si besoin

  4. Position ordering (byte_from/byte_to séquentiels)
     → filtre les docs où les littéraux ne sont pas dans l'ordre

  5. PosMap walk entre les positions des littéraux
     → lire ordinals, feeder gap+text au DFA
     → O(distance × token_len) par doc

  6. Émettre les matches validés

  JAMAIS de fallback vers scan FST.
  Si aucun littéral viable → retourner 0 résultats.
```

## Changements nécessaires

### 1. Fonction `resolve_literal_as_contains`

Wrapper autour de `suffix_contains_single_token_with_terms` qui retourne les postings :

```rust
fn resolve_literal_as_contains(
    sfx_reader: &SfxFileReader,
    literal: &str,
    resolver: &dyn PostingResolver,
    ord_to_term: &dyn Fn(u64) -> Option<String>,
) -> Vec<(DocId, u32, u32, u32)>  // (doc_id, position, byte_from, byte_to)
```

Utilise le MÊME code que le contains exact :
- `suffix_contains_single_token_inner` pour single-token
- `cross_token_search_with_terms` pour cross-token (sibling chain)
- Retourne les matches avec positions

### 2. Remplacer prefix_walk dans regex_contains_via_literal

Au lieu de :
```rust
let walk_results = sfx_reader.prefix_walk(&literal);
// ... DFA validation sur walk_results ...
```

Faire :
```rust
let literal_matches = resolve_literal_as_contains(sfx_reader, &literal, resolver, ord_to_term);
// literal_matches contient déjà (doc_id, position, byte_from, byte_to)
// Pas besoin de DFA validation — le contains exact a déjà validé
```

### 3. Supprimer le fallback continuation_score_sibling

Plus jamais de scan FST. Si `resolve_literal_as_contains` retourne vide → 0 résultats pour ce segment. C'est correct : si le littéral n'existe pas, le regex ne peut pas matcher.

### 4. Simplifier Phase 1-3

Phase 1 (DFA validation ordinal) → plus nécessaire pour les littéraux (contains exact a validé)
Phase 2 (gap=0 sibling chain) → déjà fait par cross_token_search_with_terms
Phase 3a (resolve accepted) → déjà fait par contains exact
Phase 3b (gap=0 chains) → déjà fait
Phase 3c (gap>0) → remplacé par PosMap walk

Ce qui reste :
- Intersection des résultats de chaque littéral
- Position ordering
- PosMap walk pour valider le DFA entre les littéraux
- Émission des résultats

### 5. Cas single-literal

Pour un regex avec un seul littéral (ex: `shard[a-z]+`), pas d'intersection.
Le contains exact trouve les tokens contenant "shard".
Le DFA valide le reste du token (les `[a-z]+` bytes).

Ici il faut quand même le DFA :
- contains_exact("shard") → tokens "shard", "sharded", "sharding", etc. avec positions
- Pour chaque match : feeder le texte du token au DFA → accepte ou pas
- Pour "sharded" : DFA `shard[a-z]+` → accepte → match
- Pour "shard" seul (3 chars) : DFA veut `[a-z]+` → pas assez → cross-token via PosMap

### 6. Gestion du `.*` entre littéraux

Quand le regex entre deux littéraux est `.*` :
- PosMap walk n'est même pas nécessaire — le `.*` accepte tout
- Position ordering suffit (le littéral B doit être après A)
- On pourrait détecter ça et skip le PosMap walk

Détection : le DFA après le premier littéral est dans un état `.*` (accepte tout, can_match toujours true). On peut tester : `automaton.is_match(&state)` après avoir feedé chaque littéral → si le DFA est déjà accepting et peut continuer, c'est un `.*`.

## Estimation d'impact

| Pattern | Avant | Après (estimé) |
|---|---|---|
| `shard[a-z]+` | 45ms | ~45ms (inchangé) |
| `rag3.*ver` | 192ms | ~50ms (intersection directe) |
| `incremental.sync` | 1217ms | ~30ms (contains exact × 2 + PosMap) |
| `.*weaver` | 481ms | ~20ms (contains exact "weaver") |
| `.*getElementById` | bloqué | ~50ms (cross-token contains "getelementbyid") |
| `flow.control` | 24ms | ~20ms (inchangé) |
| `[a-z]+ment` | lent (fallback) | 0 results immédiat (pas de littéral) |

## Risques

- Le contains exact utilise `raw_term_resolver` (closure) pas `PostingResolver` (trait). Il faut adapter l'interface ou wrapper.
- Le contains exact retourne `SuffixContainsMatch` pas `PostingEntry`. Conversion nécessaire.
- Pour le DFA single-literal, on doit quand même feeder le token text au DFA pour valider les chars après le littéral. Le contains exact ne fait pas ça — il vérifie juste la substring.

## Ordre d'implémentation

1. Écrire `resolve_literal_as_contains` — wrapper contains exact → postings
2. Remplacer `prefix_walk` dans le flow multi-literal (étapes 0-0b)
3. Remplacer le fallback `continuation_score_sibling` par 0 résultats
4. Tester `.*getElementById`, `incremental.sync`, `.*weaver`
5. Optimiser single-literal avec DFA validation sur le token text
6. Détecter `.*` entre littéraux pour skip PosMap walk
