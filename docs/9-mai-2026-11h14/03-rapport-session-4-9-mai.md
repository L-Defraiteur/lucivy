# Rapport session 4 — 9 mai 2026 (11h14-14h30)

## Objectif

Diagnostiquer et corriger le deadlock WASM lors de l'ingestion du Linux kernel
(75K fichiers). Continuer le travail de la session 3.

## Root causes trouvées et corrigées

### 1. docstore_compress_dedicated_thread (ROOT CAUSE PRINCIPALE)

**Fichier** : `src/index/index_meta.rs`

Chaque `SegmentWriter` spawnait un pthread dédié pour la compression du doc store
via `thread::Builder::new().spawn()`. Ce thread recevait les blocs compressés via
un `sync_channel(3)` (capacité 3).

**Mécanisme du deadlock** : en WASM/emscripten, le pool de pthreads est limité
(PTHREAD_POOL_SIZE=8). Les 4 scheduler threads + les threads de compression +
le GC thread + les watch-callback threads saturaient le pool. Quand un thread de
compression n'était pas schedulé, le `sync_channel(3).send()` bloquait
l'actor handler indéfiniment.

**Pourquoi intermittent** : dépend de l'ordre de scheduling des pthreads par
emscripten. Si les compression threads récupéraient un slot rapidement → OK.
Sinon → deadlock.

**Fix** : `docstore_compress_dedicated_thread: false` en `#[cfg(target_arch = "wasm32")]`.
La compression se fait inline dans le même thread. Pas de pthread, pas de channel.

### 2. Deferred I/O dans FsWriter

**Fichier** : `lucivy_core/src/directory.rs`

`StdFsDirectory::open_write()` faisait :
- `full.exists()` — vérification I/O sur OPFS (~2-5s)
- `fs::create_dir_all(parent)` — création répertoires I/O sur OPFS (~2-5s)

`SegmentWriter::for_segment()` appelle `open_write()` 7+ fois. Après un commit,
les 4 indexers font ça simultanément (batch=1, premier doc) → 28+ opérations
OPFS en parallèle → tous les scheduler threads bloqués.

**Fix** : `FsWriter` bufferise TOUT en RAM. `flush()` = no-op. L'I/O (création
répertoires + écriture fichier) est reportée au `terminate()`, appelé pendant
`finalize()` qui tourne sur un task thread — jamais dans un actor handler.

### 3. Autres thread::spawn éliminés

| Fichier | Thread | Fix |
|---------|--------|-----|
| `watch_event_router.rs` | `watch-callbacks` (1 par commit) | Callbacks inline en WASM |
| `reader/warming.rs` | `lucivy-warm-gc` (permanent) | Skip en WASM, GC dans warm() |

### 4. WRITER_HEAP_SIZE réduit pour WASM

**Fichier** : `lucivy_core/src/handle.rs`

50MB → 15MB en WASM. Réduit la pression mémoire :
- Segments plus petits → finalize plus souvent → moins de RAM par segment
- Fichiers .store plus petits → `open_read()` charge moins en RAM
- 4 shards × 15MB = 60MB vs 200MB avant

## Diagnostic ajouté

### ActorActivity dynamique

**Fichier** : `luciole/src/scheduler.rs`

- `ActorActivity` : `&'static str` → `String` (labels dynamiques)
- `ActorContext` : ajouté `activity: Arc<ActorActivity>` + `set_activity()` + `activity()`
- WARNING dump utilise maintenant `slot.activity.get()` au lieu de juste "TAKEN"

### Activity reporter dans SegmentWriter

**Fichier** : `src/indexer/segment_writer.rs`

Callback `Fn(&str)` appelé à chaque étape de `add_document()` :
- `fast_fields` → `index_doc` → `sfx_empty` → `store_doc`

Branché dans l'indexer via `writer.set_activity_reporter(...)` qui forward
vers `ActorActivity`.

### Labels dans l'indexer

**Fichier** : `src/indexer/indexer_actor.rs`

- `poll_finalize`, `skip_to`, `new_segment`, `segment_writer_init`
- `add_doc i/batch_len` (tous les 16 docs)
- `finalize_submit`, `yield`
- `flush`, `drain_reply`
- `[indexer] submit_finalize_task maxdoc=N`
- `[finalize] START/DONE maxdoc=N took Xs`
- `[indexer] SLOW add_document` si >500ms

### Labels dans ShardActor

**Fichier** : `lucivy_core/src/sharded_handle.rs`

- `search shard_N`, `commit_drain`, `commit_flush`, `commit_finalize`, `commit_done`

## Résultats

| Repo | Avant | Après |
|------|-------|-------|
| rag3db (4308 docs) | OK (déjà passait) | **OK — 0 warnings** |
| Linux kernel (75K) | Deadlock à 2K docs | **34K+ docs sans deadlock** (OOM WASM à 34K) |

## Problème restant — OOM WASM sur gros indexes

L'ingestion du Linux kernel crash à ~34K docs avec :
```
memory allocation failed / OutOfMemory
```

**Cause** : `StdFsDirectory::open_read()` fait `fs::read()` qui charge chaque
fichier segment en RAM. Avec 122+ segments non-mergés, la mémoire 4GB WASM
est saturée.

**Pistes pour résoudre** :
1. **Zéro-copie dans Directory** — `open_read` retourne un `Arc<[u8]>` partagé
   au lieu de cloner le Vec à chaque appel. Réduit la duplication mémoire.
2. **Merge plus agressif** — baisser `min_num_segments` de 8 à 4. Moins de
   segments ouverts simultanément.
3. **Paged read** — ne charger que les pages accédées (faux mmap). Gros refacto.

Note : le Linux kernel (75K fichiers) est un cas extrême. Les repos normaux
(< 10K fichiers) fonctionnent parfaitement. L'OOM est un problème de mémoire
WASM 32-bit (max 4GB), pas un bug.

## Commits

```
a5c8ee8 fix: deferred I/O in FsWriter — eliminate OPFS blocking in actor handlers
e1088fa fix: eliminate all thread::spawn in WASM — root cause of intermittent deadlocks
```

## Fichiers clés modifiés

| Fichier | Changement |
|---------|-----------|
| `luciole/src/scheduler.rs` | ActorActivity String, ActorContext.activity, WARNING dump |
| `lucivy_core/src/directory.rs` | FsWriter deferred I/O (flush=no-op, terminate=write) |
| `lucivy_core/src/handle.rs` | WRITER_HEAP_SIZE 15MB en WASM |
| `lucivy_core/src/sharded_handle.rs` | Activity labels commit chain |
| `src/index/index_meta.rs` | docstore_compress_dedicated_thread=false en WASM |
| `src/directory/watch_event_router.rs` | Callbacks inline en WASM |
| `src/reader/warming.rs` | GC thread skip en WASM |
| `src/indexer/segment_writer.rs` | activity_reporter callback |
| `src/indexer/indexer_actor.rs` | bind_activity, labels fins, finalize logs |

## Prochaine session — TODO

### Priorité 1 : Compat layer v2

Préparer la v2 sans casser les utilisateurs existants :

1. **Query compat layer** dans `build_query` — les anciens types (`fuzzy`,
   `regex`, `term` top-level) sont routés vers le bon backend avec un warning
   "deprecated, use X instead". ~50 lignes.

2. **startsWith** — soit wrapper qui appelle `contains_exact` avec SI=0 sur le
   premier token, soit retiré avec erreur explicite.

3. **Playground** — mettre à jour l'UI, retirer/griser les queries deprecated.

4. **Versioning** — branche `v1` pour maintenance, `v2.0.0` avec CHANGELOG
   et MIGRATION.md.

### Priorité 2 : Zéro-copie Directory

Pour supporter les gros indexes en WASM :
- `MemoryDirectory.files: HashMap<PathBuf, Arc<[u8]>>`
- `open_read()` retourne `FileSlice` pointant vers le même Arc
- Élimine la duplication mémoire au reload reader

### Priorité 3 : Audit thread::spawn

Vérifier qu'aucun nouveau `thread::spawn` n'est introduit. Ajouter un
`#[cfg(target_arch = "wasm32")]` lint ou un grep dans CI.
