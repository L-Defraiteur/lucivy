# Doc 12 — Blocker : interaction multi-token × cross-token

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Problème

"planInsertClau" → fonctionne (cross-token via sibling links) ✅
"void Planner planInsertClau" → ne fonctionne PAS ❌

## Cause

Le multi-token search (`suffix_contains_multi_token_impl`) tokenize la query
par espaces : `["void", "planner", "planinsertclau"]`.

Pour chaque sous-token, il fait un lookup dans le SFX :
- "void" → `resolve_suffix` → trouvé ✓
- "planner" → `resolve_suffix` → trouvé ✓
- "planinsertclau" (dernier) → `prefix_walk_si0` → **pas trouvé** car aucun
  token indexé ne commence par "planinsertclau" (c'est "plan"+"insert"+"clau...")
- → `walk_results.is_empty()` → `return Vec::new()` → **abandonne tout**

Le multi-token ne fait PAS de cross-token fallback pour les sous-tokens individuels.
Il traite chaque sous-token comme un token unique indexé.

## Pourquoi le fallback naïf ne marche pas

Tentative rejetée : si multi-token retourne vide, fallback sur cross_token_search
avec la query complète. Problème : ça ignore les résultats multi-token qui
marchaient partiellement et cherche la query entière comme un seul cross-token,
ce qui est sémantiquement différent.

"void Planner planInsertClau" en cross-token chercherait la substring
"void planner planinsertclau" comme un bloc contigu — ça ne trouvera rien non plus
car il y a des espaces (gap > 0) entre "void" et "planner".

## Le vrai problème

Le multi-token traite chaque sous-token comme un **token indexé unique**.
Mais "planinsertclau" n'est PAS un token unique — c'est une chaîne de tokens
(plan + insert + clau...) qui devrait être résolue via sibling links.

Le fix doit être **dans le multi-token lui-même**, pas en fallback :
quand un sous-token ne matche pas en single-token, essayer le cross-token
via sibling links pour ce sous-token spécifique.

## Solution proposée

Dans `suffix_contains_multi_token_impl`, step 1 (walk des tokens), quand
`walk_results.is_empty()` pour un sous-token :

1. Essayer `falling_walk(token)` → obtenir les split candidates
2. Suivre les sibling links pour trouver la chaîne qui couvre le sous-token
3. Convertir le résultat en `Vec<(String, Vec<ParentEntry>)>` compatible
   avec le format attendu par le multi-token pipeline

### Difficulté

Le multi-token attend des **ParentEntry** (un ordinal par sous-token).
Le cross-token via sibling links produit une **chaîne d'ordinals** (plusieurs
tokens par sous-token). Ces deux représentations sont incompatibles.

### Options

#### A. Résoudre le cross-token et injecter les résultats résolus

Résoudre complètement le cross-token pour le sous-token problématique
(via `cross_token_search_with_terms`), obtenir les `SuffixContainsMatch`,
puis les injecter comme des résultats "virtuels" dans le multi-token pipeline.

Problème : le multi-token fait du pivot + adjacency sur les ParentEntry/ordinals.
Des résultats déjà résolus ne rentrent pas dans ce pipeline.

#### B. Éclater la chaîne sibling en sous-tokens individuels

Si "planinsertclau" = chain [plan, insert, clau...], le multi-token pourrait
réécrire la query en ["void", "planner", "plan", "insert", "clau..."] et
relancer le multi-token avec plus de sous-tokens.

Problème : les séparateurs entre les nouveaux sous-tokens sont gap=0 (pas d'espace)
alors que le multi-token attend des séparateurs explicites.

#### C. Traiter le cross-token comme un méta-ordinal

Créer un "virtual ordinal" qui représente la chaîne entière. Le multi-token
l'utilise comme n'importe quel ordinal dans le pivot + adjacency.
Quand il faut résoudre les postings du virtual ordinal, on fait le chain walk.

Élégant mais complexe à implémenter.

#### D. Deux passes

1. Multi-token classique pour les sous-tokens qui matchent en single-token
2. Pour les sous-tokens qui échouent, résoudre via cross-token séparément
3. Intersecter les résultats : garder les docs qui matchent TOUTES les parties

Simple, correct, compatible avec le pivot existant. Le cross-token est résolu
indépendamment et les doc_ids sont croisés à la fin.

## Recommandation

**Option D (deux passes)** est la plus simple et correcte :

```rust
// Pseudo-code dans suffix_contains_multi_token_impl :

// Pass 1: résoudre chaque sous-token normalement
for token in query_tokens:
    walks = normal_walk(token)
    if walks.is_empty():
        // Ce sous-token nécessite du cross-token
        cross_token_indices.push(i)
    else:
        per_token_walks.push(walks)

// Si tous les tokens normaux matchent : pipeline normal (pivot + adjacency)
// Pour les tokens cross-token : résoudre séparément via sibling links

// Pass 2: cross-token pour les tokens manquants
for i in cross_token_indices:
    ct_results = cross_token_search_with_terms(sfx_reader, token[i], ...)
    ct_doc_ids = ct_results.map(|m| m.doc_id).collect::<HashSet>()
    // Filtrer les résultats du multi-token par ces doc_ids

// Intersecter les doc_ids
```

## Cas non couvert

Un sous-token **intermédiaire** (pas le dernier) qui est cross-token.
Par exemple : "void planInsertClause from" → "planInsertClause" est au milieu.
Le multi-token attend un `resolve_suffix` exact pour les intermédiaires.

Avec l'option D, on peut gérer ça aussi : résoudre le sous-token intermédiaire
via cross-token et croiser les doc_ids.
