# Investigation lock file lucivy + architecture IndexBlobStore

Date : 13 mars 2026

Réf : doc 16 (BM25 scoring), rag3weaver doc 18 (bug lock), doc 19 (sparse V2 mmap), doc 20 (architecture composite)

## Investigation : bug "LockBusy" (rag3weaver doc 18)

### Symptôme

Le test E2E rag3weaver `phase6_sparse_mmap_persistence` échoue au reopen :

```
cannot create writer: Failed to acquire Lockfile: LockBusy.
"there is already an IndexWriter working on this Directory"
```

Le `.tantivy-writer.lock` n'est pas relâché entre deux sessions dans le même process.

### Tests de reproduction écrits

**Au niveau `IndexWriter` (ld-lucivy)** — `src/indexer/index_writer.rs` :

| Test | Scénario | Résultat |
|------|----------|----------|
| `test_lockfile_released_on_drop_mmap` | MmapDirectory, 100 docs, commit, drop, reopen | OK |
| `test_lockfile_released_after_wait_merging_mmap` | Idem + `wait_merging_threads()` | OK |
| `test_lockfile_reopen_cycle_mmap` | 5 cycles open/insert/commit/drop sur disque | OK |

**Au niveau `LucivyHandle` (lucivy_core)** — `lucivy_core/src/handle.rs` :

| Test | Scénario | Résultat |
|------|----------|----------|
| `test_handle_close_reopen_lock` | create → 50 docs → commit → drop → reopen | OK |
| `test_handle_close_reopen_with_merges` | 500 docs, 10 commits, merges actifs → drop → reopen | OK |
| `test_handle_reopen_cycles` | 5 cycles LucivyHandle open/insert/commit/drop | OK |

### Mécanisme de lock : deux implémentations

| Directory | Mécanisme | Libération |
|-----------|-----------|-----------|
| `RamDirectory` | Fichier virtuel, `open_write` → `FileAlreadyExists` | `DirectoryLockGuard::drop()` → `directory.delete(path)` |
| `MmapDirectory` | OS file lock (`flock`) via `try_lock_exclusive()` | `ReleaseLockFile::drop()` → `File` droppé → OS libère le flock |

Les deux mécanismes fonctionnent correctement. Le `DirectoryLockGuard` / `ReleaseLockFile` a sa propre copie du directory — indépendant de l'`Index`.

### Drop order de `IndexWriter`

```rust
pub struct IndexWriter {
    _directory_lock: Option<DirectoryLock>,  // drop 1er → libère le lock
    index: Index,                             // drop 2ème
    segment_updater: SegmentUpdater,          // drop plus tard (a son propre Arc<Index>)
    ...
}

impl Drop for IndexWriter {
    fn drop(&mut self) {
        self.segment_updater.kill();      // async — envoie Kill
        for worker in &self.worker_refs {
            let _ = worker.send(Shutdown); // async — envoie Shutdown
        }
        // _directory_lock droppé ensuite → lock libéré
    }
}
```

Le drop est safe : `_directory_lock` est auto-contenu (propre `Box<dyn Directory>`), `SegmentUpdater` a son propre `Arc<Index>` clone. Aucune dépendance croisée entre les champs.

Note : `FsWriter dropped without flushing` warning observé dans le test cycles — les workers async n'ont pas fini quand le lock est libéré. Pas un bug de lock, mais des segments en cours d'écriture perdus. Sans impact si un commit a été fait avant le drop.

### Root cause : côté rag3db engine, pas lucivy

La chaîne de drop attendue :

```
drop(Catalog) → drop(Arc<dyn DbConnection>) → drop(Rag3dbConnection)
  → drop(conn: Connection<'static>)   [1er — déclaré avant db]
  → drop(db: Box<Database>)           [2ème]
       → UniquePtr::drop → ~Database() C++
            → ~Catalog() → ~NodeTable()
                 → ~LucivyIndex() → drop rust::Box<LucivyHandle>   ???
```

**Le `~Database()` C++ ne cascade pas la destruction des index d'extensions.** Le `LucivyIndex` (et donc le `rust::Box<LucivyHandle>` avec son `IndexWriter`) n'est jamais droppé.

Preuve : le test E2E utilise un workaround `remove_lucivy_locks()` qui supprime manuellement les `.tantivy-writer.lock` restants sur le disque entre les sessions.

### Solution implémentée : `LucivyHandle::close()`

Le `writer` est passé de `Mutex<IndexWriter>` à `Mutex<Option<IndexWriter>>` :

```rust
pub fn close(&self) -> Result<(), String> {
    let mut guard = self.writer.lock()?;
    if let Some(mut writer) = guard.take() {
        if self.has_uncommitted() {
            writer.commit()?;
        }
        // writer droppé ici → lock libéré
    }
    Ok(())
}
```

Ceci permet de libérer le lock explicitement via le bridge CXX, sans dépendre du destructeur C++ de `Database`. L'extension `lucivy_fts` peut exposer un `close_index()` appelé avant le drop de la connexion.

#### Garanties de sécurité de `close()`

- **Après `close()`** : toute écriture (`add_document`, `commit`, `delete`) retourne `Err("index is closed")` via `.as_mut().ok_or(...)`. Pas de panic, pas d'UB.
- **La lecture continue** : `search()` utilise le `reader`, pas le `writer`. Les recherches fonctionnent normalement après `close()`.
- **Idempotent** : appeler `close()` deux fois est safe — `guard.take()` sur `None` retourne `None`, le `if let Some` ne matche pas.
- **`rollback()`** est aussi safe : `if let Some(writer) = guard.as_mut()` — si `None`, ne fait rien.

#### Fichiers modifiés

- `lucivy_core/src/handle.rs` : `Mutex<Option<IndexWriter>>`, `close()`, tests
- `lucivy_fts/rust/src/bridge.rs` : tous les accès writer adaptés pour `Option` avec erreur `"index is closed"`
- `bindings/wasm/src/lib.rs` : même adaptation pour le binding WASM

## Architecture : trait `IndexBlobStore` unifié

### Contexte

Le pattern "DB = source of truth, mmap = cache runtime" (rag3weaver doc 20) s'applique à la fois à sparse et à lucivy FTS. Plutôt que deux traits séparés, un seul trait générique.

### Trait

```rust
/// Backend-agnostic blob persistence for any index type (sparse, FTS, vector, etc.)
trait IndexBlobStore: Send + Sync {
    /// Lister les fichiers stockés pour un index.
    fn list(&self, index_name: &str) -> Result<Vec<String>>;

    /// Charger un fichier blob.
    fn load(&self, index_name: &str, file_name: &str) -> Result<Vec<u8>>;

    /// Sauvegarder des fichiers (atomique — tout ou rien).
    fn save(&self, index_name: &str, files: &[(&str, &[u8])]) -> Result<()>;

    /// Supprimer des fichiers (segments obsolètes après merge, etc.)
    fn delete(&self, index_name: &str, files: &[&str]) -> Result<()>;
}
```

### Implémentations envisagées

| Impl | Backend | Usage |
|------|---------|-------|
| `FileBlobStore` | Fichiers sur disque | Embedded, dev, tests (ce qu'on a) |
| `CypherBlobStore` | Table `_index_blobs` via Cypher | rag3db embedded |
| `S3BlobStore` | Object storage | Cloud / production |
| `PostgresBlobStore` | Large objects ou bytea | Déploiement Postgres |

### Utilisation par lucivy FTS

Les segments lucivy sont write-once (créés, lus, supprimés au merge). Pattern idéal pour du blob store.

```
open():
  1. blob_store.list("kb_fts") → ["meta.json", "seg_abc.term", ...]
  2. Pour chaque: load() → écrire dans temp_dir/
  3. MmapDirectory::open(temp_dir) → search prêt

commit():
  1. IndexWriter::commit() → nouveaux segments dans temp_dir/
  2. Diff via list_managed_files() vs blob_store.list()
  3. blob_store.save() les nouveaux, blob_store.delete() les obsolètes

search():
  → mmap inchangé, zero-copy, OS page cache — zéro impact perf
```

### Utilisation par sparse

Même trait, 3 fichiers fixes :

```rust
store.save("kb_sparse", &[
    ("postings", &mmap_bytes),
    ("vectors", &vectors_bytes),
    ("dims", &dims_bytes),
])
```

### Table DB unifiée

```
_index_blobs(index_name STRING, file_name STRING, data BLOB, PRIMARY KEY(index_name, file_name))
```

Tous les types d'index (FTS, sparse, vector) partagent la même table.

## Fichiers modifiés

```
src/indexer/index_writer.rs         # 3 tests lock MmapDirectory
lucivy_core/src/handle.rs           # Mutex<Option<IndexWriter>>, close(), 3 tests handle
```

## Prochaines étapes

1. **Exposer `close_index()` via le bridge CXX** (lucivy_fts) pour que l'extension puisse libérer le lock avant le drop de la DB
2. **Implémenter `FileBlobStore`** comme refactoring sans changement de comportement
3. **`CypherBlobStore`** quand la persistence unifiée est nécessaire (cloud)
