# Récap session 5 — État complet (19-24 mai 2026)

## Changements implémentés

### 1. Extended ordinals (index-time)
Chaque texte étendu unique a son propre ordinal et ses propres postings.
Plus de groupement par `text[..own_len]`. Élimine le mélange d'overlap variants.

### 2. Chain builder best_consumed filter (query-time)
Le chain builder ne garde que les ordinals de sub_splits avec le même
`query_consumed` que le best. Élimine le mélange intra-partition d'ordinals
de consumed différents. **19 FP strict éliminés.**

### 3. WordSfxPost (index-time)
Format de postings dédié pour partition 0x02 (word-stripped).
- `first_position` : premier chunk du mot (pour ordering/dedup)
- `last_position` : dernier chunk du mot (pour cross-word adjacency)
- `byte_from` : début du mot (pour highlights)
- `byte_to` : fin du mot (pour highlights)

Construit par jointure first/last chunk postings par doc_id dans `into_data()`.
Stocké comme registry file "word_sfxpost".

### 4. Partition-separated chains (query-time)
Deux pipelines séparés :
- **Chunk pipeline** (0x00/0x01) : `falling_walk_chunks` → `cross_chunk_chain_v3` → `resolve_chains_v3` (strict pos+1)
- **Word pipeline** (0x02) : `falling_walk_words` → `cross_word_chain_v3` → `resolve_word_chains_v3` (relaxed posmap/bytemap)

Plus de mixing inter-partition.

### 5. Suppression ByteOrdered fallback
`resolve_chains_v3_relaxed` exige posmap + bytemap (non-Option).
Plus de dégradation silencieuse.

### 6. Propagation posmap/bytemap/word_sfxpost
Câblé dans tout le pipeline : `contains_v3`, `fuzzy_v3`, `find_literal_v3`,
`find_multi_token_v3`, `contains_query_v3`, `fuzzy_query_v3`, `regex_v3`.

### 7. Builder V3_DIAG_BUILD
Diag conditionnel des multi-parent FST keys. Activé par env var.

## État des tests

### Lib tests : 1421 pass, 3 fail
Fails restants :
- `diag_false_positive_uint64t` : test diag qui appelle `contains_v3` avec `None` pour les maps en mode relaxed
- `fz10_long_cross_token_d1_strict_false` : fuzzy relaxed sans word_sfxpost
- `test_resolve_chain_sep_skip` : test unitaire resolve qui appelle directement `resolve_chains_v3` (strict) au lieu du word pipeline

→ Ces 3 tests utilisent des appels directs sans les maps. À migrer vers le word pipeline.

### Ground truth : à relancer (en attente)

## Architecture actuelle du pipeline

```
Query "mutexlock" strict_sep=false
  │
  ├── fst_candidates (toutes partitions) → single-token matches
  │
  ├── Chunk pipeline (0x00 + 0x01)
  │     falling_walk_chunks → cross_chunk_chain_v3 → resolve_chains_v3 (strict)
  │     PostingResolver (sfxpost chunk)
  │
  └── Word pipeline (0x02) — requiert posmap + bytemap + word_sfxpost
        falling_walk_words → cross_word_chain_v3 → resolve_word_chains_v3 (relaxed)
        WordSfxPostReader (word_sfxpost) + fallback PostingResolver (sfxpost chunk)
```

## Prochaines étapes

### 1. Ground truth (immédiat)
Relancer pour voir l'impact sur strict et relaxed.

### 2. Optimisation : word_sfxpost dans le chain builder
Donner le word_sfxpost au chain builder pour filtrer les ordinals dès la
construction (pas au resolve). Avantages :
- Moins de chains mortes créées
- Le chain builder sait si un ordinal est word ou chunk
- Plus propre que le fallback word→chunk dans le resolve

### 3. Fixer les 3 tests restants
Migrer les tests diag/fuzzy pour passer les vraies maps.

### 4. Investiguer les FN relaxed
- TableFunction relax : 2 FN (grep trouve "table function" mais v3 ne le trouve pas)
- uint64_t relax : 4 FN
- Probablement lié au best_consumed filter ou à l'intermediates_are_pure_sep

### 5. Considérer la suppression du best_consumed filter
Avec la séparation par partition, le best_consumed ne mélange plus
chunk et word. Mais il filtre encore intra-partition. Question ouverte :
est-ce qu'on perd des vrais positifs intra-partition ?

## Commits cette session

| Hash | Description |
|------|-------------|
| `8f041e4` | fix: extended ordinals + chain builder consumed filter — 19 strict FP eliminated |
| `dcb6353` | docs: investigation report session 5 |
| `0667f76` | fix: remove ByteOrdered fallback, propagate posmap/bytemap |
| `2eb00e2` | docs: design partition-separated chains + word sfxpost + last-chunk postings |
| `d1690aa` | feat: WordSfxPost format + word-stripped postings separated from chunk sfxpost |
| `fd31754` | feat: partition-separated chains + WordSfxPost resolve + pipeline propagation |
