# Design — Compat layer v2

## Principe

Les utilisateurs v1 utilisent des query types (`term`, `fuzzy`, `regex`, etc.)
qui ont un comportement "par token" hérité de tantivy. En v2, toutes les
requêtes texte passent par le SFX (cross-token aware). Le compat layer route
les anciens types vers le bon comportement v2 de manière transparente.

## Décisions prises

### term → contains + anchor_start + exact_match

**Avant** : exact token lookup dans le term dict. "rag3weaver" ne matche pas
si le tokenizer a splitté en "rag3" + "weaver".

**Après** : contains avec `anchor_start=true` + `exact_match=true`.
- `anchor_start` : le match commence au début d'un token (SI=0)
- `exact_match` : le match couvre le(s) token(s) entier(s), pas un préfixe
- Cross-token aware : "rag3weaver" matche même si splitté en "rag3" + "weaver"

Nouveau paramètre `exact_match: Option<bool>` dans QueryConfig.

### fuzzy → contains avec distance

**Avant** : Levenshtein sur le term dict (par token individuel). Rapide mais
ne trouve pas les sous-chaînes.

**Après** : route vers `contains` avec `distance` du config. Utilise
`RegexContinuationQuery` (trigram pigeonhole) — cross-token, correct ordering.

Pas de tentative de reproduire le term-dict DFA walk — c'est du scotch et
les temps sont imprévisibles sur gros indexes.

### regex → contains + regex=true

**Avant** : regex sur le term dict (par token individuel, tantivy standard).

**Après** : déjà géré. `contains` avec `regex: true` utilise
`RegexContinuationQuery` qui fait du cross-token regex via SFX.
Plus puissant que l'ancien (cross-token), plus rapide sur nos benchmarks.

Route `regex` → `contains` avec `regex: true` + le pattern du config.

### startsWith → contains + anchor_start ✅ FAIT

Déjà implémenté. `startsWith` route vers `contains` avec `anchor_start=true`.
Cross-token aware grâce au `cross_token_search_with_terms` filtré SI=0.

### phrase → inchangé

PhraseQuery fonctionne bien. Pas de changement.

### parse → inchangé

QueryParser fonctionne bien. Pas de changement.

### phrase_prefix → inchangé

Autocomplétion, fonctionne bien.

### contains / contains_split → inchangé

C'est déjà le comportement v2 natif.

### disjunction_max / boolean / more_like_this → inchangés

Pas de changement.

## Paramètres à ajouter à QueryConfig

```rust
pub struct QueryConfig {
    // ... existants ...
    pub anchor_start: Option<bool>,   // ✅ FAIT — SI=0 constraint
    pub exact_match: Option<bool>,    // NOUVEAU — match couvre le token entier
}
```

## Routing compat layer (dans build_query)

```
"term"    → build_contains_query avec anchor_start=true, exact_match=true, distance=0
"fuzzy"   → build_contains_query avec distance=config.distance.unwrap_or(1)
"regex"   → build_contains_query avec regex=true
```

Les anciens types restent reconnus (pas d'erreur "unknown query type").
Warning optionnel "deprecated, use contains with ..." ? À décider.

## Questions ouvertes

### 1. Warning ou silencieux ?

Option A : routing silencieux (aucun log, l'ancien code marche tel quel).
Option B : routing + eprintln warning "term is deprecated, use contains
with anchor_start=true, exact_match=true".
Option C : routing + champ `deprecated_warning` dans la réponse JSON.

### 2. exact_match pour le fuzzy ?

Quand `term` route vers contains avec `exact_match=true`, le match doit
couvrir le token entier. Est-ce qu'on veut la même chose pour `fuzzy` ?
Probablement non — fuzzy par nature trouve des approximations, forcer
exact_match serait contradictoire.

### 3. Performance term vs contains

L'ancien `term` était O(log N) sur le term dict. Le nouveau via contains
fait un SFX walk qui est plus lent (~1-5ms vs ~0.1ms). Pour un compat layer
c'est acceptable — les utilisateurs qui veulent la perf native peuvent
utiliser `contains` directement avec les bons paramètres.

Possibilité future : si `exact_match=true` et `anchor_start=true` et un
seul token (pas de cross-token), on pourrait fast-path vers le term dict.
Mais c'est de l'optimisation, pas du compat.

### 4. Playground

Mettre à jour l'UI :
- Retirer les types deprecated du sélecteur principal
- Les garder dans un menu "legacy" ou les griser
- Afficher les paramètres `anchor_start`, `exact_match`, `distance`
  directement sur le type `contains`

## Plan d'implémentation

1. Ajouter `exact_match: Option<bool>` à QueryConfig
2. Implémenter le filtre exact_match dans SuffixContainsQuery / run_sfx_walk
3. Router `term` → contains + anchor_start + exact_match dans build_query
4. Router `fuzzy` → contains + distance dans build_query
5. Router `regex` → contains + regex dans build_query
6. Tests : vérifier que les anciens query types retournent les mêmes résultats
7. Mettre à jour le playground
