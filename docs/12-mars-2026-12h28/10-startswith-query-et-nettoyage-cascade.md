# feat: startsWith query + nettoyage cascade AutomatonPhraseQuery

## Ce qui a été fait

### 1. Nouveau query type `startsWith`

Recherche par préfixe optimale exploitant directement le FST (trie trié par préfixe). Disponible dans tous les bindings via JSON :

```json
{ "type": "startsWith", "field": "body", "value": "async prog", "distance": 1 }
```

**Sémantique multi-token** :
- Tokenise la valeur → `["async", "prog"]`
- Tokens non-derniers : match exact ou fuzzy (Levenshtein DFA complet)
- Dernier token : traité comme **préfixe** — `"prog"` matche `"program"`, `"programming"`, etc.
- Positions consécutives validées (phrase adjacente)
- Fuzzy optionnel (`distance`) sur tous les tokens

**Cascade du dernier token** :
1. Range FST `[prefix..prefix_end)` — tous les termes commençant par le token
2. Prefix fuzzy DFA (`build_prefix_dfa`) si distance > 0

**Cascade des tokens non-derniers** :
1. Exact (term dict lookup)
2. Fuzzy (Levenshtein DFA complet) si distance > 0

**Cas single-token** : route directement vers `FuzzyTermQuery::new_prefix` (pas besoin de phrase).

### 2. Retrait du fallback AutomatonPhraseQuery pour contains

`build_contains_fuzzy` dans `lucivy_core/src/query.rs` avait un fallback quand aucun champ ngram n'était configuré : il passait par `AutomatonPhraseQuery` avec cascade substring/fuzzy substring sur le FST.

Ce fallback était :
- Lent (regex `.*token.*` sur le FST entier par token)
- Trompeur (donnait l'impression que `contains` marchait sans ngram, mais avec des perfs médiocres)
- Hasardeux (fuzzy substring = NFA simulation, résultats imprévisibles)

**Remplacé par une erreur explicite** :
```
contains query requires an ngram field for 'body'.
Configure a _ngram field in your schema for substring search.
```

L'utilisateur sait maintenant qu'il doit configurer un champ ngram pour du `contains`.

### 3. Simplification de la cascade dans AutomatonPhraseWeight

**Avant** : exact → fuzzy → substring (regex) → fuzzy substring (NFA)
**Après** : exact → fuzzy

Les niveaux 3 et 4 (substring, fuzzy substring) ont été retirés de `cascade_term_infos`. Ils étaient utilisés uniquement par le fallback contains sans ngram (retiré ci-dessus).

`CascadeLevel::Substring` et `CascadeLevel::FuzzySubstring` supprimés de l'enum.

Imports retirés : `tantivy_fst::Regex`, `FuzzySubstringAutomaton`.

Le module `fuzzy_substring_automaton.rs` existe encore mais n'est plus utilisé (dead code). Peut être supprimé ultérieurement.

### 4. Refactoring helpers

- `get_automaton_builder(distance)` : helper statique pour le `LevenshteinAutomatonBuilder` cached, extrait de la méthode `cascade_term_infos` pour éviter la duplication entre `cascade_term_infos` et `prefix_term_infos`.

## Fichiers modifiés

```
lucivy_core/src/query.rs                                    # routing startsWith, erreur si contains sans ngram
src/query/phrase_query/automaton_phrase_query.rs             # last_token_is_prefix, new_starts_with()
src/query/phrase_query/automaton_phrase_weight.rs            # cascade simplifiée, prefix_term_infos, dispatch, tests
```

## Tests

1143 tests passés, 0 failed.

**Nouveaux tests** :
- `test_starts_with_single_token_prefix` — `"hel"` matche `"hello"` et `"help"`
- `test_starts_with_multi_token` — `["hello", "wor"]` matche `"hello world"` et `"hello work"`, pas `"hello there"`
- `test_starts_with_fuzzy_prefix` — `"helo"` (typo d=1) + `"wor"` matche `"hello world"`
- `test_starts_with_no_substring_fallback` — `"ell"` ne matche PAS `"hello"` (pas de substring en mode prefix)

**Tests retirés** :
- `test_automaton_phrase_substring` — testait le niveau substring retiré
- `test_automaton_phrase_fuzzy_substring` — testait le niveau fuzzy substring retiré

## Pourquoi startsWith est plus rapide que contains

| Étape | contains (ngram) | startsWith |
|-------|-----------------|------------|
| Candidats | Trigram lookup sur champ _ngram | FST range direct |
| Vérification | Lecture stored text + fuzzy match | Aucune (FST suffit) |
| I/O | Lecture stored text par candidat | Zéro I/O stored |
| Complexité | O(trigrams) + O(candidats × stored_text) | O(len(prefix)) pour le range FST |

Le FST est un trie natif — descendre au noeud du préfixe est O(len(prefix)), puis collecter les termes sous cette branche est linéaire en nombre de résultats. Pas de stored text, pas de trigrams, pas de vérification.
