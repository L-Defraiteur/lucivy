# Design : ShardStorage trait — ACID mmap + BlobStore + super-sharding

Date : 17 mars 2026

## But

Abstraire le stockage des shards pour supporter :
1. **Filesystem** (défaut, dev/standalone)
2. **BlobStore ACID** (mmap local pour reads temps réel + blob DB pour persistence durable)
3. **Super-sharding** (rag3weaver crée N `ShardedHandle`, chaque avec son propre storage)

## Problème avec Box\<dyn Directory\>

Le trait `Directory` a un supertrait `DirectoryClone` → `Box<dyn Directory>` ne peut pas implémenter `Directory`. On ne peut pas retourner un `Box<dyn Directory>` depuis un trait et le passer à `LucivyHandle::create(dir: impl Directory)`.

## Solution : ShardStorage crée les handles directement

```rust
/// Abstraction pour le stockage des shards.
/// Chaque implémentation contrôle le type concret de Directory.
pub trait ShardStorage: Send + Sync {
    /// Créer un nouveau LucivyHandle pour un shard.
    fn create_shard_handle(
        &self,
        shard_id: usize,
        config: &SchemaConfig,
    ) -> Result<LucivyHandle, String>;

    /// Ouvrir un LucivyHandle existant pour un shard.
    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String>;

    /// Écrire un fichier root (config, stats).
    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String>;

    /// Lire un fichier root.
    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String>;

    /// Vérifier si un fichier root existe.
    fn root_file_exists(&self, name: &str) -> bool;
}
```

### Pourquoi c'est mieux

- Pas de `Box<dyn Directory>`, pas de contournement du type system
- Chaque backend contrôle tout le lifecycle du handle
- Un `BlobShardStorage` peut pré-charger le cache, gérer les versions, etc.
- Un `MemShardStorage` peut utiliser `RamDirectory` pour les tests

## Implémentations

### FsShardStorage (filesystem, défaut)

```rust
struct FsShardStorage { base_path: String }

impl ShardStorage for FsShardStorage {
    fn create_shard_handle(&self, shard_id: usize, config: &SchemaConfig) -> Result<LucivyHandle, String> {
        let shard_dir = format!("{}/shard_{shard_id}", self.base_path);
        fs::create_dir_all(&shard_dir)?;
        let dir = StdFsDirectory::open(&shard_dir)?;
        LucivyHandle::create(dir, config)
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let shard_dir = format!("{}/shard_{shard_id}", self.base_path);
        let dir = StdFsDirectory::open(&shard_dir)?;
        LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        fs::write(format!("{}/{name}", self.base_path), data)?;
        Ok(())
    }
    // ...
}
```

### BlobShardStorage (ACID : mmap + DB blob)

```rust
struct BlobShardStorage<S: BlobStore> {
    blob_store: S,
    index_name: String,
    cache_dir: PathBuf,  // local temp dir for mmap cache
}

impl<S: BlobStore> ShardStorage for BlobShardStorage<S> {
    fn create_shard_handle(&self, shard_id: usize, config: &SchemaConfig) -> Result<LucivyHandle, String> {
        let prefix = format!("{}/shard_{shard_id}", self.index_name);
        let cache = self.cache_dir.join(format!("shard_{shard_id}"));
        let dir = BlobDirectory::new(self.blob_store.clone(), &prefix, &cache);
        LucivyHandle::create(dir, config)
    }

    fn open_shard_handle(&self, shard_id: usize) -> Result<LucivyHandle, String> {
        let prefix = format!("{}/shard_{shard_id}", self.index_name);
        let cache = self.cache_dir.join(format!("shard_{shard_id}"));
        let dir = BlobDirectory::new(self.blob_store.clone(), &prefix, &cache);
        LucivyHandle::open(dir)
    }

    fn write_root_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        self.blob_store.save(&self.index_name, name, data)?;
        Ok(())
    }

    fn read_root_file(&self, name: &str) -> Result<Vec<u8>, String> {
        self.blob_store.load(&self.index_name, name)
    }

    fn root_file_exists(&self, name: &str) -> bool {
        self.blob_store.exists(&self.index_name, name).unwrap_or(false)
    }
}
```

**Pattern ACID :**
1. **Write** : `BlobDirectory` écrit dans le BlobStore (source de vérité durable, e.g. Cypher, Postgres, S3)
2. **Read** : charge depuis le BlobStore → cache local temp → mmap zero-copy
3. **Drop** : nettoie le cache local (ref-counted via Arc)
4. **Recovery** : si crash, le BlobStore est la source de vérité — le cache est recréé

### MemShardStorage (tests)

```rust
struct MemShardStorage;

impl ShardStorage for MemShardStorage {
    fn create_shard_handle(&self, shard_id: usize, config: &SchemaConfig) -> Result<LucivyHandle, String> {
        let dir = RamDirectory::create();
        LucivyHandle::create(dir, config)
    }
    // root files in HashMap...
}
```

## Intégration super-sharding (rag3weaver)

```
rag3weaver Catalog
  ├── Entity "repo-A" → ShardedHandle(BlobShardStorage { store, "repo-A", cache })
  │     ├── shard_0 → BlobDirectory("repo-A/shard_0")
  │     ├── shard_1 → BlobDirectory("repo-A/shard_1")
  │     └── _shard_stats.bin → blob_store.save("repo-A", "_shard_stats.bin")
  │
  ├── Entity "repo-B" → ShardedHandle(BlobShardStorage { store, "repo-B", cache })
  │     └── shard_0 (petit repo, 1 shard)
  │
  └── Cross-entity search:
        AggregatedBm25Stats([repo-A searchers..., repo-B searchers...])
        → scatter-gather sur tous les shards de toutes les entities
```

Le `Catalog` crée un `BlobShardStorage` par entity avec le même `BlobStore` (shared connection pool). Chaque entity a son propre namespace dans le store.

Le `AggregatedBm25Stats` fonctionne cross-entity : on collecte les searchers de tous les shards de toutes les entities → IDF global exact.

## Changements dans ShardedHandle

```rust
pub struct ShardedHandle {
    shards: Vec<Arc<LucivyHandle>>,
    shard_actors: Vec<ActorRef<Envelope>>,
    router: Mutex<ShardRouter>,
    storage: Box<dyn ShardStorage>,  // ← remplace base_path
    pub schema: Schema,
    pub field_map: Vec<(String, Field)>,
    pub config: SchemaConfig,
    has_deletes: AtomicBool,
    text_fields: Vec<Field>,
}

impl ShardedHandle {
    // Raccourcis filesystem
    pub fn create(base_path: &str, config: &SchemaConfig) -> Result<Self, String> {
        Self::create_with_storage(Box::new(FsShardStorage::new(base_path)?), config)
    }

    pub fn open(base_path: &str) -> Result<Self, String> {
        Self::open_with_storage(Box::new(FsShardStorage::new(base_path)?))
    }

    // Backends custom
    pub fn create_with_storage(storage: Box<dyn ShardStorage>, config: &SchemaConfig) -> Result<Self, String> {
        // ...
        let handle = storage.create_shard_handle(i, config)?;
        // ...
    }

    pub fn open_with_storage(storage: Box<dyn ShardStorage>) -> Result<Self, String> {
        // ...
        let handle = storage.open_shard_handle(i)?;
        // ...
    }
}
```

## Fichiers à modifier

1. `lucivy_core/src/sharded_handle.rs` — ShardStorage trait + FsShardStorage + refacto create/open
2. `lucivy_core/src/blob_directory.rs` — déjà existant, utilisé par BlobShardStorage (futur)

## Ce qui ne change PAS

- `LucivyHandle` — accepte déjà `impl Directory`, aucun changement
- `ShardRouter` — indépendant du storage
- `ShardActor` — tient un `Arc<LucivyHandle>`, indifférent au storage
- Les tests — `create(base_path, config)` raccourci filesystem reste identique
- Le bench — idem

## Estimation

- ShardStorage trait + FsShardStorage : ~60 lignes
- Refacto create/open : ~30 lignes modifiées
- Aucun nouveau fichier — tout dans sharded_handle.rs
