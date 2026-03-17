# Récap session 17 mars 2026

## Commits à pusher (24 commits sur feature/sfx-unified)

Depuis `02ead34` (dernier push) jusqu'à `f58a14f`.

## Ce qui a été fait

### Luciole — Actor runtime standalone
- Conçu et implémenté le système d'acteurs générique "Luciole"
- Envelope (type_tag FNV-1a + payload bytes + local opaque + ReplyPort)
- ActorState (resource bag HashMap<TypeId, Box<dyn Any>>)
- Handler trait + TypedHandler (auto-deserialize, closures typées)
- GenericActor (dispatch dynamique, handlers ajoutés/retirés à runtime)
- TypedActorRef (façade typée, encode/decode transparent)
- ActorError\<E\> générique (pas couplé à LucivyError)
- priority_fn dynamique (basée sur l'état de l'acteur)
- on_start auto-store self_ref dans ActorState (pour self-messages)
- **Extrait en lib standalone `luciole/` v0.1.0** dans le workspace
  - Dépendances : flume + std, c'est tout
  - 51 tests propres
  - README + LICENSE MIT
- `impl Message for LucivyError` dans `src/error_message.rs` (bridge, hors de luciole)

### Migration des acteurs
- **ShardActor** → GenericActor (4 rôles : search, insert, commit, delete)
- **IndexerActor\<D\>** → GenericActor (3 rôles : docs, flush, shutdown)
  - Type D capturé par closure (type erasure)
  - IndexWriter utilise ActorRef\<Envelope\> au lieu de ActorRef\<IndexerMsg\>
- **SegmentUpdaterActor** → GenericActor (7 rôles : addSegment, commit, GC, startMerge, mergeStep, drainMerges, kill)
  - Self-messages MergeStep via on_start self_ref
  - Fix : MergeStep handler démarre les pending merges si aucun merge actif
- Erreurs sérialisables : ReplyPort envoie Result\<Vec\<u8\>, Vec\<u8\>\>
- Plus aucun acteur typé en production

### ShardStorage trait
- `ShardStorage` : abstraction pour backends pluggables
- `FsShardStorage` : filesystem (défaut)
- `BlobShardStorage<S: BlobStore>` : ACID (mmap cache + BlobStore durable)
  - Testé E2E avec MemBlobStore (create → insert → search → close → reopen)
- `ShardedHandle.create_with_storage()` / `open_with_storage()`
- ShardedHandle n'a plus de `base_path`, utilise `Box<dyn ShardStorage>`

### Optimisation startsWith
- **SI=0 runtime filter** : startsWith route vers SuffixContainsQuery avec prefix_only=true
  - Filtre les parent entries SI>0 sans les résoudre
  - Gain : startsWith 'segment' 196ms → 62ms (3.2x)
- **Prefix byte partitioning** du suffix FST :
  - Entrées SI=0 préfixées \x00, SI>0 préfixées \x01
  - Le FST range scan skip nativement les entrées de l'autre partition
  - PrefixByteAutomaton : wrapper DFA qui accepte le prefix byte puis délègue
  - PrefixByteContinuationAutomaton : idem pour search_continuation
  - prefix_walk / fuzzy_walk mergent les deux partitions par clé (contains)
  - prefix_walk_si0 / fuzzy_walk_si0 ne walkent que \x00 (startsWith)
  - resolve_suffix / resolve_suffix_si0 pour les lookups exacts
  - Gain additionnel : contains 'segment' 54ms → 41ms, startsWith 62ms → 43ms

### Bench results (release, 5K docs, 4 shards TA)

```
Index time: 1-shard 2.89s | TA-4sh 3.05s | RR-4sh 2.70s

Query                                 Hits    1-shard     TA-4sh     RR-4sh
---------------------------------------------------------------------------
contains 'function'                     20     85.3ms     42.3ms     43.7ms
contains_split 'create index'           20    170.9ms     74.4ms     85.4ms
contains 'segment'                      20     78.0ms     40.8ms     44.4ms
startsWith 'segment'                    20     77.8ms     42.7ms     41.0ms
contains 'rag3db'                       20     88.9ms     47.0ms     41.3ms
startsWith 'rag3db'                     20     81.1ms     43.8ms     44.6ms
contains 'kuzu'                         20     77.6ms     36.1ms     33.4ms
startsWith 'kuzu'                       20     77.8ms     39.1ms     41.9ms
contains 'cmake' (path)                 20      3.0ms      1.7ms      1.7ms
```

### Tests
- 51 luciole + 1185 ld-lucivy + 82 lucivy-core = **1318 tests green**

## Docs créés aujourd'hui (docs/17-mars-2026/)

1. `01-design-generic-actor-system-luciole.md` — Vision Luciole
2. `02-implementation-plan-luciole-phase1.md` — Plan d'implémentation Phase 1
3. `03-migration-strategy-all-actors-to-luciole.md` — Stratégie migration + reader actors pipeline
4. `04-luciole-serializable-errors.md` — Design erreurs sérialisables
5. `05-design-shard-storage-trait-acid.md` — ShardStorage trait pour ACID
6. `06-roadmap-features-et-cleanup.md` — Roadmap features (A1-A8) + cleanup (B1-B7)
7. `07-optim-startswith-si0-filter.md` — SI=0 filter design
8. `08-optim-suffix-fst-prefix-byte-partitioning.md` — Prefix byte design (implémenté)
9. `09-bench-baseline-before-prefix-byte.md` — Baseline bench

## Ce qui reste à faire (par priorité)

### Features prochaines
- **A3 : Reader actors pipeline** — ingestion parallèle (tokenize en pool, route séquentiel)
- **A4 : Luciole Phase 2** — derive macro `#[derive(Message)]` + postcard
- **A5 : Luciole Phase 3** — état sérialisable, migration d'acteur
- **A6 : Luciole Phase 4** — transport réseau, distributed actors
- **Intégration rag3weaver** — le Catalog utilise ShardedHandle + BlobShardStorage

### Cleanup (peut attendre)
- B1 : Dead code SegmentUpdaterState (5 méthodes unused)
- B2 : Supprimer ancien trait Actor typé ou garder pour compat
- B3 : Unused imports (cargo fix)
- B4 : Dead code scoring_utils.rs (~130 lignes ngram)
- B5 : Bench Cargo.toml warnings (double targets)
- B6 : missing_docs warnings dans luciole
- B7 : Mettre à jour CLAUDE.md
- Supprimer ContinuationAutomaton legacy (remplacé par PrefixByteContinuationAutomaton)

## Rappels importants
- **Ne PAS mentionner Claude** dans les commits
- **lucivy est sa propre lib** — ne jamais dire "fork de Tantivy"
- **Pas de concessions** — corriger les bugs, pas les rationaliser
- Le bench release 213K docs n'a pas encore été fait (estimé ~2min indexation)
- Le prefix byte est un **format break** du .sfx — les anciens index ne sont plus lisibles
