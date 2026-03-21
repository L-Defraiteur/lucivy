# Doc 14 — Plan de cleanup : warnings, code mort, ngrams

Date : 20 mars 2026
Branche : `feature/luciole-dag`

## Inventaire complet

### A. Fichiers entiers à supprimer (~1065 lignes)

| Fichier | Lignes | Raison |
|---------|--------|--------|
| `src/tokenizer/ngram_tokenizer.rs` | 491 | Plus utilisé — contains search passe par SFX |
| `src/tokenizer/stemmer.rs` | 201 | Feature-gated mais inutile pour code search, stemming supprimé |
| `src/query/fuzzy_substring_automaton.rs` | 218 | Marqué `#[allow(dead_code)]`, zéro caller |
| `src/query/phrase_query/substring_automaton.rs` | 156 | Jamais appelé hors tests internes, gardé "pour le futur" |

Actions :
1. Supprimer les 4 fichiers
2. Retirer `mod ngram_tokenizer` de `src/tokenizer/mod.rs:129`
3. Retirer `mod stemmer` de `src/tokenizer/mod.rs:143`
4. Retirer `mod fuzzy_substring_automaton` de `src/query/mod.rs:15`
5. Retirer `pub mod substring_automaton` de `src/query/phrase_query/mod.rs:11`
6. Retirer `rust-stemmers` de `Cargo.toml` + feature `stemmer`
7. Retirer les refs stemmer dans les bindings (`bindings/*/Cargo.toml` et `src/lib.rs`)
8. Nettoyer `examples/custom_tokenizer.rs` (utilise NgramTokenizer)

### B. Fonctions / structs mortes à supprimer (~200 lignes)

| Localisation | Item | Lignes |
|---|---|---|
| `src/query/phrase_query/scoring_utils.rs` | `generate_trigrams()`, `fold_with_byte_map()`, `ngram_threshold()`, `intersect_sorted_vecs()` + leurs tests | ~180 |
| `src/query/phrase_query/suffix_contains.rs:419` | `make_raw_resolver()` | ~30 |
| `src/query/phrase_query/suffix_contains.rs:23` | champs `token_index`, `parent_term` dans struct (never read) | 2 |
| `src/query/phrase_query/suffix_contains.rs:726` | champ `token_matches` (never read) | 1 |
| `src/suffix_fst/term_dictionary.rs:140` | `ContinuationAutomaton` (legacy, remplacé par PrefixByte) | ~28 |
| `src/indexer/segment_writer.rs:24` | `collector_token_count()` | ~15 |
| `src/indexer/segment_manager.rs:224` | `all_segment_metas()` (jamais appelé) | ~5 |

### C. Imports inutilisés (~15 lignes)

| Fichier | Import |
|---|---|
| `luciole/src/graph_node.rs:3` | `PortValue` |
| `luciole/src/node.rs:1` | `TypeId` |
| `luciole/src/pool.rs:6` | `Mailbox` |
| `src/indexer/commit_dag.rs:432` | `super::*` (dans un test) |
| `src/suffix_fst/collector.rs:294` | `is_value_boundary` |
| `src/suffix_fst/term_dictionary.rs:453` | `SfxCollector` |
| `src/suffix_fst/term_dictionary.rs:565` | `TermStreamer` |
| `src/suffix_fst/stress_tests.rs:8-10` | `ParentEntry`, `GapMapReader`, `is_value_boundary` |
| `src/index/segment_reader.rs:9` | `Directory` |
| `src/query/phrase_query/suffix_contains_query.rs:20` | `Tokenizer` |

### D. Variables inutilisées / mutabilité superflue (~6 lignes)

| Fichier | Variable |
|---|---|
| `src/indexer/commit_dag.rs:453` | `seg` |
| `src/indexer/segment_writer.rs:181` | `field_ids` |
| `src/query/phrase_query/suffix_contains_query.rs:201` | `mut` inutile |
| `src/space_usage/mod.rs:130` | `SuffixFst` (+ snake_case) |
| `src/suffix_fst/builder.rs:330` | `output_table_data` |
| `src/suffix_fst/gapmap.rs:514` | `expected_gaps` |

### E. Luciole — dead code mineur (~15 lignes)

| Fichier | Item |
|---|---|
| `luciole/src/node.rs:198` | `with_services()` — jamais appelé |
| `luciole/src/scheduler.rs:234` | `name()` — jamais appelé |

### F. Documentation manquante (50 warnings `missing documentation`)

Principalement dans `src/diag.rs` (DiagEvent, DiagFilter variants).
Options :
- Ajouter les docs (propre)
- Ou `#[allow(missing_docs)]` sur le module diag (pragmatique — API interne)

### G. Dépendances Cargo à nettoyer

- `rust-stemmers` dans `Cargo.toml` (feature `stemmer`)
- Vérifier si `bindings/*/Cargo.toml` ont `features = ["stemmer"]`

### H. Fichiers Cargo.toml — bench targets en doublon

```
warning: file bench_sharding.rs found to be present in multiple build targets
warning: file bench_contains.rs found to be present in multiple build targets
```
→ Nettoyer `lucivy_core/Cargo.toml` pour avoir chaque bench dans un seul target

## Ordre d'exécution

**Phase 1 — Modules entiers** (A)
Supprimer ngram_tokenizer, stemmer, fuzzy_substring_automaton, substring_automaton.
Nettoyer les `mod` declarations, Cargo.toml, examples.
~1065 lignes supprimées.

**Phase 2 — Fonctions mortes** (B)
Supprimer generate_trigrams, fold_with_byte_map, ngram_threshold, intersect_sorted_vecs, make_raw_resolver, ContinuationAutomaton, collector_token_count, all_segment_metas, champs morts.
~260 lignes supprimées.

**Phase 3 — Imports, variables, mutabilité** (C + D + E)
Fixes rapides, une par une.

**Phase 4 — Documentation DiagBus** (F)
`#[allow(missing_docs)]` sur `diag.rs` (API interne, pas besoin de doc publique).

**Phase 5 — Cargo cleanup** (G + H)
Retirer rust-stemmers, fixer bench targets.

**Phase 6 — Vérification finale**
`cargo test --lib` → 0 warnings, 1200 tests pass.

## Estimation

~1350 lignes supprimées, ~30 lignes modifiées. Zéro changement de comportement.
