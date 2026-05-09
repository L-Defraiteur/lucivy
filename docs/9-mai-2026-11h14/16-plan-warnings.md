# Plan cleanup warnings — compilation + clippy

## Etat actuel

- **Compilation** : 134 warnings (fichier `13-compilation-warnings.txt`)
- **Clippy** : 4 erreurs + 273 warnings (fichier `14-clippy-warnings.txt`)

## Phase 1 : Erreurs clippy (bloquant clippy CI)

4 erreurs `never_loop` dans `src/suffix_fst/term_dictionary.rs` (lignes 217, 250, 271, 313).

Pattern : des `loop { ... break; }` qui ne bouclent jamais — probablement du code
copié d'un pattern `loop` qui a été simplifié mais jamais nettoyé.

**Action :** remplacer les `loop { ... break result; }` par des blocs `{ ... result }`.

**Effort :** 5 min.

## Phase 2 : Unused imports/variables (rapide, cargo fix)

| Lint | Nombre | Action |
|------|--------|--------|
| `unused_imports` | ~10 | `cargo fix --lib` (automatique) |
| `unused_variables` | ~17 | `cargo fix --lib` (préfixe `_`) |
| `unused_mut` | 1 | `cargo fix --lib` |

**Action :** `cargo fix --lib -p ld-lucivy --allow-dirty && cargo fix --lib -p luciole --allow-dirty`

**Effort :** 2 min.

## Phase 3 : Dead code (review nécessaire)

| Item | Fichier | Action |
|------|---------|--------|
| `new_state` | lucivy-fst/levenshtein.rs | Supprimer ou `#[allow]` |
| `finalizer_ref` field | indexer_actor.rs | Supprimer si inutilisé |
| `merge_sibling_links` | sfx_merge.rs | Supprimer |
| `decode_vint` | sfx_merge.rs | Supprimer |
| `ctx` field WriteSfxNode | sfx_dag.rs | Supprimer si inutilisé |
| `get_mergeable_segments` (×3) | segment_manager/register/updater | Supprimer (remplacé par DAG) |
| `run_deferred_merges`, `collect_merge_candidates` | segment_updater_actor.rs | Supprimer |
| `extract_all_literals` | regex_continuation_query.rs | Supprimer (remplacé par analyze_regex) |
| `pick_best_literal` | regex_continuation_query.rs | Supprimer |
| `cross_token_resolve_for_multi` | suffix_contains.rs | Supprimer si inutilisé |
| `segments` field SfxCache | suffix_contains_query.rs | Supprimer si inutilisé |
| `byte_in_ranges` | regex_gap_analyzer.rs | Supprimer |
| `write_sfxpost/posmap/bytemap` | segment_serializer.rs | Supprimer si inutilisés |

**Action :** review chaque item, supprimer le dead code.

**Effort :** 20 min.

## Phase 4 : Clippy warnings (le gros morceau)

### Auto-fixable (cargo clippy --fix)

| Lint | Nombre | Description |
|------|--------|-------------|
| `uninlined_format_args` | ~91 | `format!("{}", x)` → `format!("{x}")` |
| `redundant_closure` | ~20 | `\|x\| foo(x)` → `foo` |
| `len_zero` | ~14 | `.len() == 0` → `.is_empty()` |
| `needless_borrow` | ~11 | `&x` quand pas nécessaire |
| `unnecessary_map_or` | ~8 | `.map_or(false, ...)` → `.is_some_and(...)` |
| `clone_on_copy` | ~3 | `.clone()` sur type Copy |
| `borrow_deref_ref` | ~3 | `&*x` → `x` |
| `unused_unit` | ~4 | `() =>` inutile |
| `let_and_return` | ~4 | `let x = ...; x` → direct |
| `collapsible_if/else_if` | ~7 | `if { if }` → `if && ` |

**Action :** `cargo clippy --fix --lib -p ld-lucivy --allow-dirty`

**Effort :** 5 min (automatique) + review.

### Manuels (allow ou refactor)

| Lint | Nombre | Action recommandée |
|------|--------|--------------------|
| `type_complexity` | ~36 | `#[allow(clippy::type_complexity)]` sur les fonctions concernées |
| `new_without_default` | ~22 | Ajouter `impl Default` ou `#[allow]` |
| `too_many_arguments` | ~12 | `#[allow(clippy::too_many_arguments)]` (refactor serait trop invasif) |
| `items_after_test_module` | ~5 | Déplacer les items avant `#[cfg(test)] mod tests` |
| `should_implement_trait` | ~3 | `#[allow]` (faux positifs sur `fn new()`) |
| `len_without_is_empty` | ~3 | Ajouter `is_empty()` |
| `doc_lazy_continuation` | ~4 | Fixer la doc markdown |
| `never_loop` | 4 | Phase 1 (erreurs) |

**Effort :** 30 min.

## Phase 5 : Missing docs

~70 warnings `missing_docs` dans les modules SFX (`suffix_fst/`, `query/phrase_query/`).

**Action :** ajouter `#![allow(missing_docs)]` en haut des fichiers SFX concernés.
Ce sont des modules internes, pas de l'API publique.

Fichiers concernés :
- `src/suffix_fst/gapmap.rs`
- `src/suffix_fst/posmap.rs`
- `src/suffix_fst/bytemap.rs`
- `src/suffix_fst/sepmap.rs`
- `src/suffix_fst/termtexts.rs`
- `src/suffix_fst/freqmap.rs`
- `src/suffix_fst/sibling_table.rs`
- `src/suffix_fst/sfxpost_v2.rs`
- `src/suffix_fst/index_registry.rs`
- `src/query/phrase_query/suffix_contains.rs`
- `src/query/phrase_query/suffix_contains_query.rs`
- `src/query/phrase_query/regex_continuation_query.rs`
- `src/query/phrase_query/literal_resolve.rs`
- `src/query/phrase_query/literal_pipeline.rs`
- `src/query/phrase_query/regex_gap_analyzer.rs`
- `src/query/posting_resolver.rs`
- `src/indexer/index_writer.rs`

**Effort :** 5 min.

## Phase 6 : BranchNode snake_case

1 warning `non_snake_case` pour `pub fn BranchNode`. C'est voulu (API design).

**Action :** `#[allow(non_snake_case)]` sur la fonction.

**Effort :** 1 min.

## Ordre recommandé

```
Phase 1 → Phase 2 → Phase 5 → Phase 6 → Phase 3 → Phase 4 (auto) → Phase 4 (manual)
```

Estimation totale : **1h-1h30**

## Objectif

Zero warnings compilation + clippy clean → réactiver clippy en CI.
