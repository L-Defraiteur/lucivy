# Doc 24 — Diagramme complet du search flow actuel

Date : 22 mars 2026

## DAG de recherche (ShardedHandle)

```
                         ┌──────────────────────────────────────────────────────┐
                         │              BuildWeightNode                         │
                         │                                                      │
drain ── flush ──trigger─┤  1. build_query(config) → Box<dyn Query>            │
                         │  2. thread::scope: N threads (1 par shard)          │
                         │     ├─ thread 0: build_query → prescan(shard_0_segs)│
                         │     ├─ thread 1: build_query → prescan(shard_1_segs)│  ← overhead ici
                         │     ├─ thread 2: build_query → prescan(shard_2_segs)│    (rebuild query
                         │     └─ thread 3: build_query → prescan(shard_3_segs)│     par thread)
                         │  3. merge caches + sum doc_freqs                    │
                         │  4. query.set_global_contains_doc_freqs(merged)     │
                         │  5. query.inject_prescan_cache(merged_cache)        │
                         │  6. query.weight(EnableScoring{global_stats})       │
                         │     └─ SuffixContainsQuery::weight() :              │
                         │        - lit prescan_cache → skip auto-prescan      │
                         │        - lit global_doc_freq → IDF correct          │
                         │        - crée SuffixContainsWeight avec cache       │
                         │  7. → Arc<dyn Weight>                               │
                         └──────────────┬───────────────────────────────────────┘
                                        │ weight (fan-out)
                    ┌───────────────────┼───────────────────┐
                    ▼                   ▼                   ▼
            SearchShardNode_0   SearchShardNode_1   SearchShardNode_2  ...
            (via shard pool)    (via shard pool)    (via shard pool)
                    │                   │                   │
                    │  Pour chaque segment du shard :       │
                    │  weight.scorer(seg_reader)            │
                    │  └─ lit prescan_cache[seg_id]         │
                    │  └─ zéro SFX walk (caché)             │
                    │  └─ BM25 avec global doc_freq         │
                    │  → top-K hits                         │
                    │                   │                   │
                    └───────────────────┼───────────────────┘
                                        │ hits_0, hits_1, hits_2
                                        ▼
                                MergeResultsNode
                                (binary heap top-K)
                                        │
                                        ▼
                              Vec<ShardedSearchResult>
```

## Flow du prescan dans weight() (non-shardé / fallback)

```
searcher.search(&query, &collector)
    │
    ▼
query.weight(EnableScoring::enabled_from_searcher(&searcher))
    │
    ▼
SuffixContainsQuery::weight()
    ├─ prescan_cache exists? → utilise le cache (shardé)
    └─ pas de cache? → auto-prescan :
       │  searcher.segment_readers() → tous les segments
       │  pour chaque segment :
       │    ouvre .sfx
       │    SFX walk (run_sfx_walk)
       │    cache doc_tf + highlights
       │    accumule doc_freq
       │  → prescan_cache + global_doc_freq
       │
       ▼
    crée SuffixContainsWeight {
        prescan_cache: HashMap<SegmentId, CachedSfxResult>,
        global_doc_freq: u64,  ← correct (global)
        global_num_docs: u64,  ← de EnableScoring
        global_num_tokens: u64, ← de EnableScoring
    }
```

## Flow du scorer

```
weight.scorer(seg_reader, boost)
    │
    ▼
SuffixContainsWeight::scorer()
    ├─ prescan_cache.get(segment_id)?
    │  └─ OUI: scorer_from_cached()
    │     ├─ emit highlights
    │     └─ build_scorer(cached.doc_tf, global_doc_freq)
    │        └─ Bm25Weight::for_one_term(global_doc_freq, global_num_docs, avg_fieldnorm)
    │        └─ SuffixContainsScorer { doc_tf, bm25_weight, fieldnorm }
    │
    └─ NON (fallback, scoring disabled, etc.)
       ├─ ouvre .sfx
       ├─ run_sfx_walk()
       └─ build_scorer(doc_tf, per-segment doc_freq)  ← IDF per-segment (legacy)
```

## Flow distribué (multi-nodes)

```
    Node A                          Coordinator                      Node B
    ──────                          ───────────                      ──────

1.  export_stats(query)                                          export_stats(query)
    ├─ build_query                                               ├─ build_query
    ├─ prescan all local segs                                    ├─ prescan all local segs
    ├─ collect doc_freqs                                         ├─ collect doc_freqs
    └─ → ExportableStats {                                       └─ → ExportableStats {
         total_docs: 50,              ◄── réseau ──►                  total_docs: 50,
         total_tokens: {...},                                         total_tokens: {...},
         contains_doc_freqs:                                          contains_doc_freqs:
           {"mutex": 50}                                                {"mutex": 50}
       }                                                            }
                                    2. merge([stats_a, stats_b])
                                       → GlobalStats {
                                           total_docs: 100,
                                           contains_doc_freqs:
                                             {"mutex": 100}
                                         }

3.  search_with_global_stats                                     search_with_global_stats
    (query, global_stats)            ◄── réseau ──►              (query, global_stats)
    ├─ build_query                                               ├─ build_query
    ├─ prescan local segs (cache)                                ├─ prescan local segs (cache)
    ├─ inject global                                             ├─ inject global
    │  contains_doc_freqs                                        │  contains_doc_freqs
    ├─ weight(global_stats)                                      ├─ weight(global_stats)
    │  └─ IDF = f(100, 100) ✓                                   │  └─ IDF = f(100, 100) ✓
    ├─ scorer per segment                                        ├─ scorer per segment
    │  └─ lit prescan cache                                      │  └─ lit prescan cache
    └─ → top-K + highlights          ◄── réseau ──►             └─ → top-K + highlights

                                    4. merge top-K (binary heap)
                                       → résultats finaux
```

## Trait methods sur Query (pour le prescan)

```
trait Query {
    fn weight(...)              ← standard
    fn query_terms(...)         ← standard
    fn prescan_segments(...)    ← SFX walk + cache + count doc_freq
    fn collect_prescan_doc_freqs(...)  ← export doc_freq pour coordinateur
    fn set_global_contains_doc_freqs(...)  ← inject doc_freq global
    fn take_prescan_cache(...)  ← extraire cache (pour merge entre threads)
    fn inject_prescan_cache(...)  ← injecter cache mergé
}
```

Implémentés par :
- **SuffixContainsQuery** : fait le travail
- **BooleanQuery** : propage aux sous-queries
- **Tous les autres** : no-op (défaut)

## Où est le overhead actuel

```
BuildWeightNode::execute()
    │
    ├─ thread::scope (spawn N threads)     ← ~5-10ms overhead
    │   ├─ build_query() × N              ← ~5-10ms par query × N shards
    │   ├─ prescan_segments() × N          ← travail utile (~15ms/shard sur 5K)
    │   └─ take/collect × N                ← ~1ms
    │
    ├─ merge caches                        ← ~1ms
    ├─ build_query() (main, avec highlights) ← ~5ms
    ├─ inject_prescan_cache                ← ~1ms
    └─ weight()                            ← ~1ms (lit le cache, pas de SFX walk)

Total sur 5K/4sh : ~100ms (dont ~60ms de SFX walk utile, ~40ms d'overhead)
Sans prescan (ancien) : ~60ms (SFX walk dans scorer, parallèle via shard pool)
```

## Piste d'optimisation : supprimer le rebuild de query par thread

```
BuildWeightNode::execute() optimisé
    │
    ├─ build_query() (une seule fois)
    ├─ collecter tous les segment_readers de tous les shards
    ├─ query.prescan_segments(&all_segs)   ← séquentiel, zéro overhead
    │   └─ SFX walk sur chaque segment (~15ms × 4 = 60ms)
    ├─ weight()                            ← lit le cache
    └─ total : ~65ms (vs 100ms actuel, vs 60ms ancien)
```

Alternative parallèle sans rebuild :
```
    ├─ build_query() (une seule fois)
    ├─ thread::scope : chaque thread appelle prescan() directement
    │   sur les segments de son shard (pas build_query, juste le walk)
    ├─ merge caches
    ├─ inject dans la query
    ├─ weight()
    └─ total : ~25ms (walk parallèle sans rebuild overhead)
```
