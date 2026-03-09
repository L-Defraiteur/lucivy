# Investigation requise : fuzzy contains highlights incohérents

## Symptômes observés

### 1. Highlights sans rapport avec la query
- Query "ownership of" en mode auto/contains_split, distance 1
- Highlights sur "MemoryArena", "Customized", "storing", "TokenFilter", etc.
- Explication partielle : "of" (2 chars) avec distance 1 → tout mot contenant "o" matche (car "o" est à distance 1 de "of" par suppression du "f")
- Mais certains highlights semblent aller au-delà de ce que Levenshtein devrait matcher

### 2. Résultats non-déterministes
- Query "ownership takes" en mode contains_split
- Première exécution : highlights sur "updates" (qui ne devrait pas matcher "takes" même avec distance 1)
- Deuxième exécution : résultats différents pour la même query
- **Comportement non-reproductible = bug sérieux**

### 3. Vérification Levenshtein possiblement insuffisante
- Hypothèse : le pipeline ngram candidates → vérification ne fait peut-être pas un vrai calcul de distance Levenshtein
- Le flow actuel dans `build_contains_fuzzy` (lucivy_core/src/query.rs) :
  1. Tokenize query → trigrams
  2. NgramContainsQuery trouve les candidats via ngram index
  3. VerificationMode::Fuzzy vérifie les candidats
- **Question** : la vérification vérifie-t-elle réellement la distance d'édition, ou juste la présence des ngrams/tokens ?
- L'ordre des lettres est-il vérifié ?

## Fichiers à investiguer

- `lucivy_core/src/query.rs` l.319-416 — build_contains_fuzzy, construction de NgramContainsQuery et AutomatonPhraseQuery
- `src/query/ngram_contains_query.rs` — NgramContainsQuery, comment les candidats sont vérifiés
- `src/query/automaton_phrase_query.rs` — AutomatonPhraseQuery, le fallback FST
- `src/query/fuzzy_params.rs` ou similaire — FuzzyParams et la logique de vérification
- HighlightSink — comment les offsets sont enregistrés, possible race condition expliquant le non-déterminisme ?

## Pistes pour le non-déterminisme

- Threading dans emscripten (pthreads) — race condition dans le HighlightSink ?
- Ordre d'itération des segments (merge en cours ?)
- Cache ou état global entre les recherches ?

## Actions

- [ ] Ajouter un test reproductible : "ownership takes" distance 1 → vérifier que "updates" ne matche PAS
- [ ] Lire le code de vérification fuzzy dans NgramContainsQuery pour confirmer qu'il fait un vrai Levenshtein
- [ ] Vérifier si HighlightSink est thread-safe (Arc + Mutex ou lock-free ?)
- [ ] Envisager un boost de score inversement proportionnel à la distance d'édition réelle + pénalité pour mots courts
