# Doc 15 — Récap des modes de recherche et pistes d'unification

Date : 20 mars 2026

## Modes disponibles

### contains (SuffixContainsQuery)
- **Moteur** : SFX walk direct sur le suffix FST
- **Cross-token** : oui, continuation hybride (walk + gapmap + walk), depth 8
- **Fuzzy** : oui, `with_fuzzy_distance(d)` — edit distance sur les suffixes pendant le walk
- **Regex** : non (mais bascule sur RegexContinuationQuery si `regex: true`)
- **Fichiers** : `suffix_contains_query.rs`, `suffix_contains.rs`
- **Routing** : `build_contains_query()` dans `lucivy_core/src/query.rs`
- **Variantes** : `contains_split` (split whitespace → boolean should)

### startsWith (SuffixContainsQuery + prefix_only)
- **Moteur** : même SFX walk mais filtré SI=0 (début de token uniquement)
- **Cross-token** : oui, continuation
- **Fuzzy** : oui, `with_fuzzy_distance(d)`
- **Fichiers** : même que contains, flag `prefix_only`
- **Routing** : `build_starts_with_query()` dans `lucivy_core/src/query.rs`
- **Variantes** : `startsWith_split`

### contains + regex:true (RegexContinuationQuery, mode Contains)
- **Moteur** : compile le pattern en DFA regex, walk le SFX avec le DFA
- **Cross-token** : oui, chaîne DFA state → gap bytes (GapMap) → next token walk, depth 64
- **Fuzzy** : oui (Levenshtein DFA cumulable avec regex... mais cas d'usage rare)
- **Fichiers** : `regex_continuation_query.rs`
- **Routing** : `build_contains_regex()` dans `lucivy_core/src/query.rs`

### fuzzy (RegexContinuationQuery, mode Contains)
- **Moteur** : Levenshtein DFA sur le SFX + GapMap chaîné
- **Cross-token** : oui, depth 64. Le DFA absorbe les gaps comme insertions dans le budget d'édition
- **Regex** : non (Levenshtein DFA, pas regex)
- **Fichiers** : `regex_continuation_query.rs`
- **Routing** : `build_fuzzy_query()` dans `lucivy_core/src/query.rs`
- **Note** : avant c'était `FuzzyTermQuery` (match sur un seul token). Maintenant cross-token par défaut.

### regex (RegexContinuationQuery, mode Contains)
- **Moteur** : regex DFA compilé, walk SFX + GapMap
- **Cross-token** : oui, depth 64
- **Fuzzy** : non
- **Fichiers** : `regex_continuation_query.rs`
- **Routing** : `build_regex_query()` dans `lucivy_core/src/query.rs`

### term (TermQuery)
- **Moteur** : lookup direct dans le term dict
- **Cross-token** : non
- **Fuzzy** : non
- **Usage** : match exact d'un token (lowercase). Utilise sfxpost pour les highlights.
- **Fichiers** : `term_query.rs`
- **Réflexion** : pourrait être remplacé par `contains` avec distance=0 si le query = un token entier. Mais term est plus rapide (O(1) lookup vs walk FST). Garder pour l'instant.

### phrase (PhraseQuery)
- **Moteur** : positions inverted index (postings avec positions)
- **Cross-token** : oui (adjacency des positions)
- **Fuzzy** : non
- **Usage** : "hello world" → tokens adjacents dans l'index. Utilise le stemmer si configuré.
- **Fichiers** : `phrase_query.rs`, `phrase_scorer.rs`
- **Réflexion** : le seul mode qui utilise encore les positions de l'inverted index classique. Tous les autres passent par SFX. Candidat à remplacement par `contains` multi-token? Mais phrase a le BM25 scoring natif via positions, et `contains` n'utilise pas les positions BM25. À voir si on veut scorer les résultats contains par fréquence/position un jour.

## Deux moteurs cross-token

### 1. SuffixContainsQuery — continuation hybride
- Walk SFX → résolution ordinals via sfxpost → continuation par gapmap + walk SI=0
- Optimisé pour le cas exact (pas de construction de DFA)
- Depth max 8
- Utilisé par : contains, startsWith

### 2. RegexContinuationQuery — DFA chaîné
- Construit un DFA (Levenshtein ou Regex), walk le SFX avec le DFA
- Traverse les boundaries : DFA state survit au gap, continue sur le token suivant
- Depth max 64
- Utilisé par : fuzzy, regex, contains+regex:true

### Partage commun
Les deux utilisent :
- Le suffix FST (.sfx) pour le walk
- Le GapMap pour vérifier/traverser les boundaries inter-tokens
- Le sfxpost pour la résolution doc_id + byte offsets
- Le SfxFileReader comme interface unifiée

## Pistes d'unification future

### Option A — RegexContinuationQuery absorbe SuffixContainsQuery
RegexContinuationQuery est le plus général. On pourrait lui passer un DFA littéral
(substring exact) au lieu d'une regex. Avantage : un seul chemin cross-token.
Inconvénient : construire un DFA pour un simple substring est du overhead inutile.
Le SFX walk direct de SuffixContainsQuery est plus rapide pour le cas exact.

### Option B — Garder les deux, factoriser la traversée GapMap
Extraire la logique de traversée GapMap dans un trait/helper commun.
Les deux moteurs l'utilisent mais avec des implémentations dupliquées.
Plus propre sans sacrifier la performance.

### Option C — SuffixContainsQuery pour tout, regex en fallback
Pour les cas simples (exact, fuzzy), SuffixContainsQuery est optimal.
Regex = cas rare, ok si un peu plus lent. Garder RegexContinuationQuery juste pour ça.

**Recommandation** : Option B à court terme. Option A si un jour on veut simplifier
radicalement (un seul query type "search" avec des options).

## Questions ouvertes

- **phrase** : est-ce qu'on le garde ? C'est le seul mode qui utilise les positions
  classiques. Si on veut scorer par BM25 les résultats contains, il faudrait ajouter
  le scoring aux résultats SFX. Pour l'instant phrase reste utile pour le scoring.
- **term** : garde pour le lookup O(1), mais `contains` avec un token complet et
  distance=0 donnerait le même résultat (juste plus lent).
- **Stemming** : supprimé du pipeline. phrase tokenize toujours via le tokenizer
  configuré (qui est maintenant RAW_TOKENIZER = lowercase only). Si on revient
  au stemming un jour, phrase serait le premier à en bénéficier.
