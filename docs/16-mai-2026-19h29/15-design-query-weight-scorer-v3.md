# Design : Query/Weight/Scorer v3 — types propres + routing

**Date** : 17 mai 2026

---

## 1. Situation actuelle (v2)

```
build_query() dans lucivy_core/src/query.rs
  ↓
  "contains" / "term" / "startsWith" / "phrase" → SuffixContainsQuery
  "fuzzy"                                       → RegexContinuationQuery (DfaKind::Fuzzy)
  "regex"                                       → RegexContinuationQuery (DfaKind::Regex)
```

Problèmes :
- `SuffixContainsQuery` gère à la fois contains, term, startsWith, phrase
- `RegexContinuationQuery` gère à la fois fuzzy et regex (via DfaKind enum)
- Noms pas clairs : "SuffixContains" et "RegexContinuation" sont des détails d'implémentation

---

## 2. Cible v3 — 3 types Query séparés

```
build_query()
  ↓
  "contains" / "term" / "startsWith" / "phrase" → ContainsQuery
  "fuzzy"                                       → FuzzyQuery
  "regex"                                       → RegexQuery
```

Chaque Query type a son propre fichier, son propre Weight, son propre Scorer.

### Aliases rétrocompat

```rust
pub type SuffixContainsQuery = ContainsQuery;
pub type RegexContinuationQuery = RegexQuery; // ou FuzzyQuery selon le DfaKind
```

Comme ça les bindings et le code externe qui référencent les anciens noms continuent de compiler.

---

## 3. Chaque Query type

### 3.1 ContainsQuery

**Paramètres** : field, value, anchor_start, exact_match, strict_separators

**Prescan** :
```
Pour chaque segment :
  if SFX3 → briques::orchestrator::contains_v3(reader, value, resolver, ...)
  else    → ancien code v2 (suffix_contains)
  Cache : (doc_id → tf) + highlights
```

**Weight** : BM25 avec doc_freq agrégé cross-shard

**Scorer** : itère les doc_ids cachés, score BM25

### 3.2 FuzzyQuery

**Paramètres** : field, value, distance, strict_separators

**Prescan** :
```
Pour chaque segment :
  if SFX3 → briques::orchestrator::fuzzy_v3(reader, value, distance, resolver, ...)
  else    → ancien code v2 (fuzzy_contains via RegexContinuationQuery)
  Cache : (doc_id → tf) + highlights + doc_coverage (miss_count)
```

**Weight** : BM25 tiered par miss_count (`miss_penalty * 1000 + bm25`)

**Scorer** : comme ContainsQuery mais avec le tiering

### 3.3 RegexQuery

**Paramètres** : field, pattern, anchor_start

**Prescan** :
```
Pour chaque segment :
  if SFX3 → briques::regex_v3::regex_v3(automaton, pattern, reader, ...)
  else    → ancien code v2 (regex_contains_via_literal)
  Cache : (doc_id → tf) + highlights
```

**Weight/Scorer** : comme ContainsQuery

---

## 4. Structure du prescan (pattern commun)

Les 3 Query types partagent le même pattern de prescan two-pass :

```
Pass 1 (prescan_segments) :
  Pour chaque segment :
    Détecter sfx_version (SFX3 vs SFX1)
    Appeler la brique v3 (ou v2 fallback)
    Cacher résultats dans HashMap<SegmentId, PrescanResult>
    Accumuler doc_freq

Pass 2 (weight + scorer) :
  Utiliser doc_freq global pour BM25 IDF
  Créer scorer qui itère les doc_ids cachés
```

On pourrait extraire ce pattern dans un trait ou une struct commune. Mais vu que chaque type a des subtilités (FuzzyQuery a doc_coverage, RegexQuery a le DFA), un trait serait trop contraint. Mieux : copier le pattern dans chaque fichier (~50 lignes), c'est clair et indépendant.

---

## 5. Fichiers

### À créer

| Fichier | Contenu |
|---------|---------|
| `src/query/contains_query.rs` | ContainsQuery + Weight + Scorer |
| `src/query/fuzzy_query.rs` | FuzzyQuery + Weight + Scorer |
| `src/query/regex_query.rs` | RegexQuery + Weight + Scorer |

### À modifier

| Fichier | Changement |
|---------|------------|
| `src/query/mod.rs` | Ajouter les 3 nouveaux modules + pub use |
| `lucivy_core/src/query.rs` | build_query() route vers les nouveaux types |

### Inchangés (v2 compat, garder)

- `src/query/phrase_query/suffix_contains_query.rs` — garde le code v2
- `src/query/phrase_query/regex_continuation_query.rs` — garde le code v2

---

## 6. Détection de version dans le prescan

```rust
fn detect_and_prescan(
    reader: &SegmentReader,
    field: Field,
    /* query params */
) -> PrescanResult {
    let sfx_data = reader.sfx_file(field);
    let version = sfx_data
        .and_then(|d| d.read_bytes().ok())
        .and_then(|b| section_file::detect_sfx_version(&b))
        .unwrap_or(1);
    
    match version {
        3 => /* briques v3 */,
        _ => /* code v2 existant */,
    }
}
```

---

## 7. Ordre d'implémentation

1. **ContainsQuery** — le plus simple, juste contains_v3 + BM25
2. **FuzzyQuery** — ajoute le tiering par miss_count
3. **RegexQuery** — ajoute le DFA + literal extraction
4. **Routing** — modifier build_query() pour router vers les nouveaux types
5. **Aliases** — type aliases pour rétrocompat
6. **Tests E2E** — vérifier que les mêmes résultats sortent qu'en v2
