# VISION-02 — Trigram pre-filter hybride pour big data

Date : 15 mars 2026

## Constat

Le suffix FST attaque la recherche depuis un seul point d'entrée (début du
suffix) et prune via `can_match()` du DFA. C'est optimal pour la plupart des
cas.

Mais pour des queries longues et floues (ex: contains("configurationManager", d=3)),
le DFA a un branching factor énorme (20 chars × distance 3). Le walk explore
beaucoup de branches avant de les pruner.

Les trigrams attaquent depuis N points simultanés (18 trigrams pour un mot de
20 chars). L'intersection de ces posting lists massacre les candidats
immédiatement — c'est un filtre multi-point.

## Quand ça compte

À 50K tokens, les deux approches sont instantanées. La différence n'est pas
mesurable.

À 100M+ tokens (big data, sharding), le multi-point des trigrams comme
**pré-filtre** avant le suffix FST walk pourrait réduire significativement
l'espace de recherche.

## Architecture hybride proposée

```
Query longue + fuzzy:
  1. Trigram extraction → intersection posting lists → 50 termes candidats
  2. Suffix FST walk ciblé sur ces 50 termes (au lieu du full tree)
  3. GapMap validation des séparateurs
```

Le suffix FST reste la source de vérité (zéro faux positif). Les trigrams
servent uniquement de pré-filtre pour réduire l'espace de recherche du DFA walk.

## Optimisation des trigrams avec GapMap

Plutôt que vérifier les candidats trigram via stored text (LZ4 décompression),
on peut vérifier directement via posting list intersections + GapMap. Zéro
stored text même dans le path trigram.

## Le multi-point n'est PAS gratuit

6 prefix walks (un par trigram) c'est 6 traversées FST. Le walk unique avec
Levenshtein DFA c'est 1 traversée. Le "pré-filtre" peut coûter plus que le
filtre lui-même.

Le win c'est seulement quand :
- Le DFA explose en branches (d>=2, terme long, 20+ chars)
- L'intersection coupe 99% des candidats avant le walk DFA
- Le corpus est assez grand pour que le branching factor soit un problème

C'est un trade-off nombre-de-walks vs branching-factor-par-walk.
À benchmarker sur corpus réel, pas une certitude théorique.

## Note pour retrouver le code trigram

Si on supprime les trigrams en U4, le dernier commit avec le code ngram
complet est sur la branche `feature/sfx-unified`. Le code vit dans :
- `src/query/phrase_query/ngram_contains_query.rs`
- `src/tokenizer/ngram_tokenizer.rs`
- `lucivy_core/src/tokenizer.rs` (NgramFilter)

## Prérequis pour valider le multi-point

- Corpus > 100M tokens
- Benchmark : DFA direct vs multi-point (6 prefix walks + intersection)
- Mesurer sur queries longues fuzzy d=2 et d=3 spécifiquement
- Si le multi-point gagne, le réimplémenter via suffix FST prefix walks
  (pas besoin d'un champ ngram séparé)
