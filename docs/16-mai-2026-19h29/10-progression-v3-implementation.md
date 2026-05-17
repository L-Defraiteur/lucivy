# Progression implémentation v3

**Branche** : `feature/sfx-v3-overlap-tokenizer`  
**Démarré** : 17 mai 2026

---

## Fait

### Phase 1 — Tokenizer
- [x] `src/tokenizer/equal_chunk.rs` — EqualChunkTokenizer (20 tests)
  - Division égale du segment (mot + trailing sep)
  - MAX_TOKEN=8, respect UTF-8, pas d'orphelins
  - ChunkMeta: content_len, sep_len, is_word_start, word_id
- [x] Export dans `src/tokenizer/mod.rs`

### Phase 2 — SFX Builder
- [x] `src/suffix_fst/builder_v3.rs` — SuffixFstBuilderV3 (10 tests)
  - Encoding u64 v3: is_word_start, overlap_len, sep_len, own_len, sti, ordinal
  - Lowercase interne (comme v2)
  - ParentEntryV3 + OutputTable v3 (11 bytes/entry)
  - encode/decode round-trip vérifié

### Phase 2b — Collector
- [x] `src/suffix_fst/collector_v3.rs` — SfxCollectorV3 (12 tests)
  - Tokenize + overlap (2 bytes du token suivant)
  - Intern les extended tokens ("mutex_lo" pas "mutex_")
  - Pas de gapmap, sepmap, sibling_pairs
  - into_data() produit SfxCollectorDataV3
  - Test E2E: collector → builder → FST avec trigram cross-boundary "x_l" ✓

---

## Reste à faire — Indexation

### Phase 3 — TermTexts v3
- [ ] Format: stocker extended text + metadata par ordinal
  - Champs à persister: own_len (u16), sep_len (u8), overlap_len (u8), is_word_start (bool)
  - Nécessaire pour le merge (relire les tokens sans retokenizer)
- [ ] Nouveau fichier ou extension du format existant dans `src/suffix_fst/termtexts.rs`
- [ ] Read + write + tests round-trip

### Phase 4 — Format fichier .sfx v3
- [ ] Nouveau header (version=3, plus de gapmap_offset, plus de sibling table offset)
- [ ] Sections: FST + parent lists + word_map + next_word table
- [ ] SfxFileWriter v3 dans `src/suffix_fst/file.rs` (ou `file_v3.rs`)
- [ ] SfxFileReader v3 — decode_parents_v3, pas de sibling_table(), pas de gapmap()
- [ ] Word map sérialisation: token_to_word[TI]→WI, word_start_token[WI]→TI, next_word[TI]→TI
- [ ] Tests: write → read round-trip

### Phase 5 — DAG d'indexation v3
- [ ] Nouveau DAG node ou adaptation de `src/indexer/sfx_dag.rs` pour v3
  - Utiliser SfxCollectorDataV3 au lieu de SfxCollectorData
  - Utiliser SuffixFstBuilderV3 au lieu de SuffixFstBuilder
  - Écrire .sfx v3 (pas de .gapmap, .sepmap)
  - Écrire .termtexts v3 (avec metadata)
- [ ] Garder le DAG v2 fonctionnel (flag sfx_version)
- [ ] Tests: indexer un mini dataset → vérifier les fichiers produits

### Phase 6 — Merge v3
- [ ] Merge node v3: lire termtexts v3 des segments source → re-feed au builder v3
  - Lire extended texts + metadata des 2 segments
  - Remap ordinals (union des term sets)
  - Merger posting lists avec remapping
  - Construire nouveau FST + OutputTable via SuffixFstBuilderV3
- [ ] Plus de merge de sibling/gapmap/sepmap (structures supprimées)
- [ ] Word map reconstruit pendant le merge
- [ ] Tests: 2 segments → merge → vérifier résultat

### Phase 7 — SfxPost v3
- [ ] Vérifier que le format sfxpost est compatible (probablement inchangé)
- [ ] Les ordinals sont les extended tokens — les posting lists restent (doc_id, ti, byte_from, byte_to)
- [ ] FreqMap, PosMap, ByteMap: vérifier compatibilité (probablement inchangés, EventDriven)

---

## Reste à faire — Queries (après indexation)

### Phase 8 — Falling walk v3
- [ ] falling_walk: split à `STI + consumed == own_len`, continue dans overlap
- [ ] Sep-skip pour strict_separators=false
- [ ] Fuzzy falling walk: `STI + fst_depth == own_len`
- [ ] Cross-token par falling walk chaîné (boucle, pas sibling DFS)

### Phase 9 — Fuzzy contains v3
- [ ] Supprimer concat_query, boundary_positions, boundary_trigram_indices
- [ ] Threshold: `max(T - n*d, 1)`
- [ ] Résolution single-token uniquement (plus de cross_token_falling_walk pour les trigrams)

### Phase 10 — Exact contains v3
- [ ] Remplacer sibling DFS par falling walk chaîné
- [ ] Multi-token: inchangé structurellement

### Phase 11 — Regex continuation v3
- [ ] Supprimer continuation_score_sibling
- [ ] Utiliser continuation_score seul
- [ ] Supprimer appels au gapmap

### Phase 12 — Refactor briques communes
- [ ] Unifier les briques falling walk entre contains, fuzzy, et regex
- [ ] Garder fuzzy falling walk pour tokenization custom

---

## Fichiers créés/modifiés

| Fichier | Statut | Lignes |
|---------|:------:|--------|
| `src/tokenizer/equal_chunk.rs` | ✅ nouveau | ~330 |
| `src/tokenizer/mod.rs` | ✅ modifié | +2 lignes |
| `src/suffix_fst/builder_v3.rs` | ✅ nouveau | ~400 |
| `src/suffix_fst/collector_v3.rs` | ✅ nouveau | ~350 |
| `src/suffix_fst/mod.rs` | ✅ modifié | +3 lignes |
| `src/suffix_fst/termtexts.rs` | ⬜ à modifier | format v3 |
| `src/suffix_fst/file.rs` ou `file_v3.rs` | ⬜ à créer/modifier | writer+reader v3 |
| `src/indexer/sfx_dag.rs` | ⬜ à modifier | DAG v3 |

## Tests

| Module | Tests | Statut |
|--------|:-----:|:------:|
| equal_chunk | 20 | ✅ |
| builder_v3 | 10 | ✅ |
| collector_v3 | 12 | ✅ |
| termtexts v3 | ? | ⬜ |
| file v3 | ? | ⬜ |
| sfx_dag v3 | ? | ⬜ |
| merge v3 | ? | ⬜ |
