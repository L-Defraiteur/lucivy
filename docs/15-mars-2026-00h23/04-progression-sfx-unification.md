# Progression — U2 unification sfx

Date : 15 mars 2026

## Résumé

Session de travail sur l'unification du .sfx comme term dictionary unique.
Avancée significative, bloqué sur un point qui nécessite un tokenizer unifié.

## Travail réalisé

### SfxTermDictionary (nouveau)
- `src/suffix_fst/term_dictionary.rs`
- Wraps SfxFileReader + TermDictionary existant
- API : `get()`, `term_ord()`, `search_automaton()`, `range_scan()`, `stream_all()`
- Filtre SI=0 pour exact/prefix/fuzzy (même résultats que TermDictionary standard)
- 3 tests : parité exact lookup, range scan, fuzzy automaton

### AutomatonPhraseQuery unifié
- `cascade_term_infos()` et `prefix_term_infos()` utilisent SfxTermDictionary
- Fallback supprimé — .sfx est obligatoire
- startsWith, exact, fuzzy passent tous par le .sfx
- 9 tests automaton_phrase passent

### .sfx pour tous les champs Str indexés
- segment_writer génère des SfxCollectors pour tous les `FieldType::Str` indexés
  (plus seulement les `._raw`)
- Condition changée de `ends_with("._raw")` à `FieldType::Str + indexing_options`

### SI étendu à u16
- Encoding single-parent : SI passe de 8 bits (max 255) à 16 bits (max 65535)
- Compatible avec le format OutputTable (encode/decode déjà en u16)

### Guard MAX_TOKEN_LEN dans SfxCollector
- Tokens > MAX_TOKEN_LEN (65530) skippés dans `add_token()` (cohérent avec postings_writer)

## Point bloquant : tokens longs

### Problème
2 tests échouent : `test_drop_token_that_are_too_long` et `test_index_max_length_token`.

Ces tests créent des tokens de 65530 chars. Même avec SI u16, le SfxCollector
génère 65530 suffixes par token → O(n²) en mémoire dans le BTreeMap du builder
(chaque suffixe est une string jusqu'à 65K chars). Le process consomme 2+ GB
de RAM et prend des minutes.

### Cause racine
Le tokenizer actuel (SimpleTokenizer) ne split pas les tokens longs. Un blob
base64, une URL de 65K chars, ou un identifiant CamelCase très long arrive
comme un seul token.

### Solution nécessaire : tokenizer unifié
Plutôt qu'un cap arbitraire (SI max 255), implémenter un tokenizer/filter qui :

1. **Force-split les tokens longs** : au-delà de N chars (ex: 256), découper
   en chunks. Chaque chunk = un token normal avec ses propres suffixes.
   Séparateur "" entre chunks → le multi-token search traverse la frontière.

2. **CamelCase splitting** : "getElementById" → ["get", "element", "by", "id"]
   avec séparateurs "". Permet de chercher "Element" comme SI=0 au lieu de
   suffix profond.

3. **Doit être appliqué AVANT les deux systèmes** (posting writer + SfxCollector)
   pour que les ordinals restent cohérents. Sinon le SfxTermDictionary retourne
   des TermInfo incorrects.

4. **Impact** : change le comportement de l'indexation pour tous les champs Str.
   Tous les index doivent être reconstruits. Tests à adapter.

### Prochaine étape
Créer un plan propre pour le tokenizer unifié, puis l'implémenter.
L'unification U2 reprendra ensuite.

## Branches

- `feature/sfx-contains` : sfx contains, merge, pivot, fuzzy — STABLE
- `feature/sfx-unified` : U2 en cours — WIP, 2 tests cassés

## Fichiers modifiés

```
src/suffix_fst/term_dictionary.rs    ← NOUVEAU (SfxTermDictionary)
src/suffix_fst/mod.rs                ← export SfxTermDictionary
src/suffix_fst/file.rs               ← expose fst(), parent_list_data()
src/suffix_fst/builder.rs            ← SI u16 (16 bits)
src/suffix_fst/collector.rs          ← guard MAX_TOKEN_LEN
src/indexer/segment_writer.rs        ← sfx pour tous les Str indexés
src/query/phrase_query/automaton_phrase_weight.rs ← SfxTermDictionary, no fallback
```
