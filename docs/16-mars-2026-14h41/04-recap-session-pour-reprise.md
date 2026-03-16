# Récap complet pour reprise — 16 mars 2026

## Branche et état

- **Branche** : `feature/sfx-unified`
- **Dernier commit** : `dbc85d3` (docs: update progression)
- **Compilation** : OK, 0 erreurs
- **Tests** : 1196 passed (ld-lucivy), 64 passed (lucivy-core)

## Objectif global

Supprimer le champ `._raw` du schema en rendant le `.sfx` + `.sfxpost` entièrement autonomes. Option C du plan d'unification SFX.

## Ce qui est fait

### U4 : Suppression des ngrams (TERMINÉ)
- `contains` redirigé vers `SuffixContainsQuery` (fuzzy) et `RegexContinuationQuery` (regex) dans `lucivy_core/src/query.rs`
- NgramContainsQuery supprimé (1419 lignes), NgramFilter supprimé
- ._ngram retiré du schema (`handle.rs`)
- 6 bindings nettoyés (imports, auto-duplication, filtres)
- Commits : `efccb8b`, `5567205`

### BM25 real term_freq
- SuffixContainsQuery utilise maintenant le vrai tf (nombre de matches par doc) au lieu de tf=1
- Commit : `41f289d`

### .sfxpost infrastructure
- Format `.sfxpost` : fichier séparé par champ, posting entries complètes `(doc_id, token_index, byte_from, byte_to)` VInt-encodées
- `SfxCollector` accumule `BTreeMap<String, Vec<(u32, u32, u32, u32)>>` pendant l'indexation
- `build()` retourne `(sfx_bytes, sfxpost_bytes)` — deux fichiers séparés
- `SfxPostingsReader` dans `src/suffix_fst/file.rs` — lit le format, expose `entries(ordinal)` et `doc_freq(ordinal)`
- `SfxPostingEntry` : struct publique `{doc_id, token_index, byte_from, byte_to}`
- `SegmentReader` charge les `.sfxpost` depuis le manifest (même manifest que `.sfx`)
- `segment_serializer.write_sfxpost(field_id, data)` écrit le fichier
- Commits : `705f5b9`, `f434e89`, `31e0561`, `8fea1b5`

### PostingResolver (module partagé)
- **Fichier** : `src/query/posting_resolver.rs`
- Trait `PostingResolver` : `resolve(ordinal) → Vec<PostingEntry>`, `doc_freq(ordinal) → u32`
- `SfxPostResolver` : pré-charge tout le .sfxpost en mémoire, O(1) lookup
- `InvertedIndexResolver` : fallback, lit le ._raw inverted index
- `build_resolver(reader, field)` : construit le bon resolver (.sfxpost préféré)
- `PostingEntry` : `{doc_id, position, byte_from, byte_to}`
- Commit : `09871e1`

### ResolvedPostings (adaptateur)
- **Fichier** : `src/query/resolved_postings.rs`
- Implémente `DocSet + Postings` à partir de `Vec<PostingEntry>`
- Groupement par doc_id, binary search `seek()`, O(1) `advance()`
- `term_freq()`, `append_positions_with_offset()`, `append_offsets()`, `append_positions_and_offsets()`
- 7 tests unitaires
- Commit : `bbcb225`

### Rerouting des queries
- **SuffixContainsQuery** (`suffix_contains_query.rs`) : utilise `build_resolver()`, fallback inverted_index
- **RegexContinuationQuery** (`regex_continuation_query.rs`) : utilise `build_resolver()` via trait, `continuation_score` prend `&dyn PostingResolver`
- `ContinuationMatch` n'a plus de `term_info` — juste `raw_ordinal`
- Commits : `6b4d639`, `bba1485`, `518a6c8`

### SfxTermDictionary ordinal methods
- `search_automaton_ordinals()` → `Vec<(String, u64)>`
- `get_ordinal()` → `Option<u64>`
- `range_scan_ordinals()` → `Vec<(String, u64)>`
- Commit : `b2b8abc`

## Ce qui reste à faire

### Phase 2 : AutomatonWeight → PostingResolver (PROCHAINE ÉTAPE)

**Fichier** : `src/query/automaton_weight.rs`

Le scorer a 4 branches (highlight+scoring, highlight only, scoring only, fast path). Toutes itèrent sur `Vec<TermInfo>` → `read_postings_from_terminfo`.

Plan :
1. Ajouter `collect_ordinals()` (déjà écrit, utilise `search_automaton_ordinals`)
2. Ajouter `scorer_from_ordinals()` dans le bloc `impl<A> AutomatonWeight<A>` (PAS dans `impl Weight`)
3. `scorer_from_ordinals` utilise `ResolvedPostings` pour les 4 branches
4. Dans `impl Weight`, le `scorer()` tente d'abord `collect_ordinals` → `scorer_from_ordinals`, sinon fallback `collect_term_infos` → ancien path
5. BM25 : `doc_freq` depuis `resolver.doc_freq(ord)`, `avg_fieldnorm` calculé depuis fieldnorms

**Attention** : les méthodes `scorer_from_ordinals` et `scorer_from_term_infos` doivent être dans `impl<A> AutomatonWeight<A>`, pas dans `impl Weight for AutomatonWeight<A>` — sinon erreur "not a member of trait".

**Estimation** : ~120 lignes (le code est déjà écrit dans le stash, juste à structurer)

### Phase 3 : TermWeight → PostingResolver

**Fichier** : `src/query/term_query/term_weight.rs`

- `scorer()` : `SfxTermDictionary.get_ordinal()` → `resolver.resolve(ord)` → `ResolvedPostings` → scorer
- Fallback sur ancien path

### Phase 4 : AutomatonPhraseWeight → PostingResolver

**Fichier** : `src/query/phrase_query/automaton_phrase_weight.rs`

- `cascade_term_infos()` → `cascade_ordinals()` → `ResolvedPostings` pour chaque position
- `prefix_term_infos()` → `prefix_ordinals()`
- PhraseScorer / ContainsScorer reçoivent `ResolvedPostings` au lieu de `SegmentPostings`

### Phase 5 : BM25 weight() sans ._raw

- Déplacer construction BM25Weight de `weight()` vers `scorer()` (per-segment)
- `doc_freq` depuis `.sfxpost`, `avg_fieldnorm` depuis fieldnorms

### Phase 6 : Merger .sfxpost

- Collecter tokens depuis `.sfx` source (SI=0 stream) au lieu de `inverted_index.terms()`
- Reconstruire `.sfxpost` avec remapping doc_ids et ordinals

### Phase 7 : Supprimer ._raw

- `handle.rs` : ne plus créer ._raw
- Supprimer `RAW_SUFFIX`, `raw_field_pairs`
- Bindings : ne plus auto-dupliquer
- Virer tous les fallback InvertedIndexResolver

## Fichiers clés à connaître

```
src/query/posting_resolver.rs              ← PostingResolver trait + impls + build_resolver
src/query/resolved_postings.rs             ← ResolvedPostings (PostingEntry → Postings+DocSet)
src/query/automaton_weight.rs              ← À REFACTORER (Phase 2)
src/query/term_query/term_weight.rs        ← À REFACTORER (Phase 3)
src/query/phrase_query/automaton_phrase_weight.rs ← À REFACTORER (Phase 4)
src/query/phrase_query/suffix_contains_query.rs   ← DÉJÀ REROUTÉ
src/query/phrase_query/regex_continuation_query.rs ← DÉJÀ REROUTÉ
src/suffix_fst/term_dictionary.rs          ← SfxTermDictionary (ordinal methods ajoutées)
src/suffix_fst/collector.rs                ← SfxCollector (build → sfx + sfxpost)
src/suffix_fst/file.rs                     ← SfxPostingsReader, SfxPostingEntry
src/index/segment_reader.rs                ← charge .sfx + .sfxpost
src/indexer/segment_serializer.rs          ← write_sfxpost()
src/indexer/merger.rs                      ← merge_sfx() (TODO: .sfxpost)
lucivy_core/src/query.rs                   ← routing contains/regex/boolean
lucivy_core/src/handle.rs                  ← schema (._raw encore là, ._ngram supprimé)
```

## Docs de référence

```
docs/16-mars-2026-14h41/01-progression-option-C.md   ← état des commits
docs/16-mars-2026-14h41/02-design-suppression-raw.md  ← design Option C
docs/16-mars-2026-14h41/03-phases-refactoring-query-weights.md ← plan 7 phases
docs/15-mars-2026-19h00/03-plan-U5-raw-fst-removal.md ← analyse Options B vs C
docs/15-mars-2026-19h00/04-etude-option-C-suppression-raw.md ← faisabilité
```

## Conventions

- Ne pas mentionner Claude dans les commits
- Docs en français, code et commentaires en anglais
- Chaque phase = un commit indépendant avec fallback
- Tester après chaque modification : `cargo test --lib -p ld-lucivy`
