# 08 — Findings : optimisations regex & contains/fuzzy via structures existantes

## Contexte

Exploration complète des structures de données disponibles à la recherche pour identifier les optimisations non exploitées. Concerne `regex_contains_via_literal` (regex), `cross_token_search_with_terms` (exact/fuzzy), et `continuation_score_sibling` (continuation DFA).

## Structures disponibles sous-exploitées

### 1. `has_doc(ordinal, doc_id)` — O(log n), zéro payload decode

**Où** : `PostingResolver` trait, implémenté dans `SfxPostResolverV2` via binary search sur les doc_ids triés du `.sfxpost` v2.

**Pourquoi c'est important** : `resolve(ordinal)` décode TOUS les postings (VInt packed positions + byte offsets) pour TOUS les docs. `has_doc()` fait juste un binary search sur les doc_ids triés — pas de décodage payload.

**Impact regex** : dans l'intersection multi-littérale, au lieu de :
```
resolve(ver_ordinal) → Vec<PostingEntry> pour TOUS docs → collect doc_ids → intersect
```
On peut faire :
```
pour chaque doc du primary : has_doc(ver_ordinal, doc_id) → O(log n) → filtre
```
Élimine le resolve de tous les ordinals des littéraux secondaires.

**Impact fuzzy/contains** : non applicable (resolve-last déjà en place).

### 2. `resolve_filtered(ordinal, &doc_ids)` — O(k log n)

**Où** : `PostingResolver` trait. V2 fait un binary search par doc_id demandé, ne décode que les payloads des docs matchants.

**Pourquoi c'est important** : Phase 3c fait `resolve(ordinal)` puis filtre par `allowed_docs`. Avec `resolve_filtered`, on skip le décodage des docs éliminés.

**Impact regex** : Phase 3c (gap>0 loop), Phase 3a (accepted ordinals), Phase 3b (gap=0 chains). Partout où on a un `allowed_docs`, passer par `resolve_filtered`.

**Impact fuzzy/contains** : Step 3 de `cross_token_search_with_terms` fait `raw_term_resolver(ord)` sans filtre. Si on avait un doc set connu à l'avance, on pourrait filtrer. Mais le contains n'a pas de pré-filtre multi-littéral — pas applicable directement.

### 3. `doc_freq(ordinal)` — O(1), choix du primary

**Où** : `PostingResolver` trait. V2 lit juste le header `num_unique_docs` sans toucher les postings.

**Pourquoi c'est crucial** : Pour `rag.*ver`, `doc_freq("rag")` ≈ 120, `doc_freq("ver")` ≈ 5. Si on choisit "ver" comme primary (plus sélectif), Phase 3c tourne sur 5 docs au lieu de 120.

**Heuristique** : parmi tous les littéraux >= MIN_LITERAL_LEN, choisir celui avec le plus petit `doc_freq` comme primary. Ça demande un `prefix_walk` + somme des `doc_freq` par littéral. Coût : O(prefix_walk_entries) par littéral, une fois.

**Impact regex** : massif pour les patterns type `common.*rare`. Le primaire sélectif réduit tout le reste.

**Impact fuzzy/contains** : pas applicable (une seule query, pas de choix de littéral).

### 4. `token_len` dans ParentEntry — gratuit

**Où** : Retourné par `prefix_walk` et `falling_walk` dans chaque `ParentEntry`. Disponible sans resolve.

**Pourquoi** : Si le regex a une longueur minimum connue (ex: `shard[a-z]{3,}` → min 8 chars), on peut pruner les tokens trop courts avant le DFA.

**Impact regex** : mineur pour les patterns courants, utile pour les patterns avec quantifiers `{n,m}`.

**Impact fuzzy/contains** : la query a une longueur fixe, le falling_walk filtre déjà par `si + prefix_len == token_len`.

### 5. GapMap `read_separator` fast rejection

**Où** : `gapmap.rs`, `read_separator(doc_id, ti_a, ti_b)` retourne `None` si `ti_b != ti_a + 1` ou si VALUE_BOUNDARY, **sans lire les gap bytes**.

**Impact regex** : Phase 3c lit déjà `read_separator`. Le fast rejection est automatique. Mais on pourrait pré-vérifier `num_tokens(doc_id)` pour savoir si la chaîne de tokens est assez longue.

**Impact fuzzy/contains** : déjà utilisé implicitement.

### 6. Bit 63 single-parent detection

**Où** : `builder.rs`, encodage inline. Bit 63 = 0 → parent unique, SI + token_len inline dans la valeur FST. Pas de lookup OutputTable.

**Impact** : marginal — le décodage est déjà rapide.

## Optimisations applicables au regex (priorité)

### Priorité 1 : choix du primary par doc_freq (le plus sélectif)

```rust
// Au lieu de pick_best_literal par longueur/position :
fn pick_most_selective(literals: &[String], sfx_reader, resolver) -> String {
    literals.iter()
        .filter(|l| l.len() >= MIN_LITERAL_LEN)
        .min_by_key(|l| {
            let walk = sfx_reader.prefix_walk(l);
            walk.iter().map(|(_, p)| {
                p.iter().map(|pe| resolver.doc_freq(pe.raw_ordinal)).sum::<u32>()
            }).sum::<u32>()
        })
        .cloned()
}
```

Gain estimé : 10-100x pour `common.*rare`.

### Priorité 2 : intersection par has_doc au lieu de resolve

```rust
// Au lieu de :
for (_, parents) in &other_walk {
    for p in parents {
        for e in &resolver.resolve(p.raw_ordinal) {  // EXPENSIVE: decode all
            docs.insert(e.doc_id);
        }
    }
}

// Faire :
// 1. Résoudre le primary (plus petit set) → primary_docs
// 2. Pour chaque autre littéral, tester has_doc par doc du primary
for &doc_id in &primary_docs {
    let all_present = other_ordinals.iter().any(|ord| resolver.has_doc(*ord, doc_id));
    if all_present { survivors.insert(doc_id); }
}
```

Gain : évite le decode VInt de tous les postings des littéraux secondaires.

### Priorité 3 : resolve_filtered dans Phase 3

```rust
// Au lieu de :
let entries = resolver.resolve(ord);
for e in &entries {
    if allowed_docs.contains(&e.doc_id) { ... }
}

// Faire :
let entries = resolver.resolve_filtered(ord, &allowed_docs);
for e in &entries { ... }  // déjà filtré, moins de payload décodé
```

### Priorité 4 : position ordering strict par byte offsets

Déjà implémenté : `byte_from(next) >= byte_to(current)`. Assure que les littéraux ne se chevauchent pas et sont dans l'ordre.

## Optimisations applicables au fuzzy/contains

### Le fuzzy est déjà bien optimisé

`cross_token_search_with_terms` utilise le pattern resolve-last correctement :
1. `falling_walk` au niveau ordinal → O(query_len)
2. Sibling chain au niveau ordinal → O(chain_depth × siblings)
3. Resolve uniquement pour les chaînes validées → O(matched_chains × df)

### Opportunité : sibling filtering par gap_len aussi pour fuzzy cross-token

Actuellement `cross_token_search_with_terms` utilise `contiguous_siblings` (gap=0 seulement). Si un jour on veut du fuzzy cross-gap (ex: "getElementById" cherché comme "getelementbyid" avec des espaces tolérés), on pourrait utiliser `siblings()` (tous gap_len) + GapMap validation, comme le regex le fait.

### Opportunité : doc_freq pour early termination

Pour les queries très fréquentes (fuzzy "the" d=1), le resolve de toutes les chaînes valides peut être coûteux. `doc_freq` pourrait estimer la charge et adapter la stratégie (ex: prescan vs direct scoring).

### Pas d'intersection multi-littérale pour le fuzzy

Le fuzzy/contains travaille sur UNE seule query string. Pas de décomposition en littéraux. L'intersection multi-littérale est spécifique au regex.

## Résumé des gains attendus

| Optimisation | Applicable à | Gain estimé | Complexité |
|---|---|---|---|
| Primary par doc_freq | regex | 10-100x pour `common.*rare` | faible |
| Intersection par has_doc | regex | 5-10x (skip decode payload) | faible |
| resolve_filtered | regex, continuation | 2-5x (skip docs éliminés) | trivial |
| Position ordering byte | regex | déjà implémenté | — |
| token_len pruning | regex | mineur | trivial |
| Sibling gap>0 pour fuzzy | fuzzy | futur (pas besoin actuel) | moyen |

## Architecture cible regex (post-optims)

```
1. extract_all_literals(pattern) → ["rag", "ver", "end"]
2. Pour chaque: prefix_walk → doc_freq → trier par sélectivité
3. Primary = plus sélectif → resolve → primary_docs
4. Pour chaque autre littéral: has_doc(ordinal, doc) pour chaque doc du primary
5. Intersection → survivors
6. Position ordering strict (byte_from/byte_to) → filtered survivors
7. Phase 3c sur filtered survivors avec resolve_filtered
8. Ou: emit directement si le regex entre littéraux est `.*` (tout accepte)
```
