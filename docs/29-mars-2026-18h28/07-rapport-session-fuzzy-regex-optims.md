# 07 — Rapport session : fuzzy highlights, registry, optims regex/fuzzy

Date : 29-31 mars 2026
Branche : `feature/fuzzy-via-literal-resolve`

## Résumé

Session de 3 jours couvrant :
1. Fix des highlights fuzzy (40 failures → 0)
2. Fix du merger pour écrire tous les registry files
3. Suppression des fallbacks silencieux
4. Optimisations perf fuzzy et regex
5. Nouveaux modules : SepMap, ByteMap DFA filter, literal pipeline, regex gap analyzer
6. Fix multi-token fuzzy

## Ce qui a été fait

### 1. Fix highlights fuzzy — byte-exact mapping

**Problème** : les highlights fuzzy utilisaient un walk token-entier (fspan→lspan)
au lieu des bytes exacts du match DFA. 40/286 highlights invalides.

**Fix** : table `content_byte_starts` construite via posmap+gapmap walk,
mapping direct `match_start`/`match_end` du concat vers les content bytes
via intra-token offset.

**Fix ancre** : `first_bf` est le byte_from du suffix match (token_start + si),
pas du token. Propagation du `si` à travers LiteralMatch, MatchesByDoc,
intersect_trigrams_with_threshold pour calculer `token_start = first_bf - si`.

Résultat : 296/296 + 260/260 highlights valides.

### 2. Fix merger — registry files

**Problème** : les 3 chemins de merge (merger.rs N-way, merger.rs fallback,
sfx_dag.rs WriteSfxNode) n'écrivaient PAS les registry files (posmap, termtexts,
bytemap, gapmap, sibling). Seuls .sfx et .sfxpost étaient écrits.

**Conséquence** : le WASM (qui merge automatiquement) n'avait ni posmap ni
termtexts → fuzzy search crashait avec "PosMap not found".

**Fix** : tous les chemins utilisent maintenant `write_custom_index()`.
sfx_dag.rs reconstruit posmap/bytemap/termtexts depuis le sfxpost sérialisé.

### 3. Suppression des fallbacks silencieux

5 fallbacks `ord_to_term` sur le term dict tantivy (ordinal mismatch) remplacés
par des erreurs explicites. 1 fallback posmap ("accept conservatively") remplacé
par une erreur.

### 4. Optimisations fuzzy

| Optim | Impact | Commit |
|---|---|---|
| Anchored sliding window | ~90% réduction DFA transitions | 97c0bdc |
| Trigram proven skip | 100% skip DFA quand tous trigrams matchent | d510423 |
| Pipeline sélectivité | Trigrams par rareté + filtrage progressif | dad6d3c |
| Early termination d=0 | Break dès match parfait | 97c0bdc |
| Skip DFA d>=3 | Évite construction DFA Levenshtein d=3 | b32aa6e |

### 5. Optimisations regex

| Optim | Impact | Commit |
|---|---|---|
| ByteMap DFA pré-filtre | Skip tokens incompatibles dans validate_path | 718b363 |
| `.*` fast path | Skip validate_path quand gap est `.*` | 4e550b2 |
| Gap-by-gap validation | Valide chaque gap individuellement | 4e550b2 |
| regex-syntax AST | Parse gaps en AcceptAnything/ByteRangeCheck/DfaValidation | aa345ba |
| `\w`/`\d` as ByteRangeCheck | Clamp Unicode ranges to ASCII | d134d94 |
| Fallback DFA pour tokens adjacents | ByteRangeCheck + DFA quand pas de tokens intermédiaires | f921e08 |
| Sélectivité littéraux regex | Pipeline même que fuzzy | 4e550b2 |

### 6. Nouveaux index files

- **SepMap** (`.sepmap`) : bitmap 256 bits des bytes de séparateur après chaque
  token ordinal. Bit 0x00 = contiguous flag. Permet de vérifier les séparateurs
  sans lire le GapMap per-doc.

### 7. Nouveaux modules

- `dfa_byte_filter.rs` : `can_token_advance_dfa()` — pre-filter DFA via ByteMap
- `literal_pipeline.rs` : 4 briques composables (fst_candidates, resolve_candidates,
  cross_token_falling_walk, resolve_chains)
- `regex_gap_analyzer.rs` : parse regex via regex-syntax HIR, classify gaps,
  validate_gap_bytemap avec SepMap
- `sepmap.rs` : SepMapWriter/Reader + SepMapIndex

### 8. Fix multi-token fuzzy

- `generate_ngrams` skip les n-grams qui chevauchent des séparateurs
- Dedup des entries dans `intersect_trigrams_with_threshold` (doublons
  cross-token + single-token cassaient le greedy chain builder)

## Bugs connus

### Multi-token d=0 (exact contains) cassé sur .luce

`suffix_contains_multi_token_impl_pub` retourne 0 résultats pour
"use rag3weaver" d=0 sur le .luce, alors que "rag3weaver" seul fonctionne.
Bug pré-existant, pas lié à cette session.

### Ground truth test — faux miss de fichiers dupliqués

Le mapping content hash → file index ne gère pas les fichiers avec le même
contenu. Fix : filter les misses dont le hash mappe vers un autre index.

## Timings regex (natif, ~900 docs)

| Pattern | Temps | Type |
|---|---|---|
| `rag3.*ver` | ~20ms | .* fast path |
| `rag3.*weaver` | ~18ms | .* fast path |
| `rag3[a-z]+ver` | ~24ms | ByteRangeCheck + SepMap |
| `impl.*fn.*self` | ~88ms | multiple .* gaps |
| `use.*crate` | ~38ms | .* common words |
| `pub.*struct` | ~40ms | .* common Rust |
