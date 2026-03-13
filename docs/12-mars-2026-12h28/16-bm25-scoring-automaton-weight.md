# BM25 scoring pour AutomatonWeight (startsWith, fuzzy, regex)

## Contexte

Suite au fix merger offsets (doc 15), le scoring startsWith retournait toujours 1.0 pour tous les docs matchés. `AutomatonWeight::scorer` utilisait `ConstScorer` — pas de BM25.

## Implémentation

### `AutomatonScorer` (nouveau)

Struct qui wraps `BitSetDocSet` + un `Vec<Score>` dense (indexé par doc_id) pour stocker les scores BM25 pré-calculés par terme.

```rust
struct AutomatonScorer {
    doc_bitset: BitSetDocSet,
    scores: Vec<Score>,    // scores[doc_id] = somme BM25 de tous les termes matchés
    boost: Score,
}
```

`score()` retourne `scores[doc] * boost`.

### Scoring BM25 dans `scorer()`

Pour chaque terme matché par l'automate FST :
1. Calcule `Bm25Weight::for_one_term_without_explain(doc_freq, total_num_docs, avg_fieldnorm)` — stats per-segment
2. Lit les postings avec `WithFreqs` pour obtenir `term_freq` par doc
3. Accumule `bm25.score(fieldnorm_id, term_freq)` dans le vecteur de scores

`FieldNormReader` : fallback sur `constant(max_doc, 1)` si le champ n'a pas de fieldnorms (ex: champs JSON).

### 3 paths dans `scorer()`

| Condition | Postings lus | Scorer | Vitesse |
|-----------|-------------|--------|---------|
| `highlight_sink` présent | `WithFreqsAndPositionsAndOffsets` | `AutomatonScorer` (BM25 + highlights) | Le plus lent |
| `scoring_enabled = true` | `WithFreqs` (doc-by-doc) | `AutomatonScorer` (BM25) | Moyen |
| `scoring_enabled = false` | `Basic` (block) | `ConstScorer` (score = boost) | Le plus rapide |

### `scoring_enabled` — opt-in via `EnableScoring`

- **Défaut : `false`** (fast path, pas de BM25)
- Activé automatiquement quand `order_by_score()` est utilisé (`EnableScoring::Enabled`)
- Setter : `AutomatonWeight::with_scoring(bool)`

Propagé dans :
- `FuzzyTermQuery::weight()` → `with_scoring(enable_scoring.is_scoring_enabled())`
- `RegexQuery::weight()` → idem
- `TermSetQuery::specialized_weight()` → idem

## Tests

- `test_automaton_weight` : scoring désactivé par défaut → score = 1.0 (ConstScorer)
- `test_automaton_weight_boost` : boost 1.32x vérifié
- `test_automaton_weight_bm25` : scoring activé → score > 0 et ≠ 1.0
- Tests fuzzy, regex, set_query adaptés (assertions `score > 0.0` au lieu de `== 1.0`)
- **1093/1093 tests passent**

## WASM rebuild

Build OK. `lucivy.wasm` = 6.6 MB, `lucivy.js` = 87 KB. Copié dans `playground/pkg/`.

## Fichiers modifiés

```
src/query/automaton_weight.rs    # AutomatonScorer, 3 paths scoring, with_scoring()
src/query/fuzzy_query.rs         # propagation scoring_enabled
src/query/regex_query.rs         # propagation scoring_enabled
src/query/set_query.rs           # propagation scoring_enabled
bindings/emscripten/pkg/*        # rebuild WASM
playground/pkg/*                 # rebuild WASM
```

## Note : bug lock file lucivy (doc 19 rag3weaver)

Le rapport sparse-v2 (doc 19 rag3weaver) mentionne un bug lock file lucivy ("cannot create writer: LockBusy") lors du test close → reopen dans le même process. Investigation en cours : `IndexWriter::Drop` envoie `Shutdown` aux acteurs de façon asynchrone, le `DirectoryLock` est droppé implicitement mais les merge threads pourraient encore tourner. Pas lié à nos changements BM25/merger.
