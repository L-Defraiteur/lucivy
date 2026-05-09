# Migration guide: v1 to v2

## Query types

v2 introduces `contains` as the primary query type. All legacy query types still work via the compat layer — they are automatically routed through the SFX engine.

### What changed

| v1 query type | v2 equivalent | Notes |
|---------------|---------------|-------|
| `startsWith` | `contains` + `anchor_start: true` | `startsWith` still works (compat layer) |
| `term` | `contains` + `anchor_start: true` + `exact_match: true` | Cross-token exact match |
| `fuzzy` | `contains` + `distance: N` | Cross-token fuzzy via SFX |
| `regex` | `contains` + `regex: true` | Cross-token regex via literal extraction |
| `phrase` | `contains` | Multi-token adjacency |
| `parse` | `contains` | Simple values only |
| `phrase_prefix` | `contains` | Prefix match on last token |
| `contains` | unchanged | Now powered by SFX instead of trigrams |
| `contains_split` | unchanged | Still splits on whitespace, OR's sub-queries |
| `boolean` | unchanged | |

### New parameters on `contains`

```python
index.search({
    "type": "contains",
    "field": "body",
    "value": "mutex",
    "distance": 1,          # Levenshtein distance (0 = exact)
    "anchor_start": True,   # Match must start at token boundary
    "exact_match": True,    # Match must cover entire token(s)
    "regex": True,          # Treat value as regex pattern
})
```

### No code changes required

If you were using `term`, `fuzzy`, `regex`, `phrase`, or `startsWith` — they still work. The compat layer routes them through the SFX engine automatically. You only need to change if you want to use the new parameters.

## Scoring

### Fuzzy scores can be negative

Fuzzy search uses a tiered scoring system:

```
score = miss_penalty * 1000 + bm25_score
```

- 0 misses (exact match): positive BM25 score
- 1 miss (1-edit): -1000 + BM25
- 2 misses (2-edit): -2000 + BM25

**This is intentional.** The ordering is correct: exact matches rank above 1-edit matches, which rank above 2-edit matches. BM25 serves as a tiebreaker within each tier.

If you were filtering results by `score > 0`, you'll need to adjust for fuzzy queries.

## Sharding

### Default balance_weight changed

The default `balance_weight` changed from **0.2** (token-aware) to **1.0** (round-robin).

- `1.0` — even distribution, fastest indexation
- `0.2` — co-locates documents sharing rare tokens (better search performance, slower indexation)

If you relied on token-aware routing, explicitly set `balance_weight: 0.2` in your schema config.

## WASM

### emscripten only

The wasm-bindgen (single-threaded) binding has been removed. Only the emscripten build (multithreaded, SharedArrayBuffer) is supported.

If you were using `lucivy-wasm` with the wasm-bindgen build, switch to the emscripten build. The API is async (worker-based).

### COOP/COEP headers required

The emscripten build requires these HTTP headers for SharedArrayBuffer:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

For environments without header control (GitHub Pages), use [coi-serviceworker](https://github.com/nicojuicy/coi-serviceworker).

## New features (no migration needed)

### Snapshots & delta sync

All bindings now support:

```python
# Full snapshot
snapshot = index.export_snapshot()
restored = lucivy.Index.from_snapshot(snapshot, "/tmp/restored")

# Incremental delta (only changed segments)
versions = index.shard_versions()
delta = index.export_sharded_delta(client_versions)
client_index.apply_sharded_delta(delta)
```

### Distributed search

```python
# Export local BM25 stats
stats = index.export_stats(query_config)

# Merge stats from all nodes
global_stats = lucivy.merge_stats([stats_a, stats_b])

# Search with global stats (correct IDF)
results = index.search_with_global_stats(query_config, top_k=10, global_stats=global_stats)
```

## Index format

v2 indexes are **not backwards compatible** with v1. You need to re-index your data or use snapshot import/export.

The `.sfx`, `.sfxpost`, `.termtexts`, and `.gapmap` files are new in v2 and are generated automatically during indexing.
