# Récap session SFX Unified — 16 mars 2026

## Branche : `feature/sfx-unified`

## Ce qui a été fait

### Phases 2-5 : Query weights reroutés via PostingResolver
- AutomatonWeight, TermWeight, AutomatonPhraseWeight, SuffixContainsQuery
- `prefer_sfxpost` flag opt-in pour le path ordinal
- BM25 per-segment depuis match stats

### Phase 6 : Merger .sfxpost
- Reconstruction .sfxpost pendant merge
- Filtrage tokens des docs supprimés (ordinal alignment)
- GC fix : .sfxpost protégé du garbage collector

### Phase 7c : Suppression ._raw
- `build_schema()` ne crée plus de champs ._raw
- `build_query()` simplifié à 4 args (plus de raw_pairs/ngram_pairs)
- Merger filtre par .sfx présence au lieu de ._raw name
- Segment_writer : double tokenization quand stemmer actif
- RAW_TOKENIZER comme tokenizer principal quand pas de stemmer

### Optimisation ingestion
- Token interning dans SfxCollector (HashMap au lieu de BTreeMap)
- Batch suffix generation dans SuffixFstBuilder (Vec+sort au lieu de BTreeMap)
- SfxCollector self-tracking ti avec gap boundary

### Cleanup complet
- Supprimé : raw_field_pairs, ngram_field_pairs, RAW_SUFFIX, InvertedIndexResolver
- 6 bindings nettoyés (auto-duplication, RAW_SUFFIX filters)
- Version bump : ld-lucivy 0.27.0, lucivy-core 0.2.0

## Tests
- ld-lucivy : 1203 passed
- lucivy-core : 64 passed

## Prochaine étape : Token-Aware Sharding

### Design docs
- `09-design-token-aware-sharding.md` — IDF-weighted routing, df threshold, power of two choices
- `10-architecture-sharding-deux-couches.md` — lucivy intra-index + rag3weaver applicative

### Implémentation Phase 1
1. `ShardRouter` struct dans lucivy_core (compteurs per-token per-shard, score IDF sqrt)
2. `ShardedHandle` wraps N `LucivyHandle` — même API (create/open/add/search/commit)
3. Route `add_document` via ShardRouter
4. `search` dispatch sur N shards en parallèle, heap merge top-K
5. `_shard_stats.bin` persistance des compteurs au commit
6. Stats globales BM25 agrégées

### Fondations en place
- LucivyHandle est un wrapper propre (pas de raw_field_pairs)
- Queries stateless (build_query ne dépend pas de l'état)
- BM25 stats via EnableScoring (injectable)
- Commit indépendant par index
- Format .sfx/.sfxpost autonome par segment
