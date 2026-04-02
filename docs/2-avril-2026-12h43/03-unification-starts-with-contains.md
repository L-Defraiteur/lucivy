# 03 — Unification startsWith dans contains

Date : 2 avril 2026

## Situation actuelle

`startsWith` est un mode séparé dans `SuffixContainsQuery` contrôlé par
`prefix_only: bool`. En pratique, la seule différence avec `contains` est :

| | contains | startsWith |
|--|---------|------------|
| Premier token | si quelconque | **si=0** (doit commencer au début d'un token indexé) |
| Dernier token | prefix match OK | prefix match OK |
| Tokens du milieu | si=0, exact-reach | si=0, exact-reach |

C'est un seul flag : `anchor_start: bool` (ou `starts_with: bool`).

## Plan

### 1. Remplacer `prefix_only` par `anchor_start`

Dans `SuffixContainsQuery` :
- `prefix_only: bool` → `anchor_start: bool`
- Quand `anchor_start=true` : le premier token est filtré à si=0
  (il doit commencer au début d'un token indexé)
- Quand `anchor_start=false` : le premier token accepte tout si (contains normal)

### 2. Impact sur le pipeline

`resolve_token_for_multi` a déjà le param `is_first`. La logique :

```rust
let is_first = i == 0 && !anchor_start;
// anchor_start=true → is_first=false → require_si0=true sur le premier token
// anchor_start=false → is_first=true → require_si0=false (contains normal)
```

C'est exactement ce qu'on fait déjà ! Le code actuel dans
`suffix_contains_multi_token_impl` :

```rust
let is_first = i == 0 && !prefix_only;
```

Il suffit de renommer `prefix_only` → `anchor_start`.

### 3. Impact sur le fuzzy (d>0)

Pour `startsWith fuzzy`, le trigram pigeonhole dans `fuzzy_contains_via_trigram`
devrait aussi contraindre le premier trigram à si=0. Actuellement le fuzzy
ne distingue pas contains vs startsWith.

Le fix : passer `anchor_start` à `fuzzy_contains_via_trigram`, et filtrer
les candidats FST du premier trigram à si=0.

### 4. Suppression de `ContinuationMode::StartsWith`

Dans `RegexContinuationQuery`, le mode `StartsWith` est utilisé pour
la query startsWith. Après unification, c'est juste `Contains` avec
`anchor_start=true`.

### 5. Impact sur `build_contains_query`

```rust
// Avant
fn build_contains_query(...) → SuffixContainsQuery { prefix_only: false }
fn build_starts_with_query(...) → SuffixContainsQuery { prefix_only: true }

// Après
fn build_contains_query(...) → SuffixContainsQuery { anchor_start: false }
fn build_starts_with_query(...) → SuffixContainsQuery { anchor_start: true }
```

Même résultat, juste un renaming. `build_starts_with_query` peut rester
comme raccourci API.

### Étapes

1. Renommer `prefix_only` → `anchor_start` dans SuffixContainsQuery
2. Renommer dans suffix_contains.rs (multi_token_impl, single_token)
3. Passer `anchor_start` à `fuzzy_contains_via_trigram`
4. Filtrer premier trigram candidates à si=0 quand anchor_start
5. Optionnel : supprimer `ContinuationMode::StartsWith` si plus utilisé
6. Tests : vérifier que startsWith et contains donnent les mêmes résultats
   quand le match commence au début d'un token
