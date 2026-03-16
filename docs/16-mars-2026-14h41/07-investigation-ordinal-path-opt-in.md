# Investigation : path ordinal opt-in — 16 mars 2026

## Problème identifié

Après Phase 7c, le champ principal a :
- **Inverted index** : tokens **stemmés** (pour phrase/parse)
- **.sfx/.sfxpost** : tokens **raw** (pour term/fuzzy/regex/contains)

Le path ordinal (Phases 2-4) s'active quand `.sfxpost_file(field).is_some()`. Après le fix GC (commit `738e47d`), les .sfxpost survivent pour TOUS les text fields. Conséquence : le path ordinal s'active pour les TermQuery/FuzzyTermQuery standards venant du QueryParser, qui cherchent des tokens **stemmés** dans un .sfxpost qui contient des tokens **raw** → résultats incorrects.

### Cas concret

```
Index: champ "body" avec stemmer français
Document: "programming en Rust"

Inverted index (stemmé) : ["programm", "en", "rust"]
.sfxpost (raw)          : ["programming", "en", "rust"]

QueryParser("programming") → tokenize → "programm" (stemmé)
  → TermQuery(field="body", term="programm")
  → Path ordinal : sfx_dict.get_ordinal("programm") → None (pas dans .sfx raw)
  → Résultat: 0 docs ✗ (devrait être 1)

QueryParser("programming") → fallback inverted index
  → inverted_index.get_term_info("programm") → Found
  → Résultat: 1 doc ✓
```

### Tests cassés (12 failures)

1. **Boolean/Term tests** : `scorer.is::<TermScorer>()` → `ResolvedTermScorer` ≠ `TermScorer`
2. **Fieldnorm tests** : BM25 scores différents (doc_freq depuis resolver vs TermInfo)
3. **Merger tests** : empty results post-merge
4. **BlockWAND test** : `ResolvedTermScorer` n'a pas de block structure

## Solution : flag `prefer_sfxpost`

Le type de **query** détermine s'il faut utiliser .sfxpost ou l'inverted index :

| Query type | Source | Route actuelle (._raw) | Route après 7c |
|-----------|--------|----------------------|----------------|
| term | lucivy_core | → ._raw field | → main field + `prefer_sfxpost=true` |
| fuzzy | lucivy_core | → ._raw field | → main field + `prefer_sfxpost=true` |
| regex | lucivy_core | → ._raw field | → main field + `prefer_sfxpost=true` |
| contains | lucivy_core | → ._raw field | → main field (SuffixContainsQuery toujours .sfx) |
| startsWith | lucivy_core | → ._raw field | → main field (AutomatonPhraseWeight toujours .sfx) |
| phrase | lucivy_core | → main field | → main field (inverted index, inchangé) |
| parse | QueryParser | → main field | → main field (inverted index, inchangé) |
| TermQuery (direct) | tests ld-lucivy | → field directement | → inverted index (pas de flag) |

### Changements nécessaires

#### 1. AutomatonWeight — `prefer_sfxpost: bool`

```rust
pub struct AutomatonWeight<A> {
    // ... existing fields ...
    prefer_sfxpost: bool,  // NEW
}
```

- Default `false` (constructeur `new()`)
- `.with_prefer_sfxpost(true)` builder method
- `scorer()` : remplacer le guard `reader.sfxpost_file(field).is_some()` par `self.prefer_sfxpost && reader.sfxpost_file(field).is_some()`

Propagation :
- `FuzzyTermQuery::specialized_weight()` → `AutomatonWeight::new(...).with_prefer_sfxpost(enable)`
- `RegexQuery::weight()` → `AutomatonWeight::new(...).with_prefer_sfxpost(enable)`
- Le flag est passé depuis `FuzzyTermQuery` et `RegexQuery` qui ont un `prefer_sfxpost: bool`

#### 2. TermWeight — `prefer_sfxpost: bool`

```rust
pub struct TermWeight {
    // ... existing fields ...
    prefer_sfxpost: bool,  // NEW
}
```

- Réactiver le path ordinal de Phase 3 mais gardé par `self.prefer_sfxpost`
- `TermQuery::specialized_weight()` propage le flag
- `TermQuery` a un `prefer_sfxpost: bool`

#### 3. lucivy_core query routing

```rust
// term, fuzzy, regex → prefer_sfxpost = true
fn build_term_query(...) -> ... {
    let query = TermQuery::new(term, record_option)
        .with_prefer_sfxpost(true);  // NEW
    ...
}

fn build_fuzzy_query(...) -> ... {
    let query = FuzzyTermQuery::new(term, distance, transposition)
        .with_prefer_sfxpost(true);  // NEW
    ...
}
```

#### 4. Queries non impactées

- **SuffixContainsQuery** : toujours .sfx (pas de flag nécessaire)
- **AutomatonPhraseWeight** : toujours .sfx (pas de flag nécessaire)
- **PhraseQuery** : toujours inverted index (pas de flag)
- **QueryParser** : crée des TermQuery sans flag → inverted index

### Ordre d'implémentation

1. Ajouter `prefer_sfxpost` à `AutomatonWeight` + guard
2. Ajouter `prefer_sfxpost` à `TermWeight` + réactiver Phase 3 ordinal path avec guard
3. Propager dans `FuzzyTermQuery`, `RegexQuery`, `TermQuery`
4. lucivy_core : set `prefer_sfxpost=true` dans routing
5. lucivy_core : remove ._raw from schema, `raw_field_pairs`
6. merger : filter par .sfx presence au lieu de ._raw
7. bindings : remove auto-duplication

### Estimation

- ~30 lignes : flags dans AutomatonWeight, TermWeight
- ~20 lignes : propagation dans FuzzyTermQuery, RegexQuery, TermQuery
- ~10 lignes : lucivy_core routing
- ~100 lignes supprimées : ._raw schema, raw_field_pairs, auto-duplication
