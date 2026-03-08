# 10 — Design : Snapshot import/export

## Objectif

Permettre l'export/import d'un ou plusieurs index lucivy en un seul blob binaire.
Cas d'usage : backup, migration, transfert entre instances, sync entre onglets (browser).

## Format binaire : LUCE (Lucivy Unified Compact Export)

```
┌──────────────────────────────────────────────────────┐
│ Header                                               │
│   [4 octets] magic : "LUCE" (0x4C 0x55 0x43 0x45)   │
│   [4 octets] version : u32 LE (actuellement 1)       │
│   [4 octets] num_indexes : u32 LE                    │
├──────────────────────────────────────────────────────┤
│ Index 0                                              │
│   [4 octets] path_len : u32 LE                       │
│   [path_len octets] path : UTF-8                     │
│   [4 octets] num_files : u32 LE                      │
│   ┌────────────────────────────────────────────────┐ │
│   │ File 0                                         │ │
│   │   [4 octets] name_len : u32 LE                 │ │
│   │   [name_len octets] name : UTF-8               │ │
│   │   [4 octets] data_len : u32 LE                 │ │
│   │   [data_len octets] data : raw bytes           │ │
│   ├────────────────────────────────────────────────┤ │
│   │ File 1                                         │ │
│   │   ...                                          │ │
│   └────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────┤
│ Index 1                                              │
│   ...                                                │
└──────────────────────────────────────────────────────┘
```

### Choix de design

- **Little-endian force** pour tous les entiers (`u32::to_le_bytes` / `u32::from_le_bytes`). Toutes les cibles (x86, ARM, WASM) sont LE, mais on le force explicitement pour garantir la portabilite cross-platform.
- **Pas de compression** au niveau snapshot. Les fichiers de segments tantivy sont deja comprimes en lz4 en interne. Compresser par-dessus aurait un gain marginal. La compression pour le transport (HTTP, download) se fait a la couche au-dessus.
- **Pas de padding/alignment** — donnees sequentielles, pas de struct C, pas de piege cross-platform.
- **Magic + version** permettent la detection de format et l'evolution future sans casser la compatibilite.

### Limites de taille

- `data_len` en u32 → max 4 Go par fichier. Largement suffisant (les segments tantivy depassent rarement quelques centaines de Mo).
- `num_files` en u32 → max ~4 milliards de fichiers par index.
- Pas de limite sur la taille totale du snapshot (lecture streaming possible).

## Garde-fou : commit obligatoire avant export

**Erreur** (pas warning) si l'index a des changements non-commites.

Raison : sans commit, les documents ajoutes vivent dans le buffer memoire de l'IndexWriter et ne sont PAS materialises dans les fichiers du Directory. Le snapshot serait silencieusement incomplet.

### Detection

Tantivy expose `IndexWriter::opstamp()` qui retourne l'operation stamp courant. Apres un `commit()`, l'opstamp du writer et celui du dernier meta.json correspondent. Si ils divergent → pending operations → erreur.

Alternative plus simple : ajouter un flag `has_uncommitted: bool` dans `LucivyHandle` qui passe a `true` sur `add/remove/update` et repasse a `false` sur `commit/rollback`. Plus fiable car ne depend pas de l'API interne tantivy.

**Choix retenu** : flag `has_uncommitted` dans `LucivyHandle`.

## Implementation

### 1. lucivy-core : `snapshot.rs`

Nouvelles fonctions dans `lucivy_core::snapshot` :

```rust
/// Erreur si uncommitted changes.
pub fn export_snapshot(indexes: &[(&str, &LucivyHandle)]) -> Result<Vec<u8>, String>

/// Retourne Vec<(path, Vec<(filename, data)>)>.
pub fn import_snapshot(data: &[u8]) -> Result<Vec<(String, Vec<(String, Vec<u8>)>)>, String>
```

`export_snapshot` :
1. Pour chaque index, verifier `has_uncommitted == false`, sinon erreur.
2. Lire tous les fichiers du Directory via la liste des segments dans meta.json + `_config.json`.
3. Serialiser au format LUCE.

`import_snapshot` :
1. Valider magic + version.
2. Deserialiser en liste de (path, fichiers).
3. L'appelant (binding) decide quoi faire : creer des MemoryDirectory, ecrire sur disque, etc.

### 2. StdFsDirectory : accesseur `root()`

Ajouter `pub fn root(&self) -> &Path` pour que les bindings natifs puissent lister les fichiers du repertoire d'un index.

Alternative : fonction standalone `list_directory_files(path: &Path) -> Vec<(String, Vec<u8>)>` qui lit tous les fichiers d'un repertoire. Plus propre car ne requiert pas d'acceder a l'implementation concrete du Directory.

**Choix retenu** : fonction standalone, decouplage du type Directory.

### 3. LucivyHandle : tracking uncommitted

```rust
pub struct LucivyHandle {
    // ... champs existants ...
    pub has_uncommitted: AtomicBool,  // nouveau
}
```

- `add_document` / `delete_term` → `has_uncommitted.store(true)`
- `commit` / `rollback` → `has_uncommitted.store(false)`

Probleme : ces operations sont dans les bindings, pas dans LucivyHandle.
Solution : ajouter des methodes wrapper dans LucivyHandle :

```rust
impl LucivyHandle {
    pub fn add_doc(&self, doc: LucivyDocument) -> Result<(), String> {
        let writer = self.writer.lock()...;
        writer.add_document(doc)?;
        self.has_uncommitted.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn delete_doc(&self, doc_id: u64) -> Result<(), String> {
        let field = self.field(NODE_ID_FIELD)...;
        let term = Term::from_field_u64(field, doc_id);
        let writer = self.writer.lock()...;
        writer.delete_term(term);
        self.has_uncommitted.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn commit(&self) -> Result<(), String> {
        let mut writer = self.writer.lock()...;
        writer.commit()?;
        self.reader.reload()?;
        self.has_uncommitted.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), String> {
        let mut writer = self.writer.lock()...;
        writer.rollback()?;
        self.has_uncommitted.store(false, Ordering::Relaxed);
        Ok(())
    }
}
```

Bonus : ces wrappers simplifient aussi le code duplique dans chaque binding (add/remove/commit/rollback sont quasi-identiques dans les 5 bindings aujourd'hui). Refactoring optionnel a ce stade.

### 4. Bindings

#### Python

```python
# Export
snapshot_bytes = index.export_snapshot()  # bytes
index.export_snapshot_to(path)           # ecrit directement dans un fichier

# Import
index = Index.import_snapshot(snapshot_bytes)
index = Index.import_snapshot_from(path)

# Multi-index
snapshot_bytes = lucivy.export_snapshots([index1, index2])
indexes = lucivy.import_snapshots(snapshot_bytes)
```

#### Node.js natif

```javascript
// Export
const buffer = index.exportSnapshot();       // Buffer
index.exportSnapshotTo(path);                // ecrit dans un fichier

// Import
const index = Index.importSnapshot(buffer);
const index = Index.importSnapshotFrom(path);

// Multi-index
const buffer = Lucivy.exportSnapshots([index1, index2]);
const indexes = Lucivy.importSnapshots(buffer);
```

#### C++

```cpp
// Export
std::vector<uint8_t> data = index.export_snapshot();
index.export_snapshot_to(path);

// Import
auto index = Index::import_snapshot(data);
auto index = Index::import_snapshot_from(path);
```

#### WASM (wasm-bindgen)

```javascript
// Export — retourne Uint8Array
const snapshot = index.exportSnapshot();

// Import — accepte Uint8Array
const index = Index.importSnapshot(snapshot);
```

#### Emscripten

```javascript
// Export — via FFI, retourne base64 (ou mieux : pointeur + longueur)
const snapshotPtr = Module.ccall('lucivy_export_snapshot', 'number', ['number'], [ctx]);
const snapshotLen = Module.ccall('lucivy_export_snapshot_len', 'number', [], []);
const snapshot = Module.HEAPU8.slice(snapshotPtr, snapshotPtr + snapshotLen);

// Import — via FFI
const ctx = Module.ccall('lucivy_import_snapshot', 'number',
    ['number', 'number'], [dataPtr, dataLen]);
```

Note : pour emscripten, eviter le base64 (overhead 33%). Passer par pointeur + longueur directement.

## Listing des fichiers d'un index

Pour `StdFsDirectory` (natif), il faut lister tous les fichiers du repertoire de l'index. Fonction utilitaire :

```rust
pub fn read_directory_files(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            let data = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
            files.push((name, data));
        }
    }
    Ok(files)
}
```

Pour `MemoryDirectory` (wasm/emscripten), `export_all()` existe deja.

## Ecriture des fichiers a l'import

Pour `StdFsDirectory` : ecrire chaque fichier dans le repertoire cible.
Pour `MemoryDirectory` : `import_file()` pour chaque fichier, puis `LucivyHandle::open()`.

## Resume des fichiers a creer/modifier

| Fichier | Action |
|---------|--------|
| `lucivy_core/src/snapshot.rs` | **nouveau** — format LUCE, serialize/deserialize |
| `lucivy_core/src/handle.rs` | modifier — ajouter `has_uncommitted: AtomicBool` |
| `lucivy_core/src/lib.rs` | modifier — `pub mod snapshot;` |
| `bindings/python/src/lib.rs` | modifier — ajouter `export_snapshot`, `import_snapshot` |
| `bindings/nodejs/src/lib.rs` | modifier — ajouter `exportSnapshot`, `importSnapshot` |
| `bindings/cpp/src/lib.rs` | modifier — ajouter `export_snapshot`, `import_snapshot` |
| `bindings/wasm/src/lib.rs` | modifier — ajouter `exportSnapshot`, `importSnapshot` |
| `bindings/emscripten/src/lib.rs` | modifier — ajouter `lucivy_export_snapshot`, `lucivy_import_snapshot` |

## Ordre d'implementation

1. `has_uncommitted` dans LucivyHandle
2. `snapshot.rs` dans lucivy-core (format + serialize/deserialize)
3. Python binding (le plus facile a tester — 64 tests existants)
4. Node.js binding
5. C++ binding
6. WASM binding
7. Emscripten binding
8. Tests pour chaque binding
