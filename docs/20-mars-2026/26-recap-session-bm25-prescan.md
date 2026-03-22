# Doc 26 — Recap session : BM25 global, prescan, ACID, distribué

Date : 22 mars 2026
Branche : `feature/acid-postgres-tests`

## Ce qui a été accompli

### 1. Cleanup complet (branche `lucivyV2/sfx-dag-shards-cleanup`)
- Supprimé ngram_tokenizer (491L), stemmer (201L), fuzzy_substring_automaton (218L), substring_automaton (156L)
- Stemmer retiré de tous Cargo.toml et bindings
- 0 warnings (était 102)
- 1155 tests, 0 fail
- Bench 5K/90K validé avec 5/5 MATCH ground truth sur le kernel Linux

### 2. ACID Postgres (branche `feature/acid-postgres-tests`)
- `PostgresBlobStore` : implémentation BlobStore backed by Postgres bytea
- `BlobDirectory` : skip les .lock files (fix crash recovery)
- 6 tests ACID avec Docker Postgres :
  - create/reopen/search avec highlights
  - verify blobs in Postgres (meta.json, segments)
  - crash recovery (drop sans close, nuke cache, reopen)
  - distributed search 2 nodes
  - distributed ground truth
  - BM25 score consistency (1-shard vs 4-shard)

### 3. Distribué : export_stats + search_with_global_stats
- `ExportableStats` : sérialisable (~60 bytes JSON), avec `contains_doc_freqs`
- `ShardedHandle::export_stats()` : prescan + corpus stats
- `ShardedHandle::search_with_global_stats()` : search avec stats globales + inject contains_doc_freqs
- `ShardedHandle::search_with_docs()` : convenience method (docs + highlights résolus)
- `SearchHit` : struct avec score, doc, highlights
- Protocole : export_stats → merge → search_with_global_stats (1 round-trip)

### 4. BM25 global correct
- Trait methods sur `Query` :
  - `prescan_segments(&mut self, segs)` — SFX walk + cache + count doc_freq
  - `collect_prescan_doc_freqs(&self, out)` — export pour coordinateur
  - `set_global_contains_doc_freqs(&mut self, freqs)` — inject freqs globales
  - `take_prescan_cache(&mut self, out)` — extraire cache
  - `inject_prescan_cache(&mut self, cache)` — injecter cache mergé
- Implémenté par : `SuffixContainsQuery`, `BooleanQuery` (propage)
- Auto-prescan fallback dans `weight()` pour le non-shardé
- Ground truth validé : no-shard = 1-shard = 4-shard = distribué = 0.004963

## Bug en cours : prescan parallèle + contamination

### Symptôme
Le prescan parallèle dans `BuildWeightNode` (via scatter DAG luciole ou
thread::scope) donne des scores corrects pour la PREMIÈRE query, mais les
queries suivantes sont 10-50x plus lentes.

```
contains 'mutex_lock'     85ms  ← OK
contains 'function'     1275ms  ← 15x plus lent !
contains_split          5632ms  ← catastrophique
startsWith 'sched'        92ms  ← OK (pas de prescan)
```

### Hypothèse principale
`inject_prescan_cache` et `set_global_contains_doc_freqs` ne sont pas
dispatchés correctement via le vtable de `Box<dyn Query>`. Le cache du
prescan parallèle n'est pas injecté dans la query → `weight()` fait
l'auto-prescan sur shard_0 seulement → les scorers des shards 1-3 refont
le SFX walk complet.

Mais ça n'explique pas pourquoi la PREMIÈRE query est rapide.

### Hypothèse alternative
Le prescan crée des `PostingResolver` et `SfxFileReader` qui font des
mmap sur les fichiers .sfx/.sfxpost. Les scorers font aussi des mmap
sur les mêmes fichiers. Possible contention mmap entre les lectures
parallèles du prescan et les lectures séquentielles des scorers.

### Ce qui marche
- Mode séquentiel (prescan dans weight()) : correct mais perd le parallélisme shardé
- 1-shard et no-shard : toujours corrects (pas de contention inter-shard)
- Distribué via export_stats + search_with_global_stats : correct

### Prochaines pistes
1. **Investiguer le vtable** : pourquoi inject_prescan_cache ne dispatch pas
   vers SuffixContainsQuery dans certains contextes ?
2. **Downcast explicite** : utiliser `downcast_mut::<SuffixContainsQuery>()` au lieu
   du vtable (on a déjà `downcast_rs::Downcast` sur le trait Query)
3. **Alternative** : ne pas injecter le cache dans la query. Au lieu de ça,
   passer le cache via un `Arc<Mutex<...>>` partagé entre BuildWeightNode et les scorers
4. **Alternative** : stocker le cache dans l'`EnableScoring` (ajout d'un champ)
5. **Tester** : le downcast a fonctionné dans un test précédent (ground truth Postgres).
   Comprendre pourquoi il marche là mais pas dans le bench.

## Architecture actuelle

### DAG de recherche
```
drain → flush → build_weight → search_0..N ∥ → merge
```

### BuildWeightNode (état actuel = scatter prescan)
```
1. extract_contains_terms(config) → termes à prescan
2. build_scatter_dag(N tasks, une par shard)
3. execute_dag(scatter) → N résultats (cache + freqs) en parallèle
4. merge caches + sum freqs
5. build_query(config) → Box<dyn Query>
6. query.set_global_contains_doc_freqs(merged_freqs)  ← vtable bug ?
7. query.inject_prescan_cache(merged_cache)             ← vtable bug ?
8. query.weight(EnableScoring{global_stats}) → Weight
9. Weight.scorer() → lit le cache (si injecté) ou auto-prescan (si pas injecté)
```

### weight() dans SuffixContainsQuery
```rust
fn weight(&self, enable_scoring: EnableScoring) -> Result<Box<dyn Weight>> {
    // 1. Si prescan_cache est Some → utilise le cache + global_doc_freq
    // 2. Sinon auto-prescan depuis enable_scoring.searcher().segment_readers()
    //    (ne voit qu'un seul shard dans le mode shardé)
}
```

### scorer() dans SuffixContainsWeight
```rust
fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
    // 1. Si prescan_cache contient segment_id → lit le cache (fast)
    // 2. Sinon fallback : SFX walk (slow, per-segment IDF)
}
```

## Comment lancer les tests

### Tests unitaires (1155 tests)
```bash
cd packages/rag3db/extension/lucivy/ld-lucivy
cargo test --lib
```

### Bench 5K
```bash
MAX_DOCS=5000 cargo test --release --package lucivy-core --test bench_sharding -- --nocapture
```

### Bench 90K
```bash
MAX_DOCS=90000 cargo test --release --package lucivy-core --test bench_sharding -- --nocapture
```

### Bench avec ground truth verification
```bash
MAX_DOCS=90000 LUCIVY_VERIFY=1 cargo test --release --package lucivy-core --test bench_sharding -- --nocapture
```

### Tests ACID Postgres
```bash
# Démarrer Postgres
docker compose -f docker-compose.test.yml up -d

# Lancer les 6 tests
POSTGRES_URL="host=localhost port=5433 user=test password=test dbname=lucivy_test" \
  cargo test --package lucivy-core --test acid_postgres -- --ignored --nocapture

# Cleanup
docker compose -f docker-compose.test.yml down -v
```

### Test BM25 ground truth (dans les tests ACID)
```bash
POSTGRES_URL="..." cargo test --package lucivy-core --test acid_postgres \
  -- --ignored --nocapture test_bm25_scores_identical
```

### Test distribué ground truth
```bash
POSTGRES_URL="..." cargo test --package lucivy-core --test acid_postgres \
  -- --ignored --nocapture test_distributed_bm25_ground_truth
```

## Benchmarks de référence

### 5K docs (avant prescan, scoring incorrect mais rapide)
```
contains 'mutex_lock'     RR-4sh:  59ms
contains 'function'       RR-4sh:  67ms
contains_split            RR-4sh: 110ms
fuzzy 'schdule'           RR-4sh:  63ms
```

### 5K docs (prescan séquentiel, scoring correct, pas de parallélisme)
```
contains 'mutex_lock'     RR-4sh: 257ms
contains 'function'       RR-4sh: 272ms
contains_split            RR-4sh: 346ms
fuzzy 'schdule'           RR-4sh: 274ms
```

### 90K docs (prescan séquentiel)
```
contains 'mutex_lock'     RR-4sh:  818ms
contains 'function'       RR-4sh:  625ms
contains 'sched'          RR-4sh:  629ms
fuzzy 'schdule'           RR-4sh:  645ms
```

### Objectif
Prescan parallèle avec scores corrects et perf ≈ ancien (60ms/4sh sur 5K).

## Fichiers clés modifiés dans cette session

| Fichier | Rôle |
|---------|------|
| `src/query/query.rs` | Trait methods prescan_segments, inject/take/collect/set |
| `src/query/phrase_query/suffix_contains_query.rs` | prescan(), run_sfx_walk(), CachedSfxResult, SfxCache |
| `src/query/boolean_query/boolean_query.rs` | Propagation prescan aux sous-queries |
| `src/query/mod.rs` | Re-exports (CachedSfxResult, run_sfx_walk, tokenize_query, RawPostingEntry) |
| `lucivy_core/src/search_dag.rs` | BuildWeightNode avec scatter prescan |
| `lucivy_core/src/bm25_global.rs` | ExportableStats + contains_doc_freqs |
| `lucivy_core/src/sharded_handle.rs` | export_stats, search_with_global_stats, search_with_docs |
| `lucivy_core/src/blob_directory.rs` | Skip .lock files in BlobStore |
| `lucivy_core/tests/acid_postgres.rs` | 6 tests ACID + ground truth |
| `examples/distributed_postgres/` | Exemple distribué complet + README |
| `docs/20-mars-2026/17-26` | 10 docs d'architecture/design/diagrammes |

## Décisions architecturales prises

1. **ShardedHandle(4 shards RR) = API publique par défaut** — LucivyHandle reste interne
2. **prescan_segments() trait method** — modèle distribué = modèle de base
3. **BlobDirectory skip .lock** — crash recovery fonctionne
4. **ExportableStats.contains_doc_freqs** — permet le BM25 distribué en 1 round-trip
5. **Proptest dump JSON** — /tmp/proptest_fail.json quand le test flaky échoue
