# Knowledge Dump — Session 4 Complete (17-18 mai 2026)

## SFX v3 Architecture (état actuel)

### Ordinal system
- **Content ordinal** : ordinal final dans le sfxpost. Clé = `text[..own_len]` (content+sep, sans overlap)
- **Intern ordinal** : ordinal interne au collector. Chaque texte étendu unique (content+sep+overlap) = 1 intern
- **Mapping** : `intern_to_final[intern_ord] → content_ord`. Plusieurs intern → un content
- **Word-stripped** : exclus de content_key_map, mappés au premier chunk via `intern_to_final[ws] = intern_to_final[first_chunk]`. Pas de posting propre.

### Partitions FST
- **0x00 (SI=0)** : début de token, pour anchor_start
- **0x01 (SI>0)** : suffixes, pour contains anywhere
- **0x02 (stripped)** : content sans sep, pour strict_sep=false. Entrées word-level (mot entier, pas chunk).

### TokenChainV3
- `ordinals: Vec<Vec<u64>>` — chaque position a des ordinals alternatifs
- Le resolve fait l'union des postings par position avant adjacency check
- `last_ordinal: u64` dans MatchV3 pour la vérification structurelle
- `anchor_start=true` pour le remainder dans le chain builder

### Content-len filter
- `matches.retain(|m| m.span > 1 || byte_span >= query_content_len)`
- Appliqué seulement aux single-token (span=1)
- Les chains (span>1) passent sans filtre (vérification structurelle nécessaire)

### Word maps (construites, partiellement utilisées)
- **ChunkWordMap** : ordinal → [(word_id, chunk_index, total_chunks)]
- **NextWordMap** : word_id → [next_word_ids]
- **WordPosMap** : (doc_id, position) → word_id_within_doc
- Enregistrées dans index_registry, stockées comme registry files
- La vérification per-doc ne filtre pas les FP car intra-word = toujours valide

### Falling walk
- Byte-by-byte FST walk, détecte splits aux marker entries (own_len boundary)
- Marker entries : clés FST tronquées à own_len pour rendre le nœud final
- L'overlap est dans la clé FST (pour trigram coverage) mais pas dans l'ordinal

### Bugs trouvés et fixés
1. **TermTexts écrasé** : word-stripped text overwriting chunk text dans AssembleV3Node → fix: skip is_word_stripped
2. **Word-stripped postings dupliqués** : même (doc, pos) sous 2+ ordinals → fix: pas de posting pour word-stripped, mapping vers first chunk ordinal
3. **Dedup falling walk** : marker entries + full keys produisaient 2 splits avec overlap_validated différent → fix: sort par overlap_validated descending

### Problème non résolu : FP d'overlap mixing
Le mélange d'overlaps cause 7 FP pour "struct" strict :
- Token "CreateStr" avec overlap "uc" (de "CreateStruct") et overlap "in" (de "CreateString") partagent le même ordinal "CreateStr" (text[..own_len])
- Le falling walk matche "struct" via overlap "uc" sur la clé FST
- Le resolve prend le posting de la variante "in" dans un doc où le texte réel est "CreateString"
- L'adjacency check passe car le next chunk est adjacent

### Options explorées pour l'overlap mixing
- Extended-text ordinals : élimine le mélange mais casse le relaxed mode (word-stripped ne peut pas pointer vers un seul variant)
- Dual ordinal system : text[..own_len] pour 0x00/0x01 + extended pour 0x02 → complexe
- Post-filtre byte-exact : lire le stored field → exact mais I/O
- Word map verification : insuffisant (intra-word = toujours valide)

## Résultats ground truth actuels

### Strict (7/8 parfaits, was 3/15)
| Query | Grep | V3 | Status |
|-------|------|----|--------|
| function | 62 | 63 | 1 FP |
| return | 463 | 463 | **OK** |
| struct | 71 | 78 | 7 FP |
| void | 18 | 18 | **OK** |
| rag3db | 51 | 62 | 11 FP |
| include | 29 | 29 | **OK** |
| uint64_t | 11 | 11 | **OK** |
| std::unique_ptr | 8 | 8 | **OK** |
| ku_dynamic_cast | 0 | 0 | **OK** |
| TableFunction | 0 | 0 | **OK** |

### Relaxed (encore des FP — partition 0x02 issues)
Les FP relaxed sont nombreux et séparés du problème strict.

## Fichiers clés modifiés

### Nouveaux modules
- `src/suffix_fst/word_map.rs` — ChunkWordMap + NextWordMap + verify_chain_adjacency
- `src/suffix_fst/word_pos_map.rs` — WordPosMap per-doc
- `docs/17-mai-2026/01-chain-adjacency-verification-options.md`
- `docs/17-mai-2026/02-design-word-map-chain-verification.md`
- `docs/17-mai-2026/04-overlap-sibling-fp-problem.md`
- `docs/17-mai-2026/05-investigation-report-session-4.md`

### Modifiés
- `src/suffix_fst/collector_v3.rs` — content key, word map building, word-stripped mapping
- `src/suffix_fst/briques/fst_walk.rs` — Vec<Vec<u64>>, anchor_start, dedup sort
- `src/suffix_fst/briques/resolve.rs` — resolve_alternatives, last_ordinal, MatchV3
- `src/suffix_fst/briques/orchestrator.rs` — content_len filter span=1, debug trace
- `src/suffix_fst/briques/composite.rs` — last_ordinal in MatchV3
- `src/suffix_fst/briques/integration_tests.rs` — diag tests
- `src/suffix_fst/file_v3.rs` — resolve_suffix partition 0x02
- `src/suffix_fst/mod.rs` — word_map, word_pos_map modules
- `src/suffix_fst/index_registry.rs` — word map + word_pos_map registration
- `src/indexer/sfx_dag_v3.rs` — TermTexts skip word-stripped, word maps in registry, merge content key
- `src/query/contains_query_v3.rs` — word_pos_map verification (WIP)
- `lucivy_core/tests/test_sfx_v3_ground_truth.rs` — debug_struct_fp test

## Lib tests : 1421+ pass, 1 fail (include_vs_inclusive, connu)

## Prochaine session : priorités
1. Investiguer les 5+ ordinals au même (doc, pos) dans le raw trace
2. Résoudre l'overlap mixing (extended-text ordinals dual system ou post-filtre)
3. Fixer le relaxed mode FP (partition 0x02)
4. Fixer le test include_vs_inclusive
5. Commit propre + knowledge dump memory
