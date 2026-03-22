# Doc 25 — Optimisation : prescan parallèle sans rebuild de query

Date : 22 mars 2026

## Problème

Le prescan parallèle actuel (dans BuildWeightNode) rebuild `build_query()` par
thread. Sur 4 shards : 5 × build_query (4 threads + 1 main) au lieu de 1.
Résultat : 100ms au lieu de 60ms sur 5K.

## Solution

`run_sfx_walk()` est une fonction standalone qui ne dépend pas de la query.
Elle a besoin de : `SfxFileReader`, `resolver`, `query_text`, `tokens`,
`separators`, `fuzzy_distance`, `prefix_only`, `continuation`.

On extrait ces paramètres UNE FOIS depuis la config, puis chaque thread
fait juste le walk sur ses segments.

## Nouveau flow dans BuildWeightNode

```
BuildWeightNode::execute()
│
├─ 1. build_query(config) → Box<dyn Query>     (1 seule fois)
│
├─ 2. Extraire les paramètres de walk :
│     - query_text = config.value.to_lowercase()
│     - (tokens, seps) = tokenize_query(&query_text)
│     - fuzzy_d = config.distance
│     - prefix_only = config.query_type == "startsWith"
│     - raw_field = resolve_field(config, schema)
│
├─ 3. Collecter les segments par shard :
│     all_shard_segs[0] = shard_0.segment_readers()
│     all_shard_segs[1] = shard_1.segment_readers()
│     ...
│
├─ 4. thread::scope : parallèle par shard
│     ┌─ thread 0 ─────────────────────────────────┐
│     │ pour chaque segment de shard_0 :            │
│     │   sfx_reader = open_sfx(seg, raw_field)     │
│     │   resolver = build_resolver(seg, raw_field)  │
│     │   (doc_tf, hl) = run_sfx_walk(              │
│     │     sfx_reader, resolver, query_text,        │
│     │     tokens, seps, fuzzy_d, prefix, cont)     │
│     │   cache[seg_id] = (doc_tf, hl)              │
│     │   doc_freq += doc_tf.len()                   │
│     └─────────────────────────────────────────────┘
│     ┌─ thread 1 ─────────────────────────────────┐
│     │ (même chose sur shard_1)                    │
│     └─────────────────────────────────────────────┘
│     ...
│
├─ 5. Merge :
│     merged_cache = union(thread_0_cache, thread_1_cache, ...)
│     global_doc_freq = sum(thread_0_freq, thread_1_freq, ...)
│
├─ 6. Inject dans la query :
│     query.set_global_contains_doc_freqs({"mutex": 100})
│     query.inject_prescan_cache(merged_cache)
│
├─ 7. query.weight(EnableScoring{global_stats}) → Weight
│     └─ weight() voit le cache → skip auto-prescan
│     └─ utilise global_doc_freq → IDF correct
│
└─ 8. → Arc<dyn Weight>
```

## Pour contains_split "struct device"

La config a `query_type: "contains_split"`, `value: "struct device"`.
Ça s'expand en BooleanQuery { should: [contains("struct"), contains("device")] }.

Le prescan doit walker les DEUX termes sur chaque shard.

```
thread 0 (shard_0) :
  walk "struct" → cache_struct[seg_id], freq_struct += N
  walk "device" → cache_device[seg_id], freq_device += M

Merge :
  global_freqs = {"struct": sum_struct, "device": sum_device}
  merged_cache = union(all segment caches, keyed by seg_id)

Inject :
  query.set_global_contains_doc_freqs(global_freqs)
  query.inject_prescan_cache(merged_cache)  ← BooleanQuery propage
```

Chaque sous-query du boolean a son propre query_text → son propre doc_freq.
Le `set_global_contains_doc_freqs({"struct": X, "device": Y})` dispatch
via la vtable vers chaque SuffixContainsQuery qui lit SON freq.

## Comment extraire les sous-termes pour contains_split

```rust
fn extract_contains_terms(config: &QueryConfig) -> Vec<(String, bool, u8)> {
    // (query_text, prefix_only, fuzzy_distance)
    match config.query_type.as_str() {
        "contains" | "sfx_contains" => {
            vec![(config.value.clone().unwrap().to_lowercase(),
                  false, config.distance.unwrap_or(0))]
        }
        "startsWith" => {
            vec![(config.value.clone().unwrap().to_lowercase(),
                  true, config.distance.unwrap_or(0))]
        }
        "contains_split" | "sfx_contains_split" => {
            // Split par whitespace → un term par mot
            config.value.as_ref().unwrap().split_whitespace()
                .map(|w| (w.to_lowercase(), false, config.distance.unwrap_or(0)))
                .collect()
        }
        "startsWith_split" => {
            config.value.as_ref().unwrap().split_whitespace()
                .map(|w| (w.to_lowercase(), true, config.distance.unwrap_or(0)))
                .collect()
        }
        _ => vec![], // pas de prescan pour term, phrase, regex, etc.
    }
}
```

## Performances attendues

| Dataset | Actuel (rebuild) | Optimisé (sans rebuild) | Ancien (sans prescan) |
|---------|-----------------|------------------------|----------------------|
| 5K/4sh  | ~100ms          | ~25ms                  | ~60ms                |
| 90K/4sh | ~650ms          | ~200ms                 | ~620ms               |

L'optimisé est PLUS RAPIDE que l'ancien sur 5K (pas de double SFX walk
dans scorer, le cache est déjà prêt).

## Changements code

1. **BuildWeightNode::execute()** : remplacer le thread::scope qui rebuild
   par un thread::scope qui appelle run_sfx_walk directement
2. **Ajouter `extract_contains_terms()`** dans query.rs ou search_dag.rs
3. **Aucun changement** au trait Query, au DAG, au scorer, au Weight
4. **Garder le fallback** auto-prescan dans weight() pour le non-shardé

## Invariants préservés

- Scores = ground truth (même prescan, même cache, même IDF)
- DAG inchangé : drain → flush → build_weight → search_N ∥ → merge
- Non-shardé : auto-prescan dans weight() (inchangé)
- Distribué : export_stats + search_with_global_stats (inchangé)
