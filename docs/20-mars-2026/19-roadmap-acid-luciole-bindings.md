# Doc 19 — Roadmap : ACID rag3weaver, luciole lib propre, bindings

Date : 21 mars 2026

## Les 3 priorités (dans l'ordre)

### Priorité 1 — ACID pour rag3weaver + sparse
### Priorité 2 — Luciole comme lib DAG/threading propre
### Priorité 3 — Publication bindings

---

## Priorité 1 : ACID rag3weaver — profiter du sharding + BlobStore

### État des lieux

**Déjà en place :**
- `BlobStore` trait dans lucivy_core (load/save/delete/exists/list)
- `MemBlobStore` pour les tests
- `BlobDirectory` — adapte BlobStore en Directory (mmap cache local)
- `ShardStorage` trait — abstraction par shard (FsShardStorage, BlobShardStorage)
- `CypherBlobStore` dans rag3weaver — stocke les blobs dans rag3db via Cypher
- `SparseHandle` — déjà `create_with_store()` / `open_with_store()` via BlobStore
- Namespacing : `Lucivy_` pour FTS, `Sparse_` pour sparse → pas de collision

**Ce qui manque :**

| Composant | Statut | Impact |
|-----------|--------|--------|
| FTS via BlobShardStorage dans rag3weaver | PAS EN PLACE | rag3weaver utilise encore `StdFsDirectory` pour FTS |
| ShardedSparseHandle | N'EXISTE PAS | sparse est mono-shard |
| Tests E2E avec vraie DB (Supabase/Postgres dans Docker) | PAS EN PLACE | on teste qu'avec MemBlobStore |
| `search_with_docs()` sur ShardedHandle | PAS EN PLACE | boilerplate highlights |

### Plan d'action

#### Étape 1.1 : FTS shardé dans rag3weaver via BlobStore

Aujourd'hui le Catalog crée des index FTS via l'extension C++ `CREATE_LUCIVY_INDEX`
qui utilise `StdFsDirectory` en dur. Pour profiter du sharding + ACID :

```
Catalog::register_entity(config avec shards=4)
  → ShardedHandle::create_with_storage(
      BlobShardStorage::new(cypher_blob_store, "entity_products", cache_base),
      config
    )
```

Changements dans rag3weaver :
- `catalog.rs` : remplacer les appels extension C++ par des appels Rust directs
  à `ShardedHandle` (on a déjà les handles sparse en Rust direct)
- Ou : adapter l'extension C++ pour supporter BlobStore (plus lourd)

**Recommandation** : appels Rust directs depuis rag3weaver, comme pour sparse.
L'extension C++ reste pour les utilisateurs rag3db standalone.

#### Étape 1.2 : ShardedSparseHandle

Sparse est mono-shard aujourd'hui. Pour les gros corpus (1M+ vecteurs),
il faudra sharder. Le pattern est identique à lucivy :

```rust
struct ShardedSparseHandle {
    shards: Vec<Arc<SparseHandle>>,
    router: SparseRouter,  // hash(record_id) → shard_id
    storage: Box<dyn SparseShardStorage>,
}

trait SparseShardStorage: Send + Sync {
    fn create_shard(&self, id: usize) -> Result<SparseHandle, String>;
    fn open_shard(&self, id: usize) -> Result<SparseHandle, String>;
}
```

Namespace BlobStore : `Sparse_{table}/shard_{id}/sparse.mmap`

La recherche : WAND sur chaque shard en parallèle, merge top-K.

#### Étape 1.3 : Tests E2E avec vraie DB dans Docker

**Setup Docker Supabase/Postgres :**

```yaml
# docker-compose.test.yml (gitignored)
services:
  postgres:
    image: postgres:16
    environment:
      POSTGRES_DB: lucivy_test
      POSTGRES_USER: test
      POSTGRES_PASSWORD: test
    ports:
      - "5432:5432"
    volumes:
      - pgdata:/var/lib/postgresql/data
volumes:
  pgdata:
```

**PostgresBlobStore :**

```rust
struct PostgresBlobStore {
    pool: sqlx::PgPool,
}

impl BlobStore for PostgresBlobStore {
    fn save(&self, index_name: &str, file_name: &str, data: &[u8]) -> io::Result<()> {
        // INSERT INTO _index_blobs (key, data) VALUES ($1, $2)
        // ON CONFLICT (key) DO UPDATE SET data = $2
        let key = format!("{index_name}/{file_name}");
        block_on(sqlx::query("INSERT INTO _index_blobs ...")
            .bind(&key).bind(data).execute(&self.pool))?;
        Ok(())
    }
    // ... load, delete, exists, list
}
```

**Tests E2E :**

```rust
#[test]
fn test_sharded_fts_postgres_acid() {
    // 1. Create ShardedHandle backed by PostgresBlobStore
    // 2. Index 1000 docs across 4 shards
    // 3. Commit
    // 4. Drop handle (simule crash)
    // 5. Reopen from Postgres — tous les docs doivent être là
    // 6. Search → même résultats qu'avant le "crash"
    // 7. Verify highlights
}

#[test]
fn test_sparse_postgres_acid() {
    // Même pattern avec SparseHandle + PostgresBlobStore
}

#[test]
fn test_mixed_fts_sparse_postgres() {
    // Catalog avec FTS + sparse, les deux dans Postgres
    // Register entity → index docs → search hybrid → verify
}
```

**Convention :** les tests Postgres sont `#[ignore]` par défaut.
On les lance avec `POSTGRES_URL=... cargo test -- --ignored`.

```bash
# Lancer Postgres
docker compose -f docker-compose.test.yml up -d

# Lancer les tests ACID
POSTGRES_URL="postgres://test:test@localhost:5432/lucivy_test" \
  cargo test --package lucivy-core --test acid_tests -- --ignored --nocapture

# Cleanup
docker compose -f docker-compose.test.yml down -v
```

Le `docker-compose.test.yml` est gitignored. Les tests vérifient `POSTGRES_URL`
et skip si absent.

#### Étape 1.4 : search_with_docs() — quick win

Résoudre le boilerplate highlights pour ShardedHandle (voir doc 18).
~40 lignes. Bénéficie à rag3weaver immédiatement.

### Ordre d'exécution Priorité 1

```
1.4  search_with_docs()                     ~40 lignes, 1h
1.1  FTS shardé via BlobStore dans catalog   ~100 lignes, refacto catalog
1.2  ShardedSparseHandle                     ~200 lignes, nouveau crate
1.3  Tests E2E Postgres Docker               ~150 lignes tests + docker-compose
```

---

## Priorité 2 : Luciole comme lib propre

### Ce qui est déjà propre

Luciole est déjà un crate séparé (`luciole/`) avec zéro dépendance sur lucivy.
Il fournit : Dag, Node, execute_dag, Scatter, GraphNode, Pool, Scheduler,
Checkpoint, EventBus, TapRegistry.

### Ce qui pourrait encore bouger

| Code dans lucivy | Destination luciole | Effort |
|------------------|---------------------|--------|
| `search_dag.rs` pattern scatter-gather | Déjà supporté par Scatter — rien à bouger | 0 |
| Helper `gen_ports(n, prefix)` | Utile dans dag.rs | ~10 lignes |
| `ValidateNode` generic | Utile dans node.rs | ~20 lignes |
| `CascadeExecutor` (loop + condition) | Pourrait être dans runtime.rs | ~30 lignes |

### Actions concrètes

1. **Cleanup API publique** — vérifier que `pub` est cohérent
2. **README + exemples** — un exemple simple de DAG dans luciole/examples/
3. **Cargo.toml** — préparer pour publish (description, license, repository)
4. **Tests** — luciole a déjà 132+ tests
5. **Optionnel** : les 3 helpers ci-dessus (~60 lignes)

### Publication

```toml
# luciole/Cargo.toml
[package]
name = "luciole"
version = "0.1.0"
description = "DAG execution framework with persistent thread pool"
license = "MIT"
repository = "https://github.com/L-Defraiteur/luciole"
```

`cargo publish -p luciole` — publié séparément de lucivy.

---

## Priorité 3 : Bindings — rafraîchir pour la release

### État actuel (6 bindings)

| Binding | close() | Mutex\<Option\> | ShardedHandle | Stemmer retiré |
|---------|---------|-----------------|---------------|----------------|
| CXX rag3db | ✓ | ✓ | Non | ✓ (ce cleanup) |
| WASM emscripten | ✗ | ✗ | Non | ✓ (ce cleanup) |
| WASM wasm-bindgen | ✗ | ✓ | Non | ✓ (ce cleanup) |
| Node.js napi | ✗ | ✗ | Non | ✓ (ce cleanup) |
| Python PyO3 | ✗ | ✗ | Non | ✓ (ce cleanup) |
| C++ standalone | ✗ | ✗ | Non | ✓ (ce cleanup) |

### Actions par binding

**Phase 3.1 — Adapter tous les writer access pour Option** (4 bindings)
- Emscripten, Node.js, Python, C++ standalone
- Pattern : `.as_mut().ok_or("index is closed")?`
- ~10 lignes par binding

**Phase 3.2 — Exposer close()** (5 bindings)
- Tous sauf CXX rag3db (déjà fait)
- Pattern : `handle.close()` exposé comme méthode
- ~5 lignes par binding

**Phase 3.3 — Exposer ShardedHandle** (au moins Node.js)
- Le plus demandé pour les devs
- `createSharded(path, config)` / `openSharded(path)`
- `search()` unifié avec highlights
- ~100 lignes pour Node.js

**Phase 3.4 — Cleanup JS/TS/Python wrappers**
- Retirer les refs stemmer dans les fichiers JS/TS/Python
- Mettre à jour les types TypeScript
- Mettre à jour les tests Python

**Phase 3.5 — Tests WASM emscripten multi-thread**
- Vérifier que l'ingestion multi-thread n'est pas cassée
- SharedArrayBuffer + Atomics

### Ordre

```
3.1  Option guards (4 bindings)     ~40 lignes total, 30min
3.2  Expose close() (5 bindings)    ~25 lignes total, 30min
3.3  ShardedHandle Node.js          ~100 lignes, 2h
3.4  Cleanup JS/TS/Python           ~30 lignes, 30min
3.5  Tests WASM                     dépend de l'état, 2-4h
```

---

## Timeline estimée

```
Semaine 1 :
  1.4  search_with_docs()
  3.1  Option guards
  3.2  Expose close()
  3.4  Cleanup JS/TS/Python

Semaine 2 :
  1.1  FTS shardé via BlobStore dans catalog
  2.1  Luciole cleanup + README

Semaine 3 :
  1.3  Tests E2E Postgres Docker
  3.3  ShardedHandle Node.js

Semaine 4+ :
  1.2  ShardedSparseHandle
  3.5  Tests WASM
  2.2  Publish luciole
```
