# Doc 17 — Audit architecture : DAG séparation + ShardedHandle API

Date : 21 mars 2026

## 1. DAG : lucivy vs luciole — séparation propre ?

### État actuel

**Luciole** (framework generic) fournit :
- `Dag`, `Node`, `PollNode`, `GraphNode` — structure + traits
- `execute_dag()` — exécution par niveaux, parallèle intra-niveau, inline sur scheduler thread
- `ScatterDag` — fan-out N tâches → CollectNode → HashMap résultats
- `StreamDag` — topologie pour drain ordering des acteurs
- `Scheduler`, `Pool`, `Scope` — threading + actor lifecycle
- `CheckpointStore` — persistence + resume on failure
- `EventBus`, `TapRegistry` — observabilité

**Lucivy** (domain-specific) définit :
- `commit_dag.rs` — prepare → merges ∥ → finalize → save → gc → reload
- `merge_dag.rs` — init → postings ∥ store ∥ fast_fields → sfx → close
- `sfx_dag.rs` — collect_tokens → build_fst ∥ copy_gapmap ∥ merge_sfxpost → write
- `search_dag.rs` — drain → flush → build_weight → search_shard_N ∥ → merge_results

### Verdict : bien séparé

La séparation est saine. Les DAGs de lucivy sont 100% domain-specific (segments,
postings, FST, BM25). Le framework luciole ne connaît rien à lucivy.

### Patterns réutilisables identifiés (pas urgents)

| Pattern | Où dans lucivy | Abstraction possible dans luciole |
|---------|---------------|-----------------------------------|
| Cascade loop | `segment_updater_actor.rs` — loop { execute_dag; break if done } | `execute_dag_cascade(dag_builder, condition)` |
| Dynamic ports | `commit_dag.rs` — `format!("merge_{i}")` + `Box::leak` | Helper `gen_ports(n, prefix)` |
| Validation node | `sfx_dag.rs` — passthrough avec check | `ValidateNode::new(check_fn)` |
| Nested sub-DAG | `MergeNode` exécute `merge_dag` inline | Déjà supporté via `GraphNode` |

Aucun n'est assez récurrent pour justifier une extraction maintenant.
À reconsidérer si luciole est utilisé par d'autres projets.

## 2. ShardedHandle API — est-ce facile pour des gens ?

### Ce qui est bien

- **Un seul `search(config, top_k, sink)`** — l'utilisateur ne gère pas les shards
- **BM25 unifié** — IDF agrégé sur tous les shards via `AggregatedBm25StatsOwned`
  avant de dispatcher le Weight. Les scores sont globalement cohérents.
- **Merge résultats** — binary heap top-K sur les résultats de tous les shards
- **Token-aware routing** — balance_weight configurable (0.0 = pur IDF, 1.0 = round-robin)
- **`create_with_storage()`** — backend pluggable (filesystem, BlobStore, mémoire)

### Ce qui est pénible

**1. Highlights — boilerplate de 8 lignes par résultat**

```rust
// Aujourd'hui :
let sink = Arc::new(HighlightSink::new());
let results = handle.search(&config, 10, Some(sink.clone()))?;
for r in &results {
    let shard = handle.shard(r.shard_id).unwrap();
    let searcher = shard.reader.searcher();
    let seg_reader = searcher.segment_reader(r.doc_address.segment_ord);
    let seg_id = seg_reader.segment_id();
    let highlights = sink.get(seg_id, r.doc_address.doc_id);
    // ... utiliser highlights
}
```

**Proposition** : méthode `search_with_docs()` qui retourne directement
`Vec<SearchHit { score, doc: LucivyDocument, highlights: HashMap<String, Vec<[usize; 2]>> }>`.

**2. Récupérer un document stocké — pas de helper**

```rust
// Aujourd'hui :
let shard = handle.shard(result.shard_id).unwrap();
let searcher = shard.reader.searcher();
let doc: LucivyDocument = searcher.doc(result.doc_address)?;
```

**Proposition** : `handle.get_doc(&result) -> LucivyDocument`

**3. Pas dans les bindings**

Aucun binding (C++, Node, Python, WASM) n'expose ShardedHandle.
Tout passe par LucivyHandle mono-shard. Pour les utilisateurs externes,
le sharding n'existe tout simplement pas.

**4. Pipeline async implicite**

`add_document()` est non-bloquant (passe par reader pool → router → shard actor).
Il faut `commit()` pour rendre visible. Pas documenté clairement.

### Quick wins proposés

1. **`handle.get_doc(result)`** — résout shard_id + doc_address → LucivyDocument (~10 lignes)
2. **`handle.search_with_docs(config, top_k)`** — retourne docs + highlights en un appel (~30 lignes)
3. **Exposer ShardedHandle dans au moins le binding Node.js** — le plus utilisé par les devs
4. **Doc utilisateur** — exemple complet create → index → search → highlights en 20 lignes

## 3. Lock file — problème du bench

### Le problème

Quand on relance le bench, l'index persisté a des `.lucivy-writer.lock` qui traînent.
Le `ShardedHandle::open()` crée un `IndexWriter` qui prend le lock.
Si le process précédent a crash/été killé, le lock n'a pas été relâché.

### Solutions

**Actuelle** : supprimer manuellement les lock files avant de rouvrir.
Le test `test_store_fallback.rs` fait ça :
```rust
for i in 0..4 {
    let lock = format!("{base}/shard_{i}/.lucivy-writer.lock");
    let _ = std::fs::remove_file(&lock);
}
```

**Mieux — option `force_open`** :
Ajouter un paramètre à `open()` qui supprime les lock files stale :
```rust
ShardedHandle::open_with_options(base, OpenOptions { force_unlock: true })
```

Le lock est un `flock` OS — si le process est mort, le lock est automatiquement
relâché par l'OS. Le fichier reste mais le `flock` est libre. Donc on peut
juste `try_lock` et si ça marche c'est que le process est mort → safe.

**Encore mieux — pas de lock file du tout pour le reader** :
`ShardedHandle::open()` pourrait ouvrir en mode read-only (sans IndexWriter).
Le writer serait créé lazily au premier `add_document()`. Pour le bench,
on veut juste chercher, pas écrire → pas besoin de lock.

### Implémentation dans LucivyHandle

`close()` est déjà implémenté et testé (6 tests). Le problème c'est que
le bench ne l'appelle pas avant de terminer. Solutions :
1. Le bench appelle `handle.close()` à la fin (ou `drop` suffit si le writer est `Option`)
2. `open()` avec stale lock detection

## 4. ACID / BlobStore / mmap — état des lieux

### Implémenté (dans le repo)

| Composant | Fichier | Tests | Statut |
|-----------|---------|-------|--------|
| `BlobStore` trait | `lucivy_core/src/blob_store.rs` | 3 | OK |
| `MemBlobStore` | `lucivy_core/src/blob_store.rs` | 3 | OK |
| `BlobDirectory` | `lucivy_core/src/blob_directory.rs` | 7 | OK |
| `ShardStorage` trait | `lucivy_core/src/sharded_handle.rs` | — | OK |
| `FsShardStorage` | `lucivy_core/src/sharded_handle.rs` | — | OK (défaut) |
| `BlobShardStorage` | `lucivy_core/src/sharded_handle.rs` | — | OK |
| `LucivyHandle::close()` | `lucivy_core/src/handle.rs` | 3 | OK |
| Lock file release | `src/directory/directory_lock.rs` | 3 | OK |

### Pattern "DB stocke, mmap sert"

```
Écriture : données → BlobStore (durable) + cache local (temp)
Lecture :  cache local → mmap zero-copy (rapide)
Drop :    cache nettoyé (ref-counted via Arc)
Recovery : si crash, cache recréé depuis BlobStore
```

### Manquant

| Composant | Usage | Priorité |
|-----------|-------|----------|
| `CypherBlobStore` | rag3db intégré | Haute |
| `PostgresBlobStore` | déploiement standalone | Moyenne |
| `S3BlobStore` | cloud | Moyenne |
| Bindings `close()` | 4/6 bindings manquent `close()` | Haute |
| Bindings `as_mut()` guards | 4/6 bindings crash si `close()` puis write | Haute |

### Docs existantes

- `docs/12-mars-2026-12h28/17-investigation-lock-file-et-architecture-blob-store.md`
- `docs/16-mars-2026-14h41/15-todo-sharding-blobstore-acid.md`
- `docs/17-mars-2026/05-design-shard-storage-trait-acid.md`
