# Feature Inventory — lucivy v2

## Core Search Engine

### Query Types

Toutes les queries texte passent par le SFX engine (cross-token, highlights).
Les anciens types (term, fuzzy, regex, phrase) sont routés automatiquement.

| Type | Comportement | Paramètres clés |
|------|-------------|-----------------|
| **contains** | Substring search via SFX | `field, value, distance, anchor_start, exact_match, regex, strict_separators` |
| **contains_split** | Whitespace split → boolean should | `field, value, distance` |
| **term** (compat) | → contains + anchor_start + exact_match | `field, value` |
| **fuzzy** (compat) | → contains + distance | `field, value, distance` |
| **regex** (compat) | → contains + regex=true | `field, value/pattern` |
| **phrase** (compat) | → contains (multi-token adjacency) | `field, value/terms` |
| **startsWith** (compat) | → contains + anchor_start | `field, value` |
| **startsWith_split** (compat) | → contains_split + anchor_start | `field, value` |
| **phrase_prefix** (compat) | → contains (prefix last token) | `field, terms, max_expansions` |
| **parse** (compat) | → contains pour values simples | `field/fields, value` |
| **boolean** | must/should/must_not composites | `must[], should[], must_not[]` |
| **disjunction_max** | Max score parmi sub-queries | `queries[], tie_breaker` |
| **more_like_this** | Documents similaires (TF-IDF) | `field, value, min/max_doc_frequency, etc.` |

### Paramètres contains

| Param | Type | Default | Effet |
|-------|------|---------|-------|
| `distance` | u8 | 0 | Levenshtein distance (0=exact, >0=fuzzy via trigram pigeonhole) |
| `anchor_start` | bool | false | SI=0 only — match au début du token |
| `exact_match` | bool | false | Match couvre le(s) token(s) entier(s) |
| `regex` | bool | false | Pattern regex cross-token |
| `strict_separators` | bool | false | Valider les séparateurs entre tokens |

### Filter Fields (non-texte)

| Op | Types supportés |
|----|----------------|
| eq, ne | U64, I64, F64, Str |
| lt, lte, gt, gte | U64, I64, F64 |
| in, not_in | Tous |
| between | U64, I64, F64 |
| starts_with, contains | Str |
| must, should, must_not | Composites |

---

## SFX Engine — Suffix FST

L'algo propriétaire de lucivy pour le substring search.

### Architecture

- **Double tokenization** : chaque document est tokenisé en RAW (lowercase + split) puis chaque token génère toutes ses suffixes dans le FST
- **Partitioned SI** : SI=0 (début de token) séparé de SI>0 (suffixes). Permet `anchor_start` sans scanner les suffixes
- **Cross-token** : `falling_walk` + `sibling_table` pour matcher des queries qui traversent les frontières de tokens ("rag3weaver" → "rag3" + "weaver")
- **Fuzzy** : Levenshtein DFA sur le FST partitionné. `fuzzy_walk_si0` pour startsWith fuzzy
- **RegexContinuationQuery** : trigram pigeonhole pour fuzzy d>0, correct ordering

### Fichiers SFX par segment

| Extension | Contenu |
|-----------|---------|
| `.sfx` | Suffix FST (toutes les suffixes, partitionné SI=0/SI>0) |
| `.sfxpost` | Posting lists suffix → doc_ids (v2 binary format) |
| `.termtexts` | Texte des tokens (ordinal → string) |
| `.gapmap` | Byte sequences compressées (gap encoding) |
| `.sepmap` | Carte des séparateurs entre tokens |

### Performance (benchmarks 90K docs Linux kernel)

| Query type | Temps | Notes |
|-----------|-------|-------|
| term | ~0.2ms | via term dict (natif) |
| phrase | 1-5ms | 2.5x faster que tantivy sur 4-shard |
| fuzzy | ~2ms | 2x faster que tantivy sur 4-shard |
| regex | ~2ms | Comparable tantivy |
| contains (exact) | ~700ms | Substring via SFX (unique à lucivy) |
| phrase_prefix | ~1ms | Autocomplétion |
| more_like_this | ~0.7ms | Recommandation |

---

## Sharding

### Token-Aware Routing (ShardRouter)

- Documents routés vers les shards par scoring hybride IDF + balance
- `score = (1 - balance_weight) × per_token_score + balance_weight × shard_load_ratio`
- Default `balance_weight=1.0` (round-robin-like, fastest indexation, no quality loss)
- Power-of-2 choices pour N shards élevé
- `df_threshold` : ne track que les tokens avec df < seuil (default 5000)

### Storage Backends (ShardStorage trait)

| Backend | Usage |
|---------|-------|
| FsShardStorage | Natif (filesystem, default) |
| RamShardStorage | Tests, WASM |
| BlobShardStorage | ACID (mmap + DB blob) — extensible |

---

## Formats d'échange

### LUCE — Snapshot complet

- Export complet de l'index (tous les shards, tous les segments)
- `export_to_snapshot(handle)` → `Vec<u8>` (binary blob)
- `import_from_snapshot(data, dest)` → `ShardedHandle`
- Auto-wraps non-sharded → shard_0
- Exclut lock files (.lock, .managed.json)

### LUCID — Delta incrémental (1 shard)

- Sync incrémental d'un seul shard
- Format binaire : magic "LUCID" + version + segments ajoutés/supprimés + meta
- `export_delta(handle, index_path, client_segment_ids, client_version)` → `IndexDelta`
- `serialize_delta(delta)` → `Vec<u8>`
- `deserialize_delta(data)` → `IndexDelta`
- Le client envoie ses segment_ids + sa version, le serveur retourne seulement ce qui a changé

### LUCIDS — Delta incrémental sharded (N shards)

- Wraps N deltas LUCID individuels dans un seul blob
- Format binaire : magic "LUCIDS" + version + num_shards + shard_config + N × (shard_id + LUCID blob)
- Seuls les shards qui ont changé sont inclus

**API ShardedHandle :**
```rust
// Serveur : export delta pour un client qui a ces versions
handle.export_sharded_delta(base_path, &[
    ShardVersion { shard_id: 0, version: "abc", segment_ids: {...} },
    ShardVersion { shard_id: 1, version: "def", segment_ids: {...} },
]) → Vec<u8>  // LUCIDS blob

// Client : appliquer le delta reçu
handle.apply_sharded_delta(base_path, &blob) → Ok(())
// Écrit les fichiers segments, supprime les anciens, reload readers
```

**ShardVersion** : per-shard version info envoyée par le client
- `shard_id: usize`
- `version: String` (hash des segment_ids)
- `segment_ids: HashSet<String>`

---

## Distributed (multi-machine) — PRÊT

Tout le nécessaire pour le search distribué est implémenté.
Il ne manque que la couche réseau (le "comment transporter").

### Flow de recherche distribuée

```
Coordinator reçoit query
  1. broadcast export_stats(query) → tous les nodes
     ← chaque node retourne ExportableStats (sérialisable JSON)
  2. ExportableStats::merge(all_stats) → stats globales
  3. broadcast search_with_global_stats(query, top_k, merged_stats)
     ← chaque node retourne ses top-K résultats
  4. merge final des résultats (binary heap top-K)
```

### ExportableStats (sérialisable, Serialize + Deserialize)

```rust
pub struct ExportableStats {
    pub total_num_docs: u64,
    pub total_num_tokens: HashMap<u32, u64>,      // field_id → count
    pub doc_freqs: HashMap<Vec<u8>, u64>,          // term → doc frequency
    pub contains_doc_freqs: HashMap<String, u64>,  // SFX query → doc freq
    pub regex_doc_freqs: HashMap<String, u64>,     // regex pattern → doc freq
}
```

- `ExportableStats::from_searchers(searchers, terms)` — extrait les stats d'un node
- `ExportableStats::merge(stats[])` — agrège les stats de N nodes
- Couvre BM25 classique + SFX contains + regex — scoring identique local et distribué

### API ShardedHandle

```rust
// Phase 1 : export stats locales pour cette query
handle.export_stats(&query_config) → ExportableStats

// Phase 2 : search avec stats globales (reçues du coordinator)
handle.search_with_global_stats(&query_config, top_k, &global_stats, highlight_sink)
    → Vec<ShardedSearchResult>
```

### Sync de données via LUCIDS

- Chaque node sert N shards
- Le coordinator envoie les `ShardVersion` du client
- Le node retourne un LUCIDS blob (seulement les shards modifiés)
- Le client applique le delta → reload instantané

### Ce qui reste à faire

- Couche transport (gRPC / HTTP / WebSocket — au choix de l'intégrateur)
- Discovery / registration des nodes (optionnel — peut être statique)
- Rebalancing des shards (migration de shard d'un node à l'autre)
- Failover / réplication (un shard sur 2+ nodes)

---

## luciole — Framework Actor/DAG

### Composants

| Module | Description |
|--------|-------------|
| **Actor** | Trait actor avec priorités (Idle→Critical), mailbox typée |
| **GenericActor** | Actor dynamique avec handlers enregistrés par type |
| **Pool** | Pool d'actors identiques, scatter/gather |
| **Scheduler** | Pool de threads persistants, WASM compatible, work stealing |
| **DAG** | Construction + exécution topologique, services, undo |
| **StreamDag** | Pipeline streaming avec drain topologique |
| **BranchNode** | Branchement conditionnel 2-way (fonction, pas struct) |
| **GateNode** | Pass/block |
| **MergeNode** | Fan-out N-way avec merge |
| **ScatterDAG** | Fan-out distribué |
| **WaitGraph** | Tracking de dépendances inter-thread, dump mermaid |
| **pipe_to** | Request-reply déclaratif (callback avant envoi) |
| **collect_replies_to** | N:1 gather (AtomicUsize countdown) |
| **task_pipe_to** | CPU task → message actor |
| **execute_dag_async** | DagExecutor actor (DAG non-bloquant) |
| **Checkpoint** | Recovery (FileCheckpointStore, MemoryCheckpointStore) |
| **ServiceRegistry** | Services nommés injectés dans NodeContext |

### Règles WASM

- **JAMAIS de `thread::spawn`** dans les handlers — tout via le scheduler (tasks/actors)
- `docstore_compress_dedicated_thread: false` en WASM
- Callbacks watch inline en WASM
- GC thread skip en WASM

---

## Bindings

| Binding | Language | Package | Version publiée |
|---------|----------|---------|-----------------|
| **python** | PyO3 | `lucivy` (PyPI) | 0.3.2 (9 versions) |
| **nodejs** | NAPI | `lucivy` (npm) | 0.2.1 (3 versions) |
| **emscripten** | extern "C" + SharedArrayBuffer | (playground) | — |
| **wasm** | wasm-bindgen | (intégré) | — |
| **cpp** | cxx bridge | (rag3db extension) | — |
| **crates.io** | Rust | `lucivy-core` | 0.1.1 |

---

## Tokenizers

| Nom | Comportement |
|-----|-------------|
| RAW_TOKENIZER | Lowercase + split non-alphanum (default) |
| STEMMED_TOKENIZER | Lowercase + stemming + stop words |
| CamelCaseSplit | Split sur CamelCase boundaries |
| SimpleTokenizer | Split alphanum basique |
| WhitespaceTokenizer | Split whitespace only |
| RegexTokenizer | Split par regex custom |

### Filters composables

LowerCaser, AsciiFolding, AlphanumOnly, SplitCompoundWords, RemoveLong, StopWordFilter

---

## Highlights

- `HighlightSink` : `Arc<Mutex<HashMap<(SegmentId, DocId), Vec<(field, from, to)>>>>>`
- Thread-safe, passé via `with_highlight_sink()`
- Supporté sur : term, phrase, contains, regex (toutes les queries SFX)
- Byte offsets dans le texte source (cross-token aware)

---

## Diagnostics

### DiagBus (zero-cost quand pas de subscriber)

| Event | Quand |
|-------|-------|
| TokenCaptured | Indexation (chaque token) |
| SuffixAdded | Build SFX |
| SfxWalk | Recherche SFX |
| SfxResolve | Résolution posting |
| SearchMatch | Match trouvé |
| SearchComplete | Recherche terminée |
| MergeDocRemapped | Merge segments |

### ActorActivity (scheduler dumps)

- Labels dynamiques dans les handlers (`add_doc 42/500`, `commit_drain shard_2`)
- `activity_reporter` callback dans SegmentWriter (`fast_fields`, `index_doc`, `sfx_empty`, `store_doc`)
- WaitGraph mermaid/text dumps pour deadlock analysis

---

## Mutations

| Op | Méthode |
|----|---------|
| ADD | `writer.add_document(doc)` |
| DELETE | `writer.delete_term(term)` |
| UPDATE | delete + add (via NODE_ID_FIELD) |
| COMMIT | `handle.commit()` (flush + finalize + merge trigger) |
| CLOSE | `handle.close()` (commit if dirty, release flock) |

Lazy commit : mutations → `dirty_=true`, query → `flushIfDirty()`

---

## Persistence

| Directory | Platform | I/O pattern |
|-----------|----------|-------------|
| StdFsDirectory | All | Deferred I/O (RAM until terminate) |
| RamDirectory | All | Pure RAM |
| MemoryDirectory | WASM | RAM + dirty tracking + OPFS sync |
| BlobDirectory | All | DB blob store + local cache |
| MmapDirectory | Native only | Zero-copy mmap |

WRITER_HEAP_SIZE : 50MB natif, 15MB WASM
MAXIMUM_MEMORY WASM : 4GB (limit 32-bit)
