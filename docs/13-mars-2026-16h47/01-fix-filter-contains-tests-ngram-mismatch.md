# Fix : tests filter `contains` cassés — ngram pairs manquants

Date : 13 mars 2026

## Problème

Deux tests unitaires dans `lucivy_core/src/query.rs` échouaient :
- `test_filter_clause_contains`
- `test_filter_clause_contains_with_fuzzy`

### Cause

`make_filter_index()` créait un schema de test avec un champ `"name"` de type `STRING`, mais :
- Pas de champ `name._ngram` (trigrams pour contains)
- Pas de champ `name._raw` (lowercase pour précision)
- `ngram_pairs` et `raw_pairs` passés vides (`&[]`)

Or `build_contains_query()` exige un ngram field (ligne 358) pour la recherche trigram. Sans ngram pair → `resolve_ngram_field()` retourne `None` → erreur.

Le commentaire du test disait "falls back to RegexQuery" — c'était vrai avant l'ajout des ngrams obligatoires, mais le test n'avait pas été mis à jour.

### Fix

`make_filter_index()` enrichi pour créer le schema complet :
- Ajout `name._raw` (TextFieldIndexing, tokenizer "default")
- Ajout `name._ngram` (TextFieldIndexing, tokenizer "ngram")
- Enregistrement du tokenizer `"ngram"` (SimpleTokenizer + LowerCaser + NgramFilter)
- Retourne `(schema, index, raw_pairs, ngram_pairs)` au lieu de `(schema, index)`

Tous les appels à `build_filter_clause` dans les tests mis à jour pour passer les pairs.

### Résultat

69/69 tests lucivy_core passent. 1096/1096 tests ld-lucivy passent.

## Action pour les autres bindings

**À vérifier** : les bindings (lucivy_fts bridge cxx, WASM, Python, Node) qui construisent des queries `contains` ou des filtres `contains` passent-ils bien les ngram_pairs ? Si un binding crée un index sans ngram fields et tente une query contains, le même crash se produira à l'exécution.

Points à checker :
- `lucivy_fts/rust/src/bridge.rs` — les fonctions search passent-elles raw_pairs et ngram_pairs ?
- `bindings/wasm/src/lib.rs` — idem
- Tout binding Python/Node éventuel
