# Doc 05 — Design : incremental sync pour WASM offline-first

Date : 24 mars 2026

## Contexte

Issue GitHub : un utilisateur veut synchroniser un index serveur vers des clients
browser WASM pour du offline search. Les snapshots complets deviennent impraticables
à mesure que l'index grossit. Il demande un mode delta/incrémental.

## Pourquoi c'est naturel avec notre architecture

Les segments lucivy sont **WORM** (Write Once, Read Many) :
- Un segment a un UUID unique (ex: `7e2b537a`)
- Une fois écrit, un segment ne change jamais
- Un commit crée de nouveaux segments, un merge remplace N segments par 1
- `meta.json` est la seule chose qui change : il liste les segments actifs

Un "delta" entre deux versions c'est juste :
- **Segments à ajouter** : UUIDs présents dans meta_v2 mais pas meta_v1
- **Segments à supprimer** : UUIDs présents dans meta_v1 mais pas meta_v2
- **Nouveau meta.json**

Pas besoin de diff binaire — les segments sont atomiques.

## Format du delta

```rust
pub struct IndexDelta {
    /// Version source (commit_id ou meta hash)
    pub from_version: String,
    /// Version cible
    pub to_version: String,
    /// Segments à ajouter (UUID → fichiers du segment)
    pub added_segments: Vec<SegmentBundle>,
    /// Segments à supprimer (UUID seulement)
    pub removed_segment_ids: Vec<String>,
    /// Nouveau meta.json
    pub meta: Vec<u8>,
    /// Nouveau _config.json (si changé)
    pub config: Option<Vec<u8>>,
}

pub struct SegmentBundle {
    pub segment_id: String,
    /// Tous les fichiers du segment : .term, .pos, .store, .fast, .sfx, .sfxpost, etc.
    pub files: Vec<(String, Vec<u8>)>,
}
```

## API proposée

### Serveur (natif)

```rust
// Exporter un delta depuis une version connue
let delta = lucivy::sync::export_delta(
    &index,
    since_version: "abc123",  // version que le client a
)?;
// delta.added_segments contient les fichiers des nouveaux segments
// delta.removed_segment_ids contient les UUIDs à supprimer

// Sérialiser pour transport (HTTP, WebSocket, etc.)
let bytes = delta.serialize()?;  // binaire compact

// Exporter un snapshot complet (premier sync)
let snapshot = lucivy::sync::export_snapshot(&index)?;
```

### Client (WASM)

```rust
// Premier chargement : snapshot complet
let index = lucivy::sync::load_snapshot(snapshot_bytes)?;

// Mises à jour suivantes : delta
lucivy::sync::apply_delta(&mut index, delta_bytes)?;
// → télécharge rien, les bytes sont déjà dans le delta
// → ajoute les segments dans MemoryDirectory ou IndexedDB
// → supprime les segments obsolètes
// → écrit le nouveau meta.json
// → reader.reload()
```

### HTTP endpoint (exemple)

```
GET /index/snapshot           → full snapshot (premier sync)
GET /index/delta?since=abc123 → delta depuis version abc123
GET /index/version            → version actuelle (pour polling)
```

## Comment calculer le delta

### Version = hash du meta.json

Chaque commit produit un nouveau `meta.json`. Le hash SHA-256 du meta.json
est la "version". Le client envoie sa version, le serveur compare.

```rust
pub fn compute_version(index: &Index) -> String {
    let meta_bytes = index.directory().atomic_read("meta.json")?;
    let hash = sha256(&meta_bytes);
    hex::encode(&hash[..8])  // 16 chars suffisent
}
```

### Serveur : garder les N dernières versions

Le serveur garde un historique des meta.json récents :
```rust
pub struct SyncServer {
    index: Index,
    /// Historique : version → meta.json + liste des segment UUIDs
    history: VecDeque<(String, Vec<u8>, HashSet<String>)>,
    max_history: usize,  // ex: 100 versions
}
```

À chaque commit :
1. Calculer la nouvelle version
2. Diff les segment UUIDs avec la version précédente
3. Stocker dans l'historique

Quand un client demande un delta depuis `version_X` :
1. Trouver `version_X` dans l'historique
2. Calculer les segments ajoutés/supprimés entre X et current
3. Lire les fichiers des segments ajoutés depuis le Directory
4. Retourner le delta

Si `version_X` n'est plus dans l'historique → retourner un snapshot complet.

## Stockage client WASM

### Option A : MemoryDirectory (RAM)

Simple, rapide, mais perdu au refresh. OK pour des index <50MB.

### Option B : IndexedDB via BlobStore

Le `BlobDirectory` qu'on a déjà supporte ça :
```rust
// BlobStore trait : load/save/delete/exists/list
// IndexedDbBlobStore implémente BlobStore via js-sys/web-sys
let blob_store = IndexedDbBlobStore::new("my-search-index");
let directory = BlobDirectory::new(blob_store);
let index = Index::open(directory)?;
```

Les segments sont stockés comme blobs dans IndexedDB.
Le delta ajoute/supprime des blobs. Persist au refresh.

### Option C : OPFS (Origin Private File System)

API browser plus récente, plus rapide qu'IndexedDB pour des fichiers.
Même pattern que BlobDirectory mais avec FileSystemHandle.

## Transport

### Binaire compact

Le delta est sérialisé en binaire (pas JSON) :
```
[4 bytes: version_from length] [version_from bytes]
[4 bytes: version_to length] [version_to bytes]
[4 bytes: num_added_segments]
  for each:
    [4 bytes: segment_id length] [segment_id bytes]
    [4 bytes: num_files]
      for each:
        [4 bytes: filename length] [filename bytes]
        [4 bytes: file_data length] [file_data bytes]
[4 bytes: num_removed_ids]
  for each:
    [4 bytes: id length] [id bytes]
[4 bytes: meta length] [meta bytes]
```

Compressible avec zstd (déjà dans nos deps) pour ~60-70% de réduction.

### Streaming

Pour les gros deltas, le serveur peut streamer segment par segment.
Le client applique chaque segment au fur et à mesure sans tout garder en RAM.

## Intégration avec le sharding

Pour un index shardé, chaque shard a son propre meta.json et ses propres segments.
Le delta est par shard :

```rust
pub struct ShardedDelta {
    pub shard_deltas: Vec<(usize, IndexDelta)>,  // shard_id → delta
    pub shard_config: Option<Vec<u8>>,  // _shard_config.json si changé
}
```

Le client peut ne syncer qu'un sous-ensemble de shards (ex: shard 0 et 1 sur 4)
pour un index partiel plus léger.

## Estimation de taille

Pour un index 90K docs Linux kernel (~500MB avec SFX) :
- Snapshot complet : ~500MB (une fois, compressé ~200MB)
- Delta après un commit de 100 docs : ~2-5MB (1-2 nouveaux segments)
- Delta après un merge : ~50-100MB (un gros segment remplace plusieurs petits)
  mais le client gagne en espace (le segment mergé est plus compact)

Pour un index sans SFX (`sfx: false`) :
- Snapshot complet : ~100MB (compressé ~40MB)
- Delta après commit : ~0.5-1MB

## Étapes d'implémentation

```
Phase 1 : Delta export/import (single shard)
  - IndexDelta struct + serialize/deserialize
  - export_delta(index, since_version) → IndexDelta
  - apply_delta(index, delta) → Result
  - compute_version(index) → String
  - Tests : create index, commit, export delta, apply on fresh index, search OK

Phase 2 : WASM client
  - IndexedDbBlobStore (implémente BlobStore)
  - load_snapshot / apply_delta côté WASM
  - Exemple : fetch delta → apply → search

Phase 3 : Sharded delta
  - ShardedDelta pour multi-shard
  - Partial shard sync (client choisit quels shards)

Phase 4 : SyncServer
  - Historique des versions
  - HTTP endpoints (snapshot, delta, version)
  - Compression zstd du transport
```

## Fichiers à créer

| Fichier | Contenu |
|---------|---------|
| `lucivy_core/src/sync.rs` | IndexDelta, export_delta, apply_delta, compute_version |
| `lucivy_core/src/sync_server.rs` | SyncServer, historique versions, delta calculation |
| `bindings/wasm/src/sync.rs` | WASM bindings pour load_snapshot / apply_delta |
| `lucivy_core/src/blob_store_indexeddb.rs` | IndexedDbBlobStore (future, phase 2) |
