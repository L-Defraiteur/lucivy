# 08 — BUG CRITIQUE : ordinal mismatch SFX vs term dict

Date : 28 mars 2026

## Symptôme

`contains "rag3weaver"` retourne 6 résultats au lieu de 54.

Le `falling_walk` trouve ~600 candidats, le sibling table a "weaver" comme
sibling de "rag3", mais `valid_chains=0` dans la plupart des segments.

## Cause racine

**Les ordinals du SFX FST ≠ les ordinals du term dictionary.**

Le `cross_token_search_with_terms` utilise :
- `falling_walk()` → retourne `parent.raw_ordinal` = ordinal dans l'espace SFX
- `sib_table.contiguous_siblings(ord)` → retourne des ordinals SFX
- `ord_to_term(next_ord)` → utilise le **term dictionary** de tantivy

Le term dictionary a ses PROPRES ordinals (ordre alpha des tokens complets).
Le SFX FST a ses PROPRES ordinals (ordre alpha des tokens, construits par
`SuffixFstBuilder`/`SfxCollector`).

Ces deux sets d'ordinals ne sont PAS les mêmes :
- Le term dict est construit par le postings writer de tantivy
- Le SFX est construit par notre `SfxCollector`
- Les deux trient les tokens alphabétiquement, MAIS le term dict peut avoir
  des tokens que le SFX n'a pas (ou vice versa), ce qui décale tous les ordinals

Quand `ord_to_term(sfx_ordinal)` est appelé, il retourne le token à la position
`sfx_ordinal` dans le term dict, qui est un TOKEN DIFFERENT de celui à la position
`sfx_ordinal` dans le SFX. Résultat : la comparaison `rem == next_text` échoue
silencieusement.

## Pourquoi ça marchait avant dans le playground

Le playground WASM indexe via un git clone (GitHub). Selon la taille de l'index
et le nombre de tokens uniques, les ordinals SFX et term dict PEUVENT
coïncider par chance (quand les deux sets de tokens sont identiques et dans
le même ordre). C'est fragile et non garanti.

## Solution

**ON DOIT SE PASSER DU TERM DICT POUR NOS FONCTIONS SFX.**

Le term dict de tantivy n'est pas conçu pour nos ordinals. On doit avoir notre
propre lookup ordinal → texte qui respecte les ordinals SFX.

### Options

1. **Stocker les tokens dans le SFX lui-même** : ajouter une section "token texts"
   dans le fichier .sfx avec un ordinal → offset mapping. Lecture directe sans
   passer par le term dict.

2. **Construire un mapping SFX ord → term dict ord au load time** : à l'ouverture
   du segment, itérer les deux streams en parallèle et construire une table de
   correspondance. Overhead mémoire mais pas de changement de format.

3. **Utiliser le sfxpost pour reverse-lookup** : le sfxpost a (ordinal, doc_id,
   token_index, byte_from, byte_to). On peut lire le texte depuis le store via
   byte_from/byte_to. Mais c'est lent (I/O par lookup).

### Option recommandée : option 1

Ajouter dans le .sfx un bloc "term texts" :
```
[existing sfx data...]
[4 bytes] num_terms
[8 bytes × (num_terms + 1)] offset table
[concatenated term texts, UTF-8]
```

Le `SfxFileReader` expose `fn term_text(&self, ordinal: u32) -> Option<&str>`.
Remplace tous les appels à `ord_to_term(term_dict_ordinal)` par
`sfx_reader.term_text(sfx_ordinal)`.

### Fonctions impactées

| Fonction | Fichier | Usage de ord_to_term |
|----------|---------|---------------------|
| `cross_token_search_with_terms` | suffix_contains.rs | sibling chain text lookup |
| `find_literal` | literal_resolve.rs | resolve ordinal to text |
| `validate_path` | literal_resolve.rs | feed token text to DFA |
| `fuzzy_contains_via_trigram` | regex_continuation_query.rs | build concat text |
| `regex_contains_via_literal` | regex_continuation_query.rs | DFA validation text |
| `run_sfx_walk` | suffix_contains_query.rs | passes ord_to_term |
| `run_regex_prescan` | regex_continuation_query.rs | passes ord_to_term |
| `run_fuzzy_prescan` | regex_continuation_query.rs | passes ord_to_term |

### Impact

Ce bug affecte TOUTES les recherches cross-token : contains, fuzzy, regex.
Les résultats sont incomplets et non déterministes (dépendent de la coïncidence
entre ordinals SFX et term dict).

### Priorité

CRITIQUE. C'est la base de toute la recherche substring cross-token.
