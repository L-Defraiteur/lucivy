# Plan — Playground shardé + demo Linux kernel

## Vision

Showcaser la recherche sur 90K fichiers du Linux kernel dans le playground
browser, avec des temps de réponse < 100ms grâce au sharded handle parallèle.

## État actuel

### Bench existant (bench_sharding.rs)

Le bench indexe ~90K fichiers du Linux kernel en 3 modes :
- **Single shard** : `/home/luciedefraiteur/lucivy_bench_sharding/single/`
- **Token-aware 4-shard** : `/home/luciedefraiteur/lucivy_bench_sharding/token_aware/`
- **Round-robin 4-shard** : `/home/luciedefraiteur/lucivy_bench_sharding/round_robin/`

Distribution RR : ~22500 docs/shard. Score consistency : 5/5 diff=0.0000.

### Playground actuel

- Binding **emscripten** avec `LucivyHandle` (single shard)
- 4308 docs → ~300ms pour "rag3db" d=1 (après drain_merges, 1 segment)
- Threading : PTHREAD_POOL_SIZE=8, scheduler_threads=4
- Import via file drop ou git clone (GitHub API)

### Snapshot .luce

Format binaire simple (LUCE magic + fichiers sérialisés). Existe pour
single-shard (`export_index` / `import_index`). Pas encore de variant shardé.

## Ce qui manque

### 1. ShardedHandle pour emscripten

Le binding emscripten utilise `LucivyHandle` directement. Il faut migrer
vers `ShardedHandle` pour bénéficier du DAG parallèle.

**Problèmes à résoudre :**

- **Storage** : le playground utilise `MemoryDirectory` (pas de filesystem).
  `FsShardStorage` crée des sous-dossiers sur le FS. Pour emscripten,
  il faut soit :
  - (a) Utiliser le FS emscripten (MEMFS) comme backend → `FsShardStorage`
    fonctionne tel quel car emscripten émule un FS POSIX
  - (b) Créer un `MemoryShardStorage` qui wrappe N `MemoryDirectory`

  L'option (a) est la plus simple et devrait marcher car le binding
  emscripten utilise déjà un `VfsDirectory` (wrapper autour du FS emscripten).

- **Thread pool** : le DAG utilise `luciole::Pool` + `luciole::StreamDag`.
  luciole utilise `std::thread::spawn` pour les workers. En emscripten,
  les threads sont des pthreads (fonctionnels, déjà testés avec le commit
  async). Le pool size doit tenir dans PTHREAD_POOL_SIZE=8.

  Avec 4 shards + 1 router + N readers, il faut :
  - 4 shard actors (persistent)
  - 1 router actor (persistent)
  - 2-4 reader actors (persistent)
  - Total : 7-9 threads persistants → PTHREAD_POOL_SIZE=8 pourrait être juste

  **Solution** : augmenter PTHREAD_POOL_SIZE à 12, ou réduire à 2 shards
  pour le playground.

- **API binding** : le binding emscripten expose des fonctions `extern "C"`
  qui opèrent sur un `LucivyContext { handle: LucivyHandle, ... }`.
  Il faut soit :
  - (a) Ajouter un `ShardedLucivyContext` séparé avec ses propres fonctions
  - (b) Rendre `LucivyContext` générique (enum Single/Sharded)
  - (c) Toujours utiliser ShardedHandle, même avec 1 shard

  L'option (c) est la plus simple : `ShardedHandle::create(path, config)`
  avec `config.shards = Some(4)` crée un sharded handle. Avec
  `config.shards = None` ou `Some(1)`, c'est un single shard.

### 2. Export .luce shardé

Le format .luce actuel sérialise un seul index (1 set de fichiers segments).
Pour un sharded handle, il faut sérialiser N shards + la config.

**Format proposé :**
```
LUCE v2 :
  [4] magic "LUCE"
  [4] version: 2
  [4] num_shards: u32
  [var] shard_config_json (de _shard_config.json)
  [var] shard_stats_bin (de _shard_stats.bin)
  For each shard:
    [4] num_files: u32
    For each file:
      [4] name_len + name + [4] data_len + data
```

Le format v1 actuel n'a qu'un seul "index" avec N fichiers. On peut
garder la compat en détectant la version.

**Fonctions à ajouter :**
- `export_sharded_snapshot(handle: &ShardedHandle) -> Vec<u8>`
- `import_sharded_snapshot(data: &[u8], storage: &dyn ShardStorage) -> ShardedHandle`

### 3. Pipeline : indexer Linux → exporter → importer dans le playground

```
1. cargo test bench_sharding_comparison (ou script dédié)
   → indexe 90K docs en round_robin 4-shard
   → /home/luciedefraiteur/lucivy_bench_sharding/round_robin/

2. Script Rust : export_sharded_snapshot()
   → linux_kernel.luce (~150-200 MB estimé)

3. Héberger le .luce (GitHub release, S3, ou local)

4. Playground : bouton "Load Linux kernel demo"
   → fetch le .luce
   → import_sharded_snapshot() dans le binding emscripten
   → prêt à chercher
```

**Taille estimée du .luce :**
- 4 shards × ~22500 docs
- Par shard : SFX ~7MB + SfxPost ~5MB + PosMap ~18MB + autres ~5MB = ~35MB
- Total : ~140MB brut
- Compressé (gzip) : ~40-60MB (estimé, les FST compressent bien)

## Étapes ordonnées

### Phase 1 : ShardedHandle dans emscripten (prioritaire)

1. Modifier `LucivyContext` pour contenir un `ShardedHandle` au lieu de
   `LucivyHandle`
2. Adapter les fonctions `lucivy_create`, `lucivy_open_*`, `lucivy_add`,
   `lucivy_commit`, `lucivy_search` pour utiliser `ShardedHandle`
3. Ajouter `shards` dans la config JS (`create` avec `{ shards: 4 }`)
4. Ajuster PTHREAD_POOL_SIZE (12 ou 16)
5. Tester sur le repo rag3db (4308 docs, 4 shards) → objectif < 100ms
6. Rebuild + test playground

### Phase 2 : Export/import .luce shardé

7. Ajouter `export_sharded_snapshot()` dans `snapshot.rs`
8. Ajouter `import_sharded_snapshot()` dans `snapshot.rs`
9. Exposer dans le binding emscripten :
   `lucivy_export_sharded_snapshot`, `lucivy_import_sharded_snapshot`
10. Tester : export le bench RR → import dans emscripten → search

### Phase 3 : Demo Linux kernel

11. Script pour exporter le bench RR en .luce shardé
12. Héberger le .luce (GitHub release ou CDN)
13. Bouton dans le playground : "Load Linux kernel (90K files)"
14. Streaming import : progress bar pendant le fetch + import
15. Benchmark dans le browser : afficher le temps de recherche

## Risques

- **Taille mémoire** : 140MB de .luce + index en mémoire. Le browser a
  typiquement 2-4GB de heap WASM. Devrait passer.
- **Thread pool exhaustion** : 4 shards + pipeline = 8-10 threads.
  PTHREAD_POOL_SIZE=8 risque d'être juste. Monitorer.
- **Import time** : 140MB de fichiers à désérialiser + ouvrir N indexes.
  Estimé 5-10s. Acceptable pour une demo.
- **Latence réseau** : télécharger 40-60MB compressé. Avec une bonne
  connexion, 5-15s. Avec 3G, inutilisable. Prévoir un fallback plus petit.

## Quick wins en attendant

- **Commit le drain_merges** dans le binding emscripten (déjà codé)
- **Supprimer le log `[fuzzy-contains]`** (bruit dans la console)
- **Augmenter COMMIT_EVERY** dans le playground (200 → 1000) pour réduire
  le nombre de segments avant drain
