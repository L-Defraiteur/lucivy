# Doc 09 — Design : lucistore — crate partagée persistance et sync

Date : 24 mars 2026

## Motivation

Aujourd'hui la persistance (BlobStore, shard storage, sync) est dans `lucivy_core`.
Mais `sparse_vector` a besoin des mêmes primitives — et bientôt d'autres moteurs aussi
(vector HNSW, etc.). Dupliquer ce code dans chaque crate n'est pas viable.

**lucistore** = crate indépendante de tout moteur, qui fournit :
- Stockage blob abstrait (DB, S3, mémoire, filesystem)
- Gestion des shards (routing, storage, config)
- Formats d'archive binaires (LUCE, LUCID, LUCIDS)
- Serveur de sync (historique de versions, delta dispatch)

## Ce qui bouge dans lucistore

### 1. BlobStore (`blob_store.rs`)

Déjà un trait propre, zéro dépendance lucivy :

```rust
pub trait BlobStore: Send + Sync + 'static {
    fn load(&self, index_name: &str, file_name: &str) -> io::Result<Vec<u8>>;
    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()>;
    fn delete(&self, index_name: &str, file_name: &str) -> io::Result<()>;
    fn exists(&self, index_name: &str, file_name: &str) -> io::Result<bool>;
    fn list(&self, index_name: &str) -> io::Result<Vec<String>>;
}
```

Inclut `MemBlobStore` pour les tests.

**Migration** : copier `lucivy_core/src/blob_store.rs` → `lucistore/src/blob_store.rs`.
Zéro changement.

### 2. BlobCache (`blob_cache.rs`)

Pattern "DB stocke, mmap sert" extrait en générique. C'est le cœur de l'actuel
`BlobDirectory`, mais sans dépendre d'aucun trait `Directory` spécifique.

```rust
/// Cache local matérialisé depuis un BlobStore.
///
/// Gère le cycle : matérialiser → cache local → write-through → cleanup.
/// Les consommateurs wrappent BlobCache pour implémenter leur propre trait I/O.
pub struct BlobCache<S: BlobStore> {
    store: Arc<S>,
    /// Nom prefixé dans le BlobStore (ex: "Lucivy_Product", "Sparse_Product").
    prefixed_name: String,
    /// Répertoire local de cache (mmap-capable).
    cache_dir: Arc<PathBuf>,
}

impl<S: BlobStore> BlobCache<S> {
    /// Crée un nouveau cache. Matérialise tous les blobs existants.
    pub fn new(
        store: Arc<S>,
        prefix: &str,
        name: &str,
        cache_base: &Path,
    ) -> io::Result<Self> { ... }

    /// Chemin du répertoire local de cache.
    pub fn cache_path(&self) -> &Path { &self.cache_dir }

    /// Accès au store sous-jacent.
    pub fn store(&self) -> &S { &self.store }

    /// Nom prefixé (pour les clés BlobStore).
    pub fn prefixed_name(&self) -> &str { &self.prefixed_name }

    /// Re-matérialiser tous les blobs (après un sync delta par exemple).
    pub fn materialize(&self) -> io::Result<()> { ... }

    /// Écrire dans le cache ET dans le store (write-through).
    pub fn write_through(&self, file_name: &str, data: &[u8]) -> io::Result<()> { ... }

    /// Supprimer du cache ET du store.
    pub fn delete_through(&self, file_name: &str) -> io::Result<()> { ... }

    /// Lire depuis le cache local (fast path, mmap-capable).
    pub fn read_cached(&self, file_name: &str) -> io::Result<Vec<u8>> { ... }

    /// Lister les fichiers dans le cache local.
    pub fn list_cached(&self) -> io::Result<Vec<String>> { ... }
}

impl<S: BlobStore> Drop for BlobCache<S> {
    fn drop(&mut self) {
        // Cleanup du cache dir quand le dernier Arc est droppé.
    }
}
```

**Usage par lucivy** :

```rust
// lucivy_core/src/blob_directory.rs
pub struct BlobDirectory<S: BlobStore> {
    cache: lucistore::BlobCache<S>,
    inner: StdFsDirectory,  // ouvert sur cache.cache_path()
    watch_router: Arc<RwLock<WatchCallbackList>>,
}

impl<S: BlobStore> ld_lucivy::directory::Directory for BlobDirectory<S> {
    fn open_read(&self, path: &Path) -> ... { self.inner.open_read(path) }
    fn atomic_write(&self, path: &Path, data: &[u8]) -> ... {
        self.cache.write_through(path.to_str().unwrap(), data)?;
        // + notify watchers
    }
    // ...
}
```

**Usage par sparse_vector** :

```rust
// sparse_vector/src/blob_storage.rs
pub struct SparseBlobStorage<S: BlobStore> {
    cache: lucistore::BlobCache<S>,
}

impl<S: BlobStore> SparseBlobStorage<S> {
    pub fn mmap_path(&self) -> PathBuf {
        self.cache.cache_path().join("sparse.mmap")
    }
    pub fn commit(&self, mmap_data: &[u8]) -> io::Result<()> {
        self.cache.write_through("sparse.mmap", mmap_data)
    }
}
```

Le même `BlobCache<S>` sert les deux moteurs. Chacun wrappe avec ses propres méthodes.

### 3. ShardStorage (`shard_storage.rs`)

Le trait actuel retourne des `LucivyHandle` — trop couplé.

**Nouveau design** : séparer en deux niveaux.

```rust
/// Niveau lucistore : gestion des répertoires de shards.
/// Ne sait rien des handles concrets.
pub trait ShardStorage: Send + Sync {
    /// Chemin (ou identifiant) du répertoire d'un shard.
    fn shard_path(&self, shard_id: usize) -> String;

    /// Écrire un fichier au niveau racine (ex: _shard_config.json).
    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String>;

    /// Lire un fichier racine.
    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String>;

    /// Vérifier l'existence d'un fichier racine.
    fn root_file_exists(&self, name: &str) -> bool;
}
```

Implémentations dans lucistore :
- **`FsShardStorage`** — filesystem, `base_path/shard_{id}/`
- **`BlobShardStorage<S: BlobStore>`** — blob store, `{index_name}/shard_{id}`

Les fonctions `create_shard_handle` / `open_shard_handle` restent côté consommateur
(lucivy_core, sparse_vector) car elles retournent des types spécifiques.

### 3. Formats d'archive

#### LUCE — snapshot complet (`snapshot.rs`)

Format binaire pour exporter/importer N index complets.
Déjà 100% générique (`export_snapshot`, `import_snapshot`, `SnapshotIndex`).

Ce qui bouge dans lucistore :
- `export_snapshot(indexes: &[SnapshotIndex]) -> Vec<u8>`
- `import_snapshot(data: &[u8]) -> Result<Vec<ImportedIndex>>`
- `SnapshotIndex`, `ImportedIndex` structs
- `read_directory_files(path)` — helper filesystem

Ce qui reste dans lucivy_core :
- `export_index(handle, path)` — appelle `check_committed` + `export_snapshot`
- `import_index(data, path)` — appelle `import_snapshot` + `LucivyHandle::open`

#### LUCID — delta single shard (`sync.rs`)

Format binaire pour delta incrémental.
Les structs et la sérialisation sont 100% génériques.

Ce qui bouge dans lucistore :
- `IndexDelta`, `SegmentBundle` structs
- `serialize_delta` / `deserialize_delta`
- `compute_version_from_bytes` (FNV-1a hash)
- `write_string` / `read_string` / `read_u32` helpers

Ce qui reste dans lucivy_core (dépend de `Index`, `SegmentMeta`) :
- `export_delta(handle, path, client_ids, version)` — lit les segment files
- `apply_delta(path, delta)` — peut aller dans lucistore (pur filesystem)
- `compute_version(handle)` — wrapper qui lit meta.json via `Index`
- `segment_ids_from_meta(bytes)` — parse le meta.json lucivy

**Note** : `apply_delta` est en fait générique (écrit/supprime des fichiers, rename
meta.json). Il peut aller dans lucistore. `segment_ids_from_meta` aussi — c'est du
JSON parsing, pas de type lucivy.

#### LUCIDS — delta multi-shard

Même logique :
- `ShardedDelta`, `ShardVersion` structs → lucistore
- `serialize_sharded_delta` / `deserialize_sharded_delta` → lucistore
- `apply_sharded_delta(base_path, delta)` → lucistore
- `compute_shard_versions(base_path, num_shards)` → lucistore
- `export_sharded_delta(...)` → reste dans lucivy_core (dépend de `LucivyHandle`)

### 4. SyncServer (`sync_server.rs`)

100% générique — ne connaît pas les handles. Travaille avec des versions (strings)
et des segment IDs (HashSet<String>).

Ce qui bouge dans lucistore :
- `SyncServer` struct (VecDeque d'historique par shard)
- `SyncResponse`, `ShardedSyncResponse` enums
- `on_version(shard_id, version, segment_ids)` — enregistre sans toucher un handle
- `lookup(shard_id, client_version)` → segment IDs du client

Ce qui reste dans lucivy_core / sparse_vector :
- `on_commit(handle)` — lit meta.json puis appelle `server.on_version(...)`
- `sync(handle, path, client_version)` — appelle `server.lookup(...)` + `export_delta`

## Ce qui NE bouge PAS

| Module | Raison |
|--------|--------|
| `BlobDirectory` | Wrapper lucivy-spécifique autour de `BlobCache`. Reste dans lucivy_core, implémente `Directory` trait |
| `ShardedHandle` | Dépend de `LucivyHandle`, actors, DAG |
| `search_dag.rs` | 100% lucivy |
| `query.rs` | 100% lucivy |
| `bm25_global.rs` | 100% lucivy |

`BlobDirectory` reste dans lucivy_core mais devient un thin wrapper autour de
`lucistore::BlobCache<S>` + `StdFsDirectory`. La logique lourde (matérialisation,
write-through, cleanup) est dans lucistore.

## Structure de la crate

```
lucistore/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── blob_store.rs        — trait BlobStore + MemBlobStore
│   ├── blob_cache.rs        — BlobCache<S> : matérialisation, write-through, cleanup
│   ├── shard_storage.rs     — trait ShardStorage, FsShardStorage, BlobShardStorage<S>
│   ├── snapshot.rs           — LUCE format (export/import snapshot)
│   ├── delta.rs              — LUCID format (IndexDelta, serialize/deserialize)
│   ├── delta_sharded.rs      — LUCIDS format (ShardedDelta)
│   ├── sync_server.rs        — SyncServer, version history, dispatch
│   ├── version.rs            — compute_version_from_bytes, FNV-1a
│   └── fs_utils.rs           — read_directory_files, apply_delta, apply_sharded_delta
```

## Dépendances

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Pas de dépendance à ld-lucivy, luciole, ou quoi que ce soit moteur-spécifique.
```

## Qui dépend de lucistore

```
lucistore                    ← zéro dépendance moteur
  ↑
lucivy_core                  ← dépend de ld-lucivy + luciole + lucistore
  ↑
sparse_vector                ← dépend de lucistore (+ son propre moteur)
  ↑
rag3weaver                   ← dépend de lucivy_core + sparse_vector
```

## API surface pour sparse_vector

Quand sparse_vector voudra le sharding + sync :

```rust
use lucistore::blob_store::BlobStore;
use lucistore::shard_storage::{ShardStorage, FsShardStorage};
use lucistore::delta::{IndexDelta, serialize_delta, deserialize_delta};
use lucistore::delta_sharded::{ShardedDelta, apply_sharded_delta};
use lucistore::sync_server::SyncServer;
use lucistore::version::compute_version_from_bytes;
use lucistore::fs_utils::apply_delta;

// Sparse implémente son propre ShardedSparseHandle qui utilise :
// - FsShardStorage pour les répertoires
// - export_delta / apply_delta pour la sync
// - SyncServer pour l'historique
```

## Plan d'implémentation

### Phase 1 : Créer la crate et déplacer le code

1. `mkdir lucistore/` dans le workspace ld-lucivy
2. Créer `Cargo.toml` (deps: serde, serde_json)
3. Copier/adapter les modules :
   - `blob_store.rs` — copie directe depuis lucivy_core
   - `blob_cache.rs` — extraire la logique de matérialisation/write-through de `BlobDirectory`
   - `snapshot.rs` — extraire les parties génériques (LUCE format)
   - `sync.rs` → `delta.rs` + `delta_sharded.rs` + `sync_server.rs` + `version.rs`
   - `shard_storage.rs` — nouveau trait simplifié + FsShardStorage + BlobShardStorage
   - `fs_utils.rs` — `read_directory_files`, `apply_delta`, `apply_sharded_delta`
4. Ajouter `lucistore` au workspace `Cargo.toml`
5. Faire dépendre `lucivy_core` de `lucistore`
6. Vérifier compilation

### Phase 2 : Adapter lucivy_core

1. `lucivy_core/blob_store.rs` → re-export `lucistore::blob_store`
2. `lucivy_core/blob_directory.rs` → thin wrapper : `BlobCache<S>` + `StdFsDirectory` + `impl Directory`
3. `lucivy_core/snapshot.rs` → garder les fonctions high-level, déléguer le format
4. `lucivy_core/sync.rs` → garder `export_delta(handle, ...)`, importer les types
5. `lucivy_core/sharded_handle.rs` → utiliser `lucistore::shard_storage::ShardStorage`
   pour les parties root file, garder `create_shard_handle` spécifique
6. Tests : 18 sync tests + blob_store tests + snapshot tests doivent passer

### Phase 3 : sparse_vector utilise lucistore

1. Ajouter dep `lucistore` dans sparse_vector
2. Implémenter `SparseBlobStorage<S>` via `BlobCache<S>` — mmap depuis cache_path
3. Implémenter `ShardedSparseHandle` en utilisant `FsShardStorage`
4. Sync delta via `apply_delta` pour les fichiers sparse (mmap + vectors + dims)

## Risques

- **Re-exports** : lucivy_core re-exporte les types lucistore pour ne pas casser
  les consommateurs existants (lucivy_fts, bindings). Transition douce.
- **BlobDirectory refactor** : `BlobDirectory` passe de monolithique à thin wrapper
  autour de `BlobCache`. La surface API ne change pas (toujours `impl Directory`),
  mais l'implémentation interne est réorganisée. Risque de régressions subtiles
  sur le cycle de vie du cache (Arc refcount, drop order). Mitigé par les tests existants.
- **Taille du refactoring** : modéré. Les interfaces publiques ne changent pas,
  c'est surtout du déplacement de fichiers + ajustement d'imports.
