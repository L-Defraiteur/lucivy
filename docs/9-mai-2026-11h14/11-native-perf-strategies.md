# Stratégies performance native vs WASM

## Constat

L'indexation 90K docs (Linux kernel) prend ~12 min en single shard. Avant les
optimisations WASM, c'était plus rapide. Le `StdFsDirectory` (deferred I/O,
pas de mmap) est utilisé partout, y compris en natif.

## Ce qui est déjà cfg-gaté WASM vs natif

| Setting | WASM | Natif | Fichier |
|---------|------|-------|---------|
| `WRITER_HEAP_SIZE` | 15MB | 50MB | `lucivy_core/src/handle.rs:52-55` |
| `writer_with_num_threads(1, ...)` | 1 thread | multi-thread | `lucivy_core/src/handle.rs:70-80` |
| `docstore_compress_dedicated_thread` | false | true | `src/index/index_meta.rs` |
| Watch callbacks | inline | threaded | `src/directory/watch_event_router.rs` |
| GC thread | skip | active | `src/reader/warming.rs` |

## Ce qui n'est PAS cfg-gaté (même code natif et WASM)

### 1. StdFsDirectory au lieu de MmapDirectory

**Impact : MOYEN (lecture/merge)**

`lucivy_core/src/directory.rs` utilise `StdFsDirectory` partout :
- `open_read()` → `fs::read()` (copie tout le fichier en Vec<u8>)
- `open_write()` → `FsWriter` (accumule en Vec<u8>, écrit à terminate())

`MmapDirectory` existe dans `src/directory/mmap_directory/mod.rs` (feature `mmap`)
mais n'est jamais utilisé par `lucivy_core`.

**Stratégie :** En natif, utiliser `MmapDirectory` au lieu de `StdFsDirectory`.
Cfg-gater dans `ShardedHandle::create_shard` / `open_shard` :

```rust
#[cfg(not(target_arch = "wasm32"))]
let dir = MmapDirectory::open(&shard_dir)?;

#[cfg(target_arch = "wasm32")]
let dir = StdFsDirectory::open(&shard_dir)?;
```

Avantages :
- Zero-copy reads (pas de Vec allocation pour chaque segment)
- Merges plus rapides (pas de copie des segments sources)
- File watcher natif (pas de polling)

### 2. WRITER_HEAP_SIZE trop petit (50MB)

**Impact : FORT (indexation)**

Avec SFX activé, chaque doc génère toutes les suffixes de chaque token.
Un fichier source de 50KB peut consommer ~1MB de heap dans le term dictionary.
Résultat : le writer flush un segment tous les ~60-90 docs.

90K docs → ~1000+ segments → des centaines de merges en cascade.

**Stratégie :** Augmenter à 200MB en natif :

```rust
#[cfg(not(target_arch = "wasm32"))]
const WRITER_HEAP_SIZE: usize = 200_000_000;  // 200MB

#[cfg(target_arch = "wasm32")]
const WRITER_HEAP_SIZE: usize = 15_000_000;   // 15MB
```

Avec 200MB :
- ~300-500 docs par segment au lieu de 60-90
- 4x moins de segments → beaucoup moins de merges
- Estimation : indexation 2-3x plus rapide

### 3. FsWriter deferred I/O inutile en natif

**Impact : FAIBLE**

Le FsWriter accumule tout en RAM et écrit à `terminate()`. Avant le fix WASM,
`flush()` écrivait sur disque — mais c'était déjà du buffering Vec<u8> + fs::write.

En natif on pourrait utiliser `BufWriter<File>` pour écrire au fil de l'eau, ce
qui réduirait le pic mémoire par segment. Mais le gain de vitesse serait marginal
car l'I/O n'est pas le goulot (c'est le SFX qui consomme le CPU et la mémoire).

**Stratégie :** Optionnel. Si on passe à MmapDirectory, c'est résolu car
MmapDirectory a son propre writer.

### 4. open_read fait fs::read (copie complète)

**Impact : MOYEN (reload, merge)**

Chaque `reader.reload()` relit tous les segments depuis le disque via `fs::read`.
Pour 90K docs avec beaucoup de segments, ça peut représenter des centaines de MB
copiés en mémoire à chaque commit.

**Stratégie :** Résolu automatiquement si on passe à MmapDirectory (mmap = zero-copy).

## Plan d'action recommandé

| # | Action | Impact | Effort | Dépendances |
|---|--------|--------|--------|-------------|
| 1 | WRITER_HEAP_SIZE → 200MB natif | FORT | 1 ligne | Aucune |
| 2 | MmapDirectory dans FsShardStorage | MOYEN | 2 lignes + cfg | Aucune |
| 3 | MmapDirectory dans BlobDirectory | MOYEN | même pattern | Aucune |
| 4 | Type alias `NativeDirectory` | CLEAN | ~10 lignes | Regroupe #2 et #3 |
| 5 | BufWriter<File> en natif | FAIBLE | ~30 lignes | Inutile si #2/#3 faits |

**#1 est trivial et apporte le plus gros gain.** On peut le faire immédiatement.

**#2 est aussi simple** — `LucivyHandle::create` prend `impl Directory`, donc il suffit
de changer 2 lignes dans `FsShardStorage::create_shard_handle` / `open_shard_handle` :

```rust
#[cfg(not(target_arch = "wasm32"))]
let dir = ld_lucivy::directory::MmapDirectory::open(&shard_dir)?;

#[cfg(target_arch = "wasm32")]
let dir = StdFsDirectory::open(shard_dir.to_str().unwrap())?;
```

### 4. BlobDirectory n'utilise pas mmap non plus

**Impact : MOYEN (pattern ACID)**

`BlobDirectory` (`lucivy_core/src/blob_directory.rs`) est conçu pour le pattern
ACID : stockage durable dans une DB (Postgres, S3) + cache local pour les reads.
Son commentaire dit "Read: delegate to StdFsDirectory (mmap-capable, zero-copy)"
mais c'est **faux** — il délègue à `StdFsDirectory` qui fait `fs::read` (copie
complète en Vec<u8>), pas de mmap.

C'est un bug de design : tout l'intérêt du cache local est d'avoir des reads
zero-copy via mmap. Sinon autant lire depuis la DB directement.

**Stratégie :** Remplacer le `inner: StdFsDirectory` par `MmapDirectory` en natif :

```rust
// blob_directory.rs
#[cfg(not(target_arch = "wasm32"))]
inner: ld_lucivy::directory::MmapDirectory,

#[cfg(target_arch = "wasm32")]
inner: StdFsDirectory,
```

Ou mieux : abstraire avec un type alias `NativeDirectory` utilisé partout.

## Bench de référence

```
90K docs Linux kernel (torvalds/linux shallow clone)

AVANT (debug, StdFsDirectory, WRITER_HEAP_SIZE=50MB):
  Single shard:   733s (~12 min)
  4-shard TA:     758s (~12.6 min)
  RAM peak:       20GB+ (swap 38GB)

APRES (release, MmapDirectory, WRITER_HEAP_SIZE=200MB):
  Single shard:   50s
  4-shard RR:     100s
  Queries:        147-524ms
  RAM peak:       ~14GB (pas de swap)
  Total bench:    213s (3.5 min)

Speedup single:  14.7x
Speedup sharded: ~7x
RAM reduction:   -30% + pas de swap
```

Le gain vient de trois facteurs combinés :
1. **release** vs debug : ~3-5x sur le CPU (SFX, hashing, compression)
2. **200MB heap** : ~300 docs/segment au lieu de 60-90 → 4x moins de merges
3. **MmapDirectory** : zero-copy reads, le kernel gère le paging → pas de copie RAM
