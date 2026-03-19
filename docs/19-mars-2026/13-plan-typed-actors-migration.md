# Doc 13 — Plan : migration acteurs typés sharded_handle

Date : 19 mars 2026

## Pourquoi

Les 3 acteurs de sharded_handle.rs (Reader, Router, Shard) utilisent
GenericActor<Envelope> avec TypedHandler. Ça empêche :
- Pool::drain() / Pool::shutdown() (besoin de From<DrainMsg>)
- StreamDag avec drain typé
- Pattern match exhaustif (le compilateur ne vérifie pas tous les cas)
- Zero overhead (encode/decode inutile en local)

La migration vers des Actor<Msg=MyEnum> typés permet d'utiliser toutes
les primitives luciole nativement.

## Les 3 acteurs à migrer

### 1. ShardActor (FAIT — défini mais pas branché)

```rust
enum ShardMsg {
    Search { weight: Arc<dyn Weight>, top_k: usize, reply: Reply<Result<...>> },
    Insert { doc: LucivyDocument, pre_tokenized: Option<PreTokenizedData> },
    Commit { fast: bool, reply: Reply<Result<(), String>> },
    Delete { term: Term },
    Drain(DrainMsg),
}
```

Pool<ShardMsg> avec From<DrainMsg> → drain/shutdown natifs.

### 2. RouterActor

```rust
enum RouterMsg {
    Route {
        doc: LucivyDocument,
        node_id: u64,
        hashes: Vec<u64>,
        pre_tokenized: PreTokenizedData,
    },
    Drain(DrainMsg),
}
```

Le router a besoin d'accéder aux `ActorRef<ShardMsg>` des shards.
Capturés à la construction dans le struct RouterActor.

### 3. ReaderActor

```rust
enum ReaderMsg {
    Tokenize { doc: LucivyDocument, node_id: u64 },
    Batch { docs: Vec<(LucivyDocument, u64)> },
    Drain(DrainMsg),
}
```

Le reader a besoin d'envoyer des `RouterMsg::Route` au router.
Capturé à la construction via `ActorRef<RouterMsg>`.

## Ordre de migration (bottom-up)

1. **ShardActor** → `Pool<ShardMsg>` (déjà défini)
2. **RouterActor** → `ActorRef<RouterMsg>` (reçoit Pool<ShardMsg> à la construction)
3. **ReaderActor** → `Pool<ReaderMsg>` (reçoit ActorRef<RouterMsg> à la construction)
4. **ShardedHandle struct** → remplacer les champs
5. **Callers** → adapter search, commit, close, add_document, delete

## Changements par fichier

### sharded_handle.rs — struct

```rust
pub struct ShardedHandle {
    shards: Vec<Arc<LucivyHandle>>,
    shard_pool: Pool<ShardMsg>,           // was Vec<ActorRef<Envelope>>
    reader_pool: Pool<ReaderMsg>,          // was Vec<ActorRef<Envelope>>
    router_ref: ActorRef<RouterMsg>,       // was ActorRef<Envelope>
    router: Arc<Mutex<ShardRouter>>,
    // ... reste inchangé
}
```

### sharded_handle.rs — callers

| Méthode | Avant (Envelope) | Après (typé) |
|---------|-----------------|-------------|
| add_document | `ReaderTokenizeMsg.into_envelope_with_local((doc, id))` | `reader_pool.send(ReaderMsg::Tokenize { doc, id })` |
| add_documents | Batch par reader via Envelope | `reader_pool.worker(i).send(ReaderMsg::Batch { docs })` |
| search | `ShardSearchMsg.into_request_with_local(weight)` | `shard_pool.worker(i).request(\|r\| ShardMsg::Search { weight, top_k, reply: r })` |
| commit | `ShardCommitMsg.into_request()` | `shard_pool.worker(i).request(\|r\| ShardMsg::Commit { fast, reply: r })` |
| close | Same as commit | Same |
| delete | `ShardDeleteMsg.into_envelope_with_local(term)` | `shard_pool.worker(i).send(ShardMsg::Delete { term })` |
| drain_pipeline | PipelineDrainMsg scatter | `reader_pool.drain("readers")` + `router_ref.request(\|r\| RouterMsg::Drain(r))` |

### Ce qui disparaît

- ShardSearchMsg, ShardInsertMsg, ShardCommitMsg, ShardDeleteMsg (Message impls)
- ShardOkReply, ShardSearchReply, PipelineDrainMsg, PipelineDrainReply
- RouterRouteMsg, ReaderTokenizeMsg, ReaderBatchMsg
- Tous les TypedHandler registrations
- Les encode/decode inutiles
- ActorState get/insert (state direct dans le struct)

### Ce qui est créé

- ShardMsg, RouterMsg, ReaderMsg (enums)
- ShardActor, RouterActor, ReaderActor (structs avec Actor impl)
- spawn_shard_pool(), spawn_router(), spawn_reader_pool()

## Estimation

~300 lignes supprimées (GenericActor + Message boilerplate)
~200 lignes ajoutées (Actor impls + enums)
Net: -100 lignes, code plus simple
