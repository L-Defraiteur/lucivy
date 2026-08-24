# Features et comment les tester / benchmarker — 24 août 2026

Le mode d'emploi détaillé des benchs est `docs/BENCHMARKS.md` (lié du README).
Ceci est la carte : quelle feature, quel test la garde, comment la mesurer.
Toujours `> /tmp/fichier.txt 2>&1`, jamais `| tail`.

## Recherche (toutes : spans/highlights exacts à l'octet, vérifiés vs disque)

| Feature | Garde (test) | Mesure |
|---|---|---|
| contains strict/relaxed, littéraux longs à séparateurs | `v3_ground_truth_contains` (15 requêtes rag3db) + `v3_ground_truth_coherence` (32, dont sw/term/fz/rx, accents, CJK, emoji/ZWJ) | panel 50k : `V3_INDEX_DIR=/tmp/v3idx_50k_nat V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 V3_COMMIT_EVERY=500 V3_QUERIES='…' cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture` |
| startsWith / term (frontières de mots) | `v3_starts_with_is_word_start`, `v3_term_is_whole_token_not_prefix` + modes `sw/sws/term/terms` du harnais | idem, `V3_QUERIES='lock:sw,ptr:term'` |
| fuzzy d=1..3 | `test_fuzzy_ground_truth`, `test_fuzzy_monotonicity`, panels | 50k : `kmallc:fz1` ~56 ms, `kmalloc:fz2` ~175 ms |
| regex (littéraux prouvés / full scan) | `regex_v2_vs_v3`, panels | 50k : `/\*[^*]*\*/:rx` ~191 ms |
| `parse` (OU par mot×champ / syntaxe booléenne → `boolean` de contains, highlights partout) | `v3_parse_is_alive_and_honest`, `v3_parse_boolean_syntax_is_composite` | — |
| repli de casse (Kelvin, İ, DÉJÀ) | `v3_case_fold_length_changes` (sans corpus) | — |
| warnings honnêtes | module `warnings` (7 tests) + `test_query_warnings` + `v3_parse_…` | smoke bindings : `bindings/{python,nodejs}/tests/smoke_warnings.*` |

## Sharding, distribué, cycle de vie

| Feature | Garde |
|---|---|
| multi-shards = 1 shard = 2 nœuds (spans) | `v3_distributed_coherence` (19 requêtes × 3 formes) |
| fuzzy/regex atteignent tous les shards | `v3_sharded_fuzzy_regex_reach_all_shards` (RAM, sans corpus) |
| filtre `allowed_ids`, delete, delta LUCIDS | `v3_sharded_filter_delete_delta` (disque, 4 shards) |
| `_node_id` estampillé, `add_document_json`, erreurs | `v3_node_id_is_stamped_automatically` |
| config stricte / réouverture tolérante | `v3_schema_config_errors_speak` |
| migration v2→v3, index mixte | `v3_migration_from_v2_index` |
| merge = fresh (spans) | `v3_merge_equals_fresh_by_spans`, `v3_merge_preserves_results` |
| policy de merge réelle | `v3_policy_merges_preserve_everything` (`V3_POLICY=1` pour les panels) |
| close/reopen sous merges | `test_handle_reopen_cycles`, `test_close_releases_lock` |

## ACID / Blob (`lucivy_core/tests/test_acid_blob_v3.rs` — le modèle à copier)

| Feature | Garde |
|---|---|
| create → close → cache effacé → reopen depuis blobs, tous modes exacts | `v3_blob_storage_create_reopen_search` |
| lazy = mêmes réponses, ouverture < moitié du store | `v3_blob_storage_lazy_open_matches_eager` |
| `drop_index` ne laisse rien (blobs + racine + répertoire fs) | `v3_drop_index_leaves_nothing` |
| **aucun appel au store après `close()`** (contrat FFI) | `v3_close_means_no_more_store_calls` |

Variante Postgres réelle : `acid_postgres.rs` (`--ignored`, service requis, v2).

## Lancer l'essentiel

```bash
cargo test --release --lib                                   # 1415, tout vert
cargo test --release -p lucivy-core                          # tout vert sauf bench_sharding t01 (réseau) / t04 (sfx:false disparu)
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth   # rag3db ~45 s, 9 tests
cargo test --release -p lucivy-core --test test_acid_blob_v3
cargo test --release -p luciole --lib                        # 166
```

Profil d'une requête : `V3_PROFILE=1` (étages contains/fuzzy, `verify_literal`,
prescan, `relaxed chunk walk: skipped/walked`) ; `V3_DEBUG_QUERY='…'` pour les
traces ; `V3_SPANS_REPORT_ONLY=1` pour diagnostiquer sans asserter ;
`LUCIVY_BLOB_DEBUG=1` pour les matérialisations lazy. Cache d'index 50k : clé
`v=9` dans `index_shape_key` — **à incrémenter à tout changement de format**.

## Chiffres de référence (kernel 50k naturel, spans exactes)

plancher 29 ms · strict `include` 40-55 ms (36 824 docs) · relaxed `uint64_t`
32-34 ms · fz1 56-109 ms · fz2 175 ms · regex 191 ms · séparateurs purs
`\t\t` 7,2 M spans. Index fusionné à 1 segment : tout se dégrade (718 ms) —
c'est la raison du plafond 10k docs/segment.
