# Design — Reader Actors Pipeline + Background Finalize

Date : 17 mars 2026
Status : **Implémenté et benchmarké**

## Problème

`ShardedHandle::add_document()` était **synchrone et sériel** :
tokenize → route → send, un doc à la fois. Et `SegmentWriter::finalize()`
(SfxCollector.build + remap_and_write) bloquait l'IndexerActor pendant que le
suffix FST se construisait.

### Double tokenisation

Le même texte est tokenisé **deux fois** :
1. `extract_token_hashes()` — pour le routing (produit `Vec<u64>` de hashes)
2. `SegmentWriter::index_document()` — pour les postings (tokens + positions + offsets)

→ Phase 1 ne résout pas la double tokenisation. Phase 2 pourra passer des tokens pré-calculés.

## Optimisation 1 — Reader Actors Pipeline

### Architecture

```
Caller
  ↓ add_document(doc, node_id)                    // non-blocking
  ↓
IngestPipeline
  ├─→ ReaderActor[0] → tokenize+hash → send to RouterActor
  ├─→ ReaderActor[1] → tokenize+hash → send to RouterActor
  └─→ ReaderActor[N] → tokenize+hash → send to RouterActor
                                          ↓
                                    RouterActor (unique, séquentiel)
                                      ├─ router.route(hashes) → shard_id
                                      ├─ router.record_node_id(node_id, shard_id)
                                      └─ send doc to ShardActor[shard_id]
                                           ↓
                                    ShardActor[shard_id] → writer.add_document(doc)
```

### ReaderActor (pool de N, défaut = max(num_shards, 2))

- GenericActor, stateless (shared `Arc<ReaderContext>` via closure)
- Reçoit `(LucivyDocument, node_id)` via envelope.local
- Tokenize tous les champs texte → `Vec<u64>` hashes (CPU-bound)
- Forward `(doc, node_id, hashes)` au RouterActor
- Handler `PipelineDrainMsg` : ack pour garantir FIFO drain

### RouterActor (unique)

- GenericActor, capture `Arc<Mutex<ShardRouter>>` + `Vec<ActorRef<Envelope>>`
- Route séquentiellement, zéro contention
- Handler `PipelineDrainMsg` : ack

### Changements

- `add_document()` retourne `Result<(), String>` (fire-and-forget au reader)
- `add_document_with_hashes()` conservé comme path direct (bypass pipeline)
- `commit()`, `close()`, `search()` drainent le pipeline d'abord
- Drain = request/reply `PipelineDrainMsg` sur chaque reader, puis sur le router

### Fichier modifié

- `lucivy_core/src/sharded_handle.rs` — messages, acteurs, ShardedHandle fields, spawn

## Optimisation 2 — Background Finalize (FinalizerActor)

### Problème

`SegmentWriter::finalize()` est le coût dominant au commit :
- `SfxCollector.build()` : sort O(E log E) des suffix entries + FST construction
- `remap_and_write()` : sérialisation des posting lists

Pendant ce temps, l'IndexerActor est bloqué et ne peut pas accepter de nouveaux docs.

### Solution

Un **FinalizerActor** (GenericActor spawné dans le scheduler, WASM compat) exécute
`finalize_segment()` en background pendant que l'IndexerActor démarre un nouveau segment.

```
[batch N docs accumulate] ──────────→ [finalize N-1 en background]
[batch N+1 docs accumulate] ─────────→ [finalize N en background]
```

Pipeline depth = 2 : au plus 1 finalize en background + 1 segment en cours d'écriture.

### Fonctionnement

1. Chaque IndexerActor spawne un FinalizerActor dédié au démarrage
2. Quand le mem budget est atteint :
   - Si un finalize est pending → `wait_cooperative()` (attend qu'il finisse)
   - Clone le `DeleteCursor` pour le background
   - Envoie `(Segment, SegmentWriter, DeleteCursor, SegmentUpdater)` au FinalizerActor
   - Démarre un nouveau segment immédiatement
3. Le FinalizerActor exécute `finalize_segment()` et reply Ok/Err
4. Sur Flush/Shutdown : finalize le segment courant (blocking) + attend le pending

### Messages

```rust
struct FinalizeMsg;      // envelope.local: FinalizeWork
struct FinalizeReply;    // ack

struct FinalizeWork {
    segment: Segment,
    writer: SegmentWriter,
    delete_cursor: DeleteCursor,    // cloned
    segment_updater: SegmentUpdater, // Arc-based, cheap clone
}
```

### WASM compat

- Zéro `std::thread::spawn` — tout passe par le scheduler global d'acteurs
- `wait_cooperative()` dans les handlers Flush/Shutdown : le scheduler
  process les messages du FinalizerActor pendant l'attente
- Fonctionne en single-thread coopératif (WASM) et multi-thread (natif)

### Fichier modifié

- `src/indexer/indexer_actor.rs` — FinalizeMsg, FinalizerActor, IndexerState refactoré

## Bench results (release, 5K docs, rag3db clone)

### Indexation

```
                    Avant pipeline    Après pipeline    Gain
1-shard             2.89s             2.86s             ~pareil
TA-4sh              3.05s             1.67s             1.8x
RR-4sh              2.70s             1.87s             1.4x
```

Le 1-shard ne bénéficie pas du pipeline (pas de routing). Le TA-4sh gagne
le plus car le tokenize+hash pour le routing est parallélisé sur 4 readers
et le finalize est pipeliné avec le prochain batch.

### Queries (inchangées)

```
Query                                 Hits    1-shard     TA-4sh     RR-4sh
---------------------------------------------------------------------------
contains 'function'                     20     91.1ms     38.7ms     41.0ms
contains_split 'create index'           20    200.2ms     92.8ms     96.7ms
contains 'segment'                      20     76.8ms     45.8ms     43.7ms
startsWith 'segment'                    20     69.1ms     39.3ms     32.0ms
contains 'rag3db'                       20     82.4ms     46.1ms     42.5ms
startsWith 'rag3db'                     20     77.6ms     45.0ms     37.7ms
contains 'kuzu'                         20     85.3ms     49.9ms     44.9ms
startsWith 'kuzu'                       20     76.1ms     36.7ms     40.0ms
contains 'cmake' (path)                 20      1.8ms      1.6ms      1.7ms
```

### 1318 tests green

51 luciole + 1185 ld-lucivy + 82 lucivy-core.

## Phase 2 — Éliminer la double tokenisation (futur)

Le ReaderActor tokenize une fois et produit un `PreTokenizedDocument` avec
tokens + positions + offsets + hashes. Le SegmentWriter les utilise directement
sans re-tokeniser. Gain estimé : +30-50% additionnel.

## Résumé

| Phase | Description | Gain mesuré | Complexité |
|-------|------------|-------------|------------|
| Phase 1 | Reader pipeline + background finalize | **1.8x indexation** | ~300 lignes |
| Phase 2 | PreTokenizedDocument (zéro double tokenize) | estimé +30-50% | ~400 lignes |
