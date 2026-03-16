# Next: Indexation Performance + ShardActor — 16 mars 2026

## Constat

Sur 5K docs (debug build, 4 shards) :
- **Search : 300ms** (TA-4sh) — rapide, quasi-linéaire avec le nombre de shards
- **Indexation : 20s** — le bottleneck

L'indexation est 10-20x plus lente que la recherche. C'est le prochain axe.

## Axes d'optimisation

### 1. Batch insert par shard
Actuellement on lock/unlock le writer Mutex à chaque `add_document`. Avec 5K docs ça fait 5K lock/unlock. Bufferiser par shard et flusher en batch élimine la contention.

### 2. Paralléliser l'insertion via ShardActor
Le tokenize+hash est fait côté caller (séquentiel car le router a besoin de l'état précédent). Mais l'écriture dans l'index peut être parallélisée : chaque shard a son propre writer, pas de contention cross-shard.

### 3. Commit spacing
Le bench shardé fait 1 seul commit à la fin. Le single fait 1 commit/1000 docs. Ajuster pour comparer équitablement (le commit trigger le merge, qui est coûteux).

### 4. Release build
Debug amplifie tout (bounds checks, no inlining, no SIMD). Le vrai bench doit être en release.

## Design : ShardActor (remplace ShardSearchActor)

Actuellement `ShardSearchActor` ne fait que du search. Pour paralléliser l'insertion, il faut un acteur qui gère **tout** pour son shard.

```rust
enum ShardMsg {
    /// Single document insert (routed by ShardedHandle).
    Insert {
        doc: LucivyDocument,
        reply: Reply<Result<(), String>>,
    },
    /// Batch insert (buffered docs flushed at once).
    InsertBatch {
        docs: Vec<LucivyDocument>,
        reply: Reply<Result<(), String>>,
    },
    /// Execute pre-compiled Weight on this shard's segments.
    Search {
        weight: Arc<dyn Weight>,
        top_k: usize,
        reply: Reply<Result<Vec<(f32, DocAddress)>, String>>,
    },
    /// Commit pending writes.
    Commit {
        reply: Reply<Result<(), String>>,
    },
    /// Delete by term.
    Delete {
        term: Term,
        reply: Reply<Result<(), String>>,
    },
}
```

### Architecture

```
ShardedHandle
  ├── ShardRouter (tokenize + route)
  ├── ShardActor[0] ← owns Arc<LucivyHandle>, buffer Vec<Doc>
  ├── ShardActor[1]
  ├── ShardActor[2]
  └── ShardActor[3]
```

### Flux d'insertion

1. `ShardedHandle::add_document(doc, node_id)` :
   - Tokenize text fields → hash tokens
   - `router.route(hashes)` → shard_id
   - `actor_refs[shard_id].send(ShardMsg::Insert { doc, reply })`
   - Non-bloquant : le doc est envoyé dans la mailbox, pas écrit tout de suite

2. `ShardActor::handle(Insert)` :
   - Push doc dans `self.buffer`
   - Si `buffer.len() >= BATCH_SIZE` : flush dans le writer
   - Reply OK

3. `ShardedHandle::commit()` :
   - Envoie `ShardMsg::Commit` à chaque acteur
   - Chaque acteur flush son buffer + commit le writer
   - Attend les N replies

### Avantages

- **Insert parallèle** : 4 shards = 4 writers en parallèle, zéro contention
- **Batch writes** : 1 lock + N docs au lieu de N locks
- **WASM compatible** : même code, scheduler coopératif traite séquentiellement
- **Un seul acteur par shard** : plus simple que acteur search + acteur insert
- **Le caller ne bloque plus sur le writer lock** : fire-and-forget

### Impact sur le search

Aucun changement conceptuel. Le search envoie `ShardMsg::Search` au même acteur. L'acteur fait un auto-flush du buffer avant le search si des docs sont en attente (lazy commit pattern).

### Impact sur le delete

`ShardMsg::Delete` envoyé au bon acteur (via node_id → shard_id mapping). L'acteur fait `writer.delete_term()` localement.

## Lien avec le super-sharding rag3weaver

Le `ShardActor` est l'unité de travail. Au niveau rag3weaver :
- Le Catalog crée un `ShardedHandle` par entity
- Chaque `ShardedHandle` a N `ShardActor`s
- Le Catalog peut dispatch les inserts cross-entity en parallèle
- Le search cross-entity agrège les résultats de plusieurs `ShardedHandle`s

Le `AggregatedBm25Stats` fonctionne aussi cross-entity : on peut agréger les searchers de plusieurs `ShardedHandle`s pour un scoring global. Même pattern scatter-gather, juste un niveau au-dessus.

## Estimation

- Refacto ShardSearchActor → ShardActor : ~100 lignes modifiées
- Batch buffer + auto-flush : ~30 lignes
- Tests : adapter les 4 tests existants
- Bench : re-run pour mesurer le gain
