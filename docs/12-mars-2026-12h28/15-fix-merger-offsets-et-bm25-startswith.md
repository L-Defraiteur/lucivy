# Fix merger offsets + BM25 scoring pour startsWith

## Bug : panic startsWith avec highlights après merge de segments

### Symptôme

Recherche `startsWith` (ou `startsWith_split`) avec highlights activés → panic WASM :
```
range end index 2 out of range for slice of length 0
panicked at src/positions/reader.rs:137:74
```

Uniquement sur des gros datasets (ex: clone du repo rag3db, ~1500 fichiers texte) où les batch commits (tous les 200 fichiers) déclenchent des merges de segments. `contains` ne crashait pas car il utilise les champs `._ngram` indexés avec `IndexRecordOption::Basic` (pas d'offsets).

### Cause racine

`src/indexer/merger.rs:455` — le merger appelait `field_serializer.write_doc()` (positions seulement) au lieu de `write_doc_with_offsets()`. Après un merge, le fichier `.offsets` du segment résultant était vide/désynchronisé. Quand `AutomatonWeight::scorer` lisait les postings avec `WithFreqsAndPositionsAndOffsets` pour capturer les highlights, le `PositionReader` tentait de lire des données d'offsets inexistantes → panic.

### Fix

Dans `write_postings_for_field`, quand `segment_postings_option.has_offsets()` :
1. Lire les offsets absolus du doc source via `segment_postings.offsets()`
2. Convertir en deltas (from/to)
3. Appeler `field_serializer.write_doc_with_offsets()` au lieu de `write_doc()`

### Test

`test_merge_preserves_offsets` dans `src/indexer/merger.rs` :
- Crée 2 segments avec un champ `WithFreqsAndPositionsAndOffsets`
- Force un merge
- Lit les postings avec offsets → vérifie que les offsets byte [0, 5) de "hello" sont préservés
- Test passe

Test d'intégration `tests/merge_offsets_rag3db.rs` :
- Clone le repo rag3db (`/tmp/rag3db-test`)
- Indexe ~1500 fichiers texte avec commits tous les 200 fichiers (comme le playground)
- Recherche `startsWith("fn")` et `startsWith("struct")` avec highlights
- Vérifie que les résultats existent et que les highlights sont capturés sans panic

## Score toujours 1.0 pour startsWith — en cours

`AutomatonWeight::scorer` utilise `ConstScorer` → tous les docs matchés reçoivent score = 1.0 (pas de BM25). C'est le comportement hérité de tantivy pour les queries automaton (fuzzy, regex).

### Plan d'implémentation

Modifier `AutomatonWeight::scorer` pour accumuler des scores BM25 :

1. Pour chaque terme matché par l'automate FST, calculer un `Bm25Weight` basé sur son `doc_freq` et le `total_num_tokens` du segment
2. Lire les postings avec `WithFreqs` pour obtenir le `term_freq` par doc
3. Pour chaque doc, accumuler `bm25.score(fieldnorm_id, term_freq)` dans un `Vec<Score>` dense (indexé par doc_id)
4. Utiliser le `FieldNormReader` du champ pour le fieldnorm
5. Construire un scorer custom (`BitSetDocSet` + lookup dans le vec de scores)

Le `ConstScorer` ne sera plus utilisé quand le scoring est activé.

Fichier à modifier : `src/query/automaton_weight.rs`

Import déjà ajouté : `Bm25Weight`, `FieldNormReader`

## Fichiers modifiés

```
src/indexer/merger.rs              # fix write_doc → write_doc_with_offsets + test
src/query/automaton_weight.rs      # import Bm25Weight/FieldNormReader (BM25 en cours)
tests/merge_offsets_rag3db.rs      # test intégration rag3db
bindings/emscripten/pkg/*          # rebuild WASM avec le fix
playground/pkg/*                   # rebuild WASM
```

## Commit

`1142e4f fix: preserve offsets during segment merge + playground git clone + startsWith UI`

## Prochaine étape

Implémenter le BM25 scoring dans `AutomatonWeight::scorer`.
