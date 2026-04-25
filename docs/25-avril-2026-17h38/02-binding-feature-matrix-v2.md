# Feature matrix bindings — cible v2

## État actuel

| Feature | Python | Node.js | C++ | WASM-bg | Emscripten | CXX rag3db |
|---------|--------|---------|-----|---------|------------|------------|
| **Handle** | Sharded | Lucivy | Lucivy | Lucivy | Sharded | Lucivy |
| create | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| open | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| close | ✓ | ✗ | ✗ | ✗ | ✓ | ✓ |
| add/add_many | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| delete | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| commit | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| search | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| search_filtered | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| highlights | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| field retrieval | ✓ | ✓ | ✗ | ✗ | ✓ | ✗ |
| num_docs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| num_shards | ✓ | ✗ | ✗ | ✗ | ✓ | ✗ |
| schema | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| shards param | ✓ | ✗ | ✗ | ✗ | ✓ | ✗ |
| **Snapshot** | | | | | | |
| export_snapshot | ✓ | ✓ | ✓ | ✓ | ✗ | ✗ |
| import_snapshot | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ |
| export_to file | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ |
| import_from file | ✓ | ✓ | ✓ | ✗ | ✗ | ✗ |
| **Delta (LUCID)** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Delta shardé (LUCIDS)** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Distribué** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |

## Cible v2 — ce qu'on veut dans tous les bindings

### Tier 1 : obligatoire pour release

Tous les bindings doivent avoir :

- [ ] **ShardedHandle** — handle unifié, `shards` param dans create
- [ ] **close()** — release writer lock proprement
- [ ] **Toutes les query types** via JSON config :
  - contains, contains_split (string shortcut multi-field)
  - fuzzy (contains + distance)
  - regex
  - phrase, term, parse
  - startsWith, startsWith_split
  - boolean (must/should/must_not)
  - phrase_prefix
  - disjunction_max
  - more_like_this
- [ ] **search + search_filtered** — avec pre-filter AliveBitSet
- [ ] **highlights** — offsets par champ
- [ ] **field retrieval** — stored fields dans les résultats
- [ ] **num_docs, num_shards, schema**
- [ ] **Snapshot LUCE v2** :
  - export_snapshot (bytes) + export_snapshot_to (file)
  - import_snapshot (bytes) + import_snapshot_from (file)
  - Shardé transparent (auto-détecté)

### Tier 2 : nécessaire pour sync/production

- [ ] **LUCID delta sync** :
  - `version` — hash de meta.json
  - `segment_ids` — liste des segments actuels
  - `export_delta(client_version, client_segment_ids)` → LUCID blob
  - `apply_delta(blob)` — applique le delta (add/remove segments)
- [ ] **LUCIDS delta shardé** :
  - `export_sharded_delta(shard_versions)` → LUCIDS blob
  - `apply_sharded_delta(blob)` — applique per-shard
- [ ] **Export/import to file** pour LUCID/LUCIDS

### Tier 3 : nécessaire pour distribué

- [ ] **export_stats(query)** — BM25 stats pour agrégation
- [ ] **search_with_global_stats(query, stats)** — search avec IDF global
- [ ] **Shard routing info** — quel shard a quel node_id

### Par binding — travail restant

#### Python (PyO3) — quasi prêt
- [x] ShardedHandle
- [x] close, search_filtered, highlights, fields
- [ ] Delta LUCID (était là avant migration, à ré-ajouter sur ShardedHandle)
- [ ] Delta LUCIDS
- [ ] Distribué
- [ ] phrase_prefix, disjunction_max, more_like_this (JSON config suffit)

#### Node.js (NAPI) — à migrer
- [ ] Migrer vers ShardedHandle
- [ ] Ajouter close()
- [ ] Ajouter shards param
- [ ] num_shards
- [ ] Delta LUCID
- [ ] Delta LUCIDS
- [ ] Distribué

#### C++ standalone (CXX) — à migrer
- [ ] Migrer vers ShardedHandle
- [ ] Ajouter close()
- [ ] Ajouter shards param, num_shards
- [ ] field retrieval dans search results
- [ ] Delta LUCID
- [ ] Delta LUCIDS
- [ ] Distribué

#### WASM wasm-bindgen — à migrer
- [ ] Migrer vers ShardedHandle
- [ ] Ajouter close()
- [ ] Ajouter shards param, num_shards
- [ ] field retrieval
- [ ] Delta LUCID (pour sync serveur → browser)
- [ ] Delta LUCIDS

#### Emscripten — quasi prêt
- [x] ShardedHandle + OPFS
- [x] close, search_filtered
- [ ] export_snapshot (manquant)
- [ ] field retrieval dans search (le param existe, à vérifier)
- [ ] Delta LUCID
- [ ] Delta LUCIDS
- [ ] Nettoyer les logs de diagnostic

#### CXX bridge rag3db — à migrer
- [ ] Migrer vers ShardedHandle
- [ ] Snapshot support
- [ ] Delta LUCID
- [ ] Delta LUCIDS
- [ ] Distribué (prioritaire — c'est le backend rag3weaver)

## Query types — cheat sheet

Toutes les queries passent par un JSON `QueryConfig` :

```json
// Contains exact (substring via SFX)
{"type": "contains", "field": "content", "value": "rag3db"}

// Contains fuzzy (d=1 Levenshtein sur substring)
{"type": "contains", "field": "content", "value": "rag3db", "distance": 1}

// Contains split (multi-mot, chaque mot → contains, boolean should)
{"type": "contains_split", "field": "content", "value": "rust programming"}

// Contains split fuzzy
{"type": "contains_split", "field": "content", "value": "rust programming", "distance": 1}

// Regex (sur le term dict, pas substring)
{"type": "regex", "field": "content", "value": "program[a-z]+"}

// Regex contains (via SFX, substring regex)
{"type": "contains", "field": "content", "value": "program[a-z]+", "regex": true}

// Phrase exacte
{"type": "phrase", "field": "content", "value": "rust programming language"}

// Prefix autocomplétion
{"type": "phrase_prefix", "field": "content", "value": "rust prog"}

// Term exact (token complet)
{"type": "term", "field": "content", "value": "rust"}

// Fuzzy term (Levenshtein sur term dict)
{"type": "fuzzy", "field": "content", "value": "rust", "distance": 1}

// Parse (QueryParser syntax)
{"type": "parse", "field": "content", "value": "rust AND programming"}

// StartsWith (prefix via SFX)
{"type": "startsWith", "field": "content", "value": "prog"}

// Boolean composite
{"type": "boolean", "must": [...], "should": [...], "must_not": [...]}

// Disjunction max
{"type": "disjunction_max", "queries": [...], "tie_breaker": 0.3}

// More like this
{"type": "more_like_this", "field": "content", "value": "reference text here"}
```

## Architecture de sync

```
Serveur (autorité)
  ShardedHandle (N shards)
  │
  ├── LUCIDS delta ──→ Client A (browser, OPFS)
  │                     ShardedHandle (N shards)
  │                     apply_sharded_delta()
  │
  ├── LUCIDS delta ──→ Client B (Node.js)
  │                     ShardedHandle (N shards)
  │                     apply_sharded_delta()
  │
  └── Distribué ─────→ Nœud distant
                        ShardedHandle (subset de shards)
                        export_stats() → agrégation → search_with_global_stats()
```
