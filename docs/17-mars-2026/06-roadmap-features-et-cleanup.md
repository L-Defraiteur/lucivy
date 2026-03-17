# Roadmap — Features et Cleanup

Date : 17 mars 2026

## Ce qui est fait

- Token-aware sharding (Phases 1-3) + bench validé
- Scatter-gather BM25 avec stats globales exactes
- Luciole : actor runtime standalone (lib extraite)
- Tous les acteurs migrés vers GenericActor
- Erreurs sérialisables (ActorError\<E\> générique)
- ShardStorage trait (pluggable backends)
- 1317 tests green (51 luciole + 1185 ld-lucivy + 81 lucivy-core)

---

## A. Features (prioritaires)

### A1. BlobShardStorage — ACID persistence
Implémenter `BlobShardStorage<S: BlobStore>` dans rag3weaver.
Mmap local pour reads temps réel + BlobStore pour persistence durable.
Chaque shard a son namespace dans le store (`entity/shard_0/`, etc.).
Design complet dans doc 05.

### A2. Intégration rag3weaver
Le `Catalog` crée un `ShardedHandle` par entity via `create_with_storage(BlobShardStorage)`.
Search cross-entity avec `AggregatedBm25Stats` sur toutes les entities.
Highlights propagés via `Arc<HighlightSink>` partagé.

### A3. Reader actors pipeline — ingestion parallèle
Pool de ReaderActors qui tokenize+hash en parallèle.
Un RouterActor unique qui route séquentiellement (pas de contention).
Les ShardActors écrivent en parallèle (déjà en place).
Design dans doc 03 outro. Pertinent pour >10K docs.

### A4. Luciole Phase 2 — derive macro + postcard
`#[derive(Message)]` pour auto-générer type_tag + encode/decode.
`postcard` comme format de sérialisation (compact, no-std, WASM).
Supprime le boilerplate des impl Message manuels.

### A5. Luciole Phase 3 — état sérialisable
Sérialiser/désérialiser l'ActorState complet.
Migration d'acteur entre threads (load balancing local).
Snapshot d'acteur (debugging, replay).

### A6. Luciole Phase 4 — transport réseau
Trait `Transport` (TCP, QUIC, WebSocket).
`ActorRef::Remote` transparent.
Discovery service.
Extraction complète en repo standalone `luciole`.

### A7. Release build bench
Re-run bench sharding en release sur 5K et 213K docs.
Valider les perfs absolues (debug = ~10x plus lent).
Comparer indexation avec/sans Luciole actors.

### A8. Super-sharding rag3weaver
Le Catalog dispatch les queries cross-entity.
Shard pruning par entity_id au niveau applicatif.
AggregatedBm25Stats cross-entity pour IDF global.
Multi-codebase = multi-index + sharding intra-index.

---

## B. Cleanup (peut attendre)

### B1. Dead code SegmentUpdaterState
5 méthodes unused : `handle_merge_step`, `handle_add_segment`, `enqueue_merge_candidates`, `start_next_incremental_merge`, `finish_incremental_merge`.
La logique a été inlinée dans les handlers GenericActor.
Supprimer ces méthodes — ~100 lignes.

### B2. Supprimer l'ancien trait Actor typé
Le trait `Actor<Msg>` est encore utilisé par les acteurs de test dans `scheduler.rs` et par `GenericActor` lui-même.
Option : le garder comme base trait dans luciole (GenericActor l'implémente).
Option : migrer les tests vers GenericActor et supprimer le trait.
Pas urgent — les deux coexistent sans conflit.

### B3. Unused imports
Warnings `unused_imports` dans indexer_actor.rs, segment_updater_actor.rs, etc.
`cargo fix --lib` peut résoudre la plupart.

### B4. Dead code scoring_utils.rs
Fonctions ngram-related jamais utilisées depuis la suppression de ._raw :
`contains_fuzzy_substring`, `token_match_distance`, `generate_trigrams`, `fold_with_byte_map`, `ngram_threshold`, `intersect_sorted_vecs`, `NGRAM_SIZE`.
~130 lignes à supprimer.

### B5. Bench Cargo.toml warnings
`bench_contains.rs` et `bench_sharding.rs` apparaissent dans deux targets (integration-test + bench).
Fix : supprimer les `[[test]]` entries ou renommer les fichiers.

### B6. Documentation missing_docs warnings
~60 warnings `missing_docs` dans le module actor (maintenant dans luciole).
Ajouter des doc comments ou `#[allow(missing_docs)]` dans luciole.

### B7. Mettre à jour CLAUDE.md
Le CLAUDE.md mentionne encore les ._raw fields, l'ancien système d'acteurs typés, etc.
À mettre à jour avec : Luciole, ShardedHandle, ShardStorage, GenericActor.
