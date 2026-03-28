# 11 — Rapport de session : regex contains via literal_resolve + PosMap + ByteBitmap

## Date : 28 mars 2026

## Résumé

Session dédiée à l'implémentation du regex contains performant. Partis de 16 secondes pour `rag3.*ver`, arrivés à <70ms pour tous les patterns testés.

## Changements implémentés

### 1. `continuation_score_sibling` (début de session)
- Remplacement de Walk 2 (DFA × SFX FST) par sibling links + GapMap
- Fonctionne mais Phase 3c (gap>0) reste lente avec `.*` (DFA ne prune jamais)

### 2. `regex_contains_via_literal` v1
- Extraction du littéral préfixe du pattern regex
- `prefix_walk(literal)` → DFA validation → resolve-last
- Résultat : `shard[a-z]+` rapide, mais patterns non-prefix (`.*weaver`) lents

### 3. Multi-literal intersection
- `extract_all_literals(pattern)` → tous les fragments littéraux
- Intersection par `has_doc` O(log n) + position ordering par byte offsets
- `doc_freq` pour choisir le littéral le plus sélectif
- Résultat : `rag.*ver` passe de 16s à 192ms

### 4. PosMap + ByteBitmap (nouveaux fichiers d'index)
- `.posmap` : (doc_id, position) → ordinal, O(1) lookup
- `.bytemap` : 256-bit byte presence par ordinal, O(1) pre-filter
- `SfxBuildOutput` struct pour abstraction des fichiers de sortie
- `SegmentComponent` enum : PosMap + ByteMap enregistrés (fix GC)
- Résultat : PosMap walk O(distance) remplace Phase 3c O(64 × siblings)

### 5. `literal_resolve.rs` — briques réutilisables (fin de session)
- `find_literal()` : résout un littéral via exact contains (cross-token aware via sibling chain)
- `intersect_literals_ordered()` : intersection multi-littérale avec position ordering
- `validate_path()` : DFA validation entre deux positions via PosMap, early return on match
- Réécriture complète de `regex_contains_via_literal` :
  - Single-literal : find + DFA validate + PosMap cross-token
  - Multi-literal : find each + intersect + PosMap DFA walk
  - **Plus aucun fallback vers scan FST** (continuation_score_sibling)
  - ~600 lignes de vieux code Phase 1-3c supprimées

## Performances finales (WASM, 872 docs)

| Pattern | Avant session | Après session |
|---|---|---|
| `shard[a-z]+` | 538ms | **45ms** |
| `rag3.*ver` | 26 773ms | **<70ms** |
| `incremental.sync` | 1 217ms | **<70ms** |
| `flow.control` | 16ms | **24ms** |
| `.*weaver` | 16 000ms+ (bloqué) | **<70ms** |
| `.*getElementById` | bloqué | **<70ms** |
| `blob.irectory` | 83ms | **<70ms** |

## Architecture finale regex

```
regex_contains_via_literal(pattern):

  1. extract_all_literals(pattern) → ["incremental", "sync"]

  2. Pour chaque littéral :
     find_literal(sfx_reader, literal, resolver, ord_to_term)
       → suffix_contains_single_token_with_terms (réutilise contains exact)
       → cross-token via falling_walk + sibling chain
       → Vec<LiteralMatch { doc_id, position, byte_from, byte_to }>

  3. Si single-literal :
     Feed literal bytes au DFA → is_match ? → single-token match
     Si DFA alive pas accepting → PosMap walk cross-token (validate_path)

  4. Si multi-literal :
     intersect_literals_ordered → docs avec tous les littéraux dans l'ordre
     PosMap walk DFA entre first_pos et last_pos → validate_path
     Early return on DFA accept

  JAMAIS de scan FST. Pas de fallback. 0 results si pas de littéral viable.
```

## Fichiers modifiés/créés

### Nouveaux fichiers
- `src/query/phrase_query/literal_resolve.rs` — briques réutilisables
- `src/suffix_fst/posmap.rs` — PosMapWriter/Reader
- `src/suffix_fst/bytemap.rs` — ByteBitmapWriter/Reader
- `docs/arsenal.md` — inventaire de toutes les structures d'indexation
- `docs/27-mars-2026-13h34/07-09-10` — docs de design
- `playground/test_regex_bench.mjs` — benchmark Playwright
- `playground/test_regex_perf.mjs` — benchmark Node.js

### Fichiers modifiés
- `src/query/phrase_query/regex_continuation_query.rs` — réécriture complète du flow regex
- `src/suffix_fst/collector.rs` — SfxBuildOutput, collecte posmap + bytemap
- `src/suffix_fst/mod.rs` — exports posmap, bytemap
- `src/index/segment_component.rs` — PosMap + ByteMap dans l'enum (fix GC)
- `src/index/segment_reader.rs` — chargement .posmap + .bytemap
- `src/indexer/segment_writer.rs` — écriture .posmap + .bytemap
- `src/indexer/segment_serializer.rs` — write_posmap, write_bytemap

## Commits (branche `feature/regex-contains-literal`)

1. `f9c5103` — feat: regex contains via literal extraction + multi-literal intersection
2. `8397898` — feat: add PosMap + ByteBitmap index files, SfxBuildOutput abstraction
3. `3132a34` — feat: wire PosMap into regex search — O(distance) cross-token validation
4. `9e7bb9e` — fix: register PosMap + ByteMap in SegmentComponent enum
5. `4ef42fc` — feat: rewrite regex via literal_resolve — reuse exact contains logic

## Tests : 1181 passent

## Points à investiguer (prochaine étape)

### Compatibilité à vérifier
- **Merge de segments** : les .posmap et .bytemap doivent être reconstruits pendant le merge
- **Snapshots incrémentaux (lucid/lucids)** : les nouveaux fichiers doivent être inclus dans les deltas
- **Sharding** : les PosMap/ByteBitmap sont par-segment, pas d'impact cross-shard
- **Sharding distribué** : idem, chaque nœud a ses propres segments

### ByteBitmap pas encore câblé à la recherche
Le ByteBitmap est indexé et stocké mais pas utilisé par le regex pour l'instant. Sera un pré-filtre futur.

### `[a-z]+ment` — pas de littéral viable
Retourne 0 résultats immédiatement (pas de littéral ≥ 3 chars). Correct mais limitant. Piste : extraire des bigrams ou réduire MIN_LITERAL_LEN à 2.
