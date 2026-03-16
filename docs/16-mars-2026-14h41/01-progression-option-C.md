# Option C : Progression — 16 mars 2026

## Résumé

On implémente la suppression du champ `._raw` en rendant le `.sfx` + `.sfxpost` autonomes. Le `.sfxpost` est un fichier séparé (un par champ par segment) qui stocke les posting entries complètes : `(doc_id, token_index, byte_from, byte_to)` par ordinal, delta-VInt encodé.

## Commits réalisés

### U4 (ngram removal) — terminé
- `efccb8b` — Re-route contains → SuffixContainsQuery, supprimer ._ngram du schema, supprimer NgramFilter
- `5567205` — Supprimer NgramContainsQuery (1419 lignes), cleanup 6 bindings, benchmarks

### BM25 term_freq fix
- `41f289d` — Utiliser le vrai term_freq (comptage des matches par doc) au lieu de tf=1

### .sfxpost format et infrastructure
- `705f5b9` — Format .sfx v2 avec postings embarquées (annulé ensuite)
- `f434e89` — Séparation en fichier `.sfxpost` indépendant, SfxPostingsReader
- `31e0561` — Enrichir .sfxpost : entries complètes (doc_id, ti, byte_from, byte_to)
- `8fea1b5` — SegmentReader charge les .sfxpost depuis le manifest

### Rerouting des queries
- `6b4d639` — SuffixContainsQuery lit .sfxpost au lieu de inverted_index(._raw)
- `bba1485` — RegexContinuationQuery lit .sfxpost via PostingResolver trait

## Architecture PostingResolver

Trait `PostingResolver` dans `regex_continuation_query.rs` :
- `SfxPostResolver` : pré-charge les entries depuis .sfxpost, lookup O(1)
- `InvertedIndexResolver` : fallback pour les anciens index sans .sfxpost

Même pattern dans `suffix_contains_query.rs` (closure boxed).

## État des tests
- ld-lucivy : 1196 passed, 0 failed, 7 ignored
- RegexContinuation : 20/20
- SuffixContains : 9/9

## Reste à faire

### BM25 doc_freq
Le `Bm25Weight::for_terms()` lit actuellement le doc_freq depuis le TermDictionary du ._raw. Il faut le lire depuis le `.sfxpost` (méthode `doc_freq(ordinal)` existe déjà).

Fichier à modifier : `suffix_contains_query.rs` (construction du Bm25Weight dans `Query::weight()`).

### SfxTermDictionary
Utilisé par RegexContinuationQuery pour `search_continuation`. Il wrappe le term dict du ._raw. À adapter pour fonctionner sans term dict (le .sfx a déjà les ordinals via SI=0).

### Merger .sfxpost
`merge_sfx()` dans `merger.rs` ne reconstruit pas encore le `.sfxpost` après merge. TODO.

### Suppression du champ ._raw
Une fois que plus rien ne lit le ._raw inverted index :
1. Supprimer le champ ._raw de `build_schema()` dans `handle.rs`
2. Supprimer `raw_field_pairs` et `RAW_SUFFIX`
3. Supprimer l'auto-duplication vers ._raw dans les bindings
4. Virer le fallback `InvertedIndexResolver` des queries

### GC des fichiers .sfxpost
S'assurer que le GC de ManagedDirectory préserve les `.sfxpost` (via le manifest existant).

## Fichiers modifiés (par rapport au dernier commit stable)

```
src/suffix_fst/collector.rs                    — BTreeMap token_postings, build() → (sfx, sfxpost)
src/suffix_fst/file.rs                         — SfxPostingsReader, SfxPostingEntry, decode VInt
src/suffix_fst/mod.rs                          — export SfxPostingsReader, SfxPostingEntry
src/index/segment_reader.rs                    — load + expose .sfxpost files
src/indexer/segment_serializer.rs              — write_sfxpost()
src/indexer/segment_writer.rs                  — écrire .sfxpost à côté de .sfx
src/query/phrase_query/suffix_contains_query.rs — resolver sfxpost, real term_freq BM25
src/query/phrase_query/regex_continuation_query.rs — PostingResolver trait, SfxPostResolver
```
