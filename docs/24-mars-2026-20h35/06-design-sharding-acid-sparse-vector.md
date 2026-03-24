# Doc 06 — Design : porter sharding + ACID de lucivy vers sparse_vector

Date : 24 mars 2026

## Contexte

`sparse_vector` est un moteur de recherche sparse (SPLADE/BM25 vectoriel)
indépendant de lucivy. Il a son propre `SparseHandle` avec :
- Insert / Remove / Search / Search_filtered
- Mmap persistence (flat binary format)
- BlobStore support (même trait que lucivy, ré-exporté depuis lucivy_core)
- WAND pruning pour le search

**Ce qui manque** : sharding, ACID, parallélisme. Actuellement c'est un single
index derrière un `Mutex<Inner>`.

## Architecture actuelle sparse_vector

```
SparseHandle
  ├── Inner (Mutex)
  │   ├── SparseIndex (inverted index RAM)
  │   ├── MmapPostingData (mmap on-disk)
  │   ├── dirty flag
  │   └── postings_loaded / vectors_loaded (lazy)
  ├── path (filesystem ou cache tmpdir)
  └── StorageBackend (Filesystem | BlobStore)
```

**Fichiers sur disque** :
- `sparse.mmap` — posting lists (flat binary, 16 bytes/entry)
- `sparse_vectors.bin` — vecteurs originaux (bincode)
- `sparse_dims.bin` — dim mapping (bincode)

**API** :
```rust
handle.insert(node_id, &sparse_vector)?;
handle.remove(node_id)?;
handle.search(&query_vector, top_k) -> Vec<(node_id, score)>
handle.search_filtered(&query, top_k, &allowed_ids) -> Vec<(node_id, score)>
handle.commit_inner()?;
```

## Ce qu'on peut porter depuis lucivy

### 1. ShardedSparseHandle — parallélisme search

Le pattern est identique à `ShardedHandle` de lucivy :

```rust
pub struct ShardedSparseHandle {
    shards: Vec<Arc<SparseHandle>>,
    num_shards: usize,
}

impl ShardedSparseHandle {
    pub fn insert(&self, node_id: u64, vector: &SparseVector) {
        let shard_id = (node_id as usize) % self.num_shards;
        self.shards[shard_id].insert(node_id, vector);
    }

    pub fn search(&self, query: &SparseVector, top_k: usize) -> Vec<(u64, f32)> {
        // Scatter: search chaque shard en parallèle
        // Gather: merge top-K par binary heap
    }
}
```

**Différence avec lucivy** : pas besoin de prescan IDF (sparse scoring est
dot product, pas BM25). Donc pas de `BuildWeightNode` — juste fan-out search + merge.

Le DAG search sparse serait :
```
search_shard_0 ──┐
search_shard_1 ──┼── merge_top_k
search_shard_2 ──┘
```

Beaucoup plus simple que le DAG lucivy. On pourrait utiliser `fan_out_merge()` directement.

### 2. ACID via BlobStore

`SparseHandle` a DÉJÀ le support BlobStore (`create_with_store`, `open_with_store`).
Le commit écrit dans le BlobStore, l'open matérialise depuis le BlobStore vers un cache mmap.

Ce qui manque :
- **WAL** (Write-Ahead Log) pour crash recovery entre les commits
- **Snapshot isolation** : les readers voient un état consistent pendant les writes

Pour le WAL, le pattern serait le même que lucivy (commit atomique via BlobStore).
Le sparse index est plus simple : un seul fichier mmap + 2 side files.
Le commit est atomique si on écrit les 3 fichiers puis met à jour un manifest.

### 3. Parallélisme via luciole

Actuellement `SparseHandle` est single-threaded (Mutex<Inner>).
Avec luciole :

```rust
// Shard pool pour l'indexation
let shard_pool: Pool<SparseShardMsg> = Pool::spawn(num_shards, 64, |i| {
    SparseShardActor { handle: shards[i].clone() }
});

// Search DAG parallèle
dag.fan_out_merge("sparse_search", num_shards,
    |i| Box::new(SparseSearchNode::new(shards[i].clone(), query.clone(), top_k)),
    "hits",
    |results| merge_sparse_top_k(results, top_k),
)?;
```

### 4. Incremental sync (réutilise le design doc 05)

Le sparse index est 3 fichiers. Le delta serait :
- Nouveau `sparse.mmap` (complet — il est regénéré à chaque commit)
- OU : delta des vecteurs ajoutés/supprimés

Problème : contrairement à lucivy (segments immutables), le sparse index
rewrite tout le fichier mmap à chaque commit. Pas de "segments" à différ.

**Solutions** :
- A. Envoyer le mmap complet à chaque sync (simple, sparse mmap est compact)
- B. Segmenter le sparse index comme lucivy (plus complexe, gros refactor)
- C. Diff binaire du mmap (xdelta, bsdiff) — efficace si peu de changements

Option A est pragmatique pour les petits index (<50MB sparse).
Option B serait nécessaire pour les gros index mais c'est un gros chantier.

## Plan de convergence

### Phase 1 : ShardedSparseHandle (via luciole)

```
Fichier: sparse_vector/rust/src/sharded_handle.rs (nouveau)

- ShardedSparseHandle { shards: Vec<Arc<SparseHandle>> }
- Round-robin insert (node_id % num_shards)
- Parallel search via fan_out_merge ou thread::scope
- Merge top-K par binary heap
```

**Pas besoin de luciole DAG** pour le search sparse — c'est assez simple pour
un `thread::scope` ou `ScatterDAG`. Pas de prescan, pas de weight compilation.

Mais si on veut l'intégrer dans un DAG hybrid (FTS + sparse + vector), alors
les search nodes sparse seraient des noeuds dans le DAG rag3weaver.

### Phase 2 : BlobStore ACID

Déjà supporté. Ajouter :
- Manifest file pour commit atomique des 3 fichiers
- Crash recovery : lire le manifest, ignorer les fichiers partiels

### Phase 3 : Intégration rag3weaver

Le `Catalog` de rag3weaver gère déjà des `SparseHandle` par entity.
Le sharding se ferait au niveau du `Catalog`, pas du `SparseHandle` :

```
Catalog {
    entities: {
        "Document" → {
            fts_index: ShardedHandle (lucivy),          // 4 shards
            sparse_index: ShardedSparseHandle,          // 4 shards, même node_id routing
            vector_index: /* future */,
        }
    }
}
```

Le routing par `node_id` assure que le même doc est dans le même shard
pour FTS et sparse → les filtered searches restent locales par shard.

### Phase 4 : DAG hybrid search

```
query → ┬── fts_search (lucivy DAG)     ──┐
        ├── sparse_search (fan-out)      ──┼── fuse_results → rerank → top_k
        └── vector_search (HNSW/faiss)   ──┘
```

Les 3 moteurs cherchent en parallèle, les résultats sont fusionnés.
C'est exactement le use case de rag3weaver Phase 4 (doc 03).

## Comparaison SparseHandle vs LucivyHandle

| Aspect | LucivyHandle | SparseHandle |
|--------|-------------|-------------|
| Format | Segments (immutables, WORM) | Single file (rewrite à chaque commit) |
| Search | BM25 via inverted index | Dot product via posting lists |
| IDF | Global across segments/shards | Pas d'IDF (score = dot product) |
| Prescan | Nécessaire pour SFX queries | Pas nécessaire |
| Sharding bénéfice | Search parallèle + correct IDF | Search parallèle seulement |
| BlobStore | Oui (BlobDirectory) | Oui (SparseHandle.StorageBackend) |
| Delta sync | Naturel (segments immutables) | Difficile (rewrite complet) |

## Fichiers à créer/modifier

| Fichier | Changement |
|---------|-----------|
| `sparse_vector/rust/src/sharded_handle.rs` | Nouveau : ShardedSparseHandle |
| `sparse_vector/rust/src/lib.rs` | Export sharded_handle |
| `sparse_vector/Cargo.toml` | Dep luciole (optionnel, pour Pool/ScatterDAG) |
