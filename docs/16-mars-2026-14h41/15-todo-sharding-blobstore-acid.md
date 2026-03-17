# Doc 15 — TODO : ShardedHandle + BlobDirectory pour ACID persistence

Date : 17 mars 2026

## Problème

`ShardedHandle` hardcode `StdFsDirectory` pour chaque shard :

```rust
let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())?;
let handle = LucivyHandle::create(dir, config)?;
```

Pour rag3weaver en production, on veut **ACID** :
- **mmap** pour les lectures temps réel (zero-copy, latence <1ms)
- **BlobStore** pour la persistence durable (DB-backed : Cypher, Postgres, S3)
- **Callback/abstraction** pour brancher facilement n'importe quel backend de persistence

## Ce qui existe déjà

### BlobDirectory (lucivy_core/src/blob_directory.rs)

`BlobDirectory<S: BlobStore>` implémente le trait `Directory` de lucivy. Pattern :

1. **Write** : écrit dans le BlobStore (source de vérité durable)
2. **Read** : charge depuis le BlobStore → cache local temp → mmap zero-copy
3. **Drop** : nettoie le cache local (ref-counted via Arc)

Le BlobStore est un trait :
```rust
pub trait BlobStore: Send + Sync + 'static {
    fn load(&self, index: &str, path: &str) -> Result<Vec<u8>>;
    fn save(&self, index: &str, path: &str, data: &[u8]) -> Result<()>;
    fn delete(&self, index: &str, path: &str) -> Result<()>;
    fn exists(&self, index: &str, path: &str) -> Result<bool>;
    fn list(&self, index: &str) -> Result<Vec<String>>;
}
```

Implémentations prévues :
- `MemBlobStore` (tests, en place)
- `CypherBlobStore` (rag3db, via Cypher queries)
- `PostgresBlobStore` (standalone)
- `S3BlobStore` (cloud)

### LucivyHandle (lucivy_core/src/handle.rs)

`LucivyHandle::create(dir: impl Directory, config)` — accepte déjà n'importe quel `Directory`. Pas besoin de changer LucivyHandle.

## Ce qu'il faut faire

### 1. Paramétrer ShardedHandle sur Directory

Actuellement :
```rust
pub struct ShardedHandle {
    shards: Vec<Arc<LucivyHandle>>,
    // ...
}

impl ShardedHandle {
    pub fn create(base_path: &str, config: &SchemaConfig) -> Result<Self, String> {
        let dir = StdFsDirectory::open(shard_dir)?;  // ← hardcodé
        LucivyHandle::create(dir, config)?;
    }
}
```

Après :
```rust
impl ShardedHandle {
    /// Create with filesystem directories (default).
    pub fn create(base_path: &str, config: &SchemaConfig) -> Result<Self, String> {
        Self::create_with_directory_factory(config, |shard_id| {
            let shard_dir = Path::new(base_path).join(format!("shard_{shard_id}"));
            fs::create_dir_all(&shard_dir)?;
            Ok(StdFsDirectory::open(shard_dir.to_str().unwrap())?)
        })
    }

    /// Create with a custom directory factory (BlobDirectory, etc.).
    pub fn create_with_directory_factory<F, D>(
        config: &SchemaConfig,
        dir_factory: F,
    ) -> Result<Self, String>
    where
        F: Fn(usize) -> Result<D, String>,
        D: Directory,
    {
        // ... même logique, mais dir_factory(shard_id) au lieu de StdFsDirectory
    }
}
```

### 2. BlobStore namespace par shard

Chaque shard préfixe ses clés BlobStore :
```
{index_name}/shard_0/meta.json
{index_name}/shard_0/segment_xxx.sfx
{index_name}/shard_1/meta.json
{index_name}/shard_1/segment_xxx.sfx
{index_name}/_shard_stats.bin      ← router state, au root
{index_name}/_shard_config.json    ← config, au root
```

Le `_shard_stats.bin` et `_shard_config.json` sont écrits directement via le BlobStore (pas via Directory), car ils ne sont pas gérés par le ManagedDirectory de lucivy.

### 3. Usage depuis rag3weaver

```rust
// rag3weaver Catalog
let blob_store = CypherBlobStore::new(db_connection);

let handle = ShardedHandle::create_with_directory_factory(&config, |shard_id| {
    let index_name = format!("entity_{entity_id}/shard_{shard_id}");
    Ok(BlobDirectory::new(blob_store.clone(), &index_name, temp_cache_dir))
});
```

Le Catalog n'a pas besoin de savoir combien de shards il y a — c'est transparent.

### 4. Callback pattern pour rechargement depuis DB

Pour le cas "l'index a été modifié par un autre noeud, rechargeons" :

```rust
// Option A : le BlobStore versionne
blob_store.has_newer_version(index, since_version)?

// Option B : callback de notification
handle.on_external_change(|| {
    // recharger les segments depuis le BlobStore
    handle.reload_from_store()?;
});
```

L'option B est plus flexible et s'intègre avec le pattern rag3weaver où le Catalog écoute les events DB.

## Priorité

Pas bloquant pour le sharding local (StdFsDirectory marche). À faire **avant** l'intégration rag3weaver cloud. Le changement est principalement dans `ShardedHandle::create` et `::open` — ~50 lignes.

## Lien avec sparse_vector

Même pattern : `ShardedSparseHandle` avec directory factory. Le `SparseHandle` utilise déjà `StorageBackend` qui est similaire à `BlobStore`. À terme, unifier ou bridger les deux traits.
