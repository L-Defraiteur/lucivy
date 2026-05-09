# Design — Deferred I/O dans StdFsDirectory

## Problème

`StdFsDirectory::open_write()` fait de l'I/O filesystem immédiate :
1. `full.exists()` — vérification d'existence (I/O)
2. `fs::create_dir_all(parent)` — création répertoires (I/O)

En WASM/OPFS, ces opérations sont synchrones et lentes (~2-10s par appel).

`SegmentWriter::for_segment()` appelle `open_write()` 7+ fois (Store, FastFields,
FieldNorms, Terms, Postings, Positions, Offsets). Ceci se produit dans un handler
d'actor (thread scheduler) — bloquant tout le scheduler pendant que les fichiers
sont créés sur OPFS.

## Root cause observée

Pendant l'ingestion du Linux kernel (75K fichiers), les 4 indexers font un
`SegmentWriter::for_segment()` en simultané après chaque commit (batch=1, premier
doc du nouveau segment). Les 4 threads scheduler sont bloqués sur l'I/O OPFS du
`open_write()`. Le drain (qui attend que les indexers traitent leurs messages)
ne peut pas aboutir → deadlock fonctionnel.

Diagnostic observé :
```
ActorId(32) indexer: BUSY segment_writer_init batch=1 (10.0s) q:0
ActorId(23) indexer: BUSY segment_writer_init batch=1 (10.1s) q:0
ActorId(26) indexer: BUSY segment_writer_init batch=1 (10.1s) q:0
ActorId(29) indexer: BUSY segment_writer_init batch=1 (10.1s) q:0
```

## Solution — Lazy I/O dans FsWriter

### Principe

`open_write()` ne fait AUCUNE I/O. Il retourne un `FsWriter` qui garde le path
en mémoire. L'I/O filesystem (vérification d'existence, création de répertoires,
écriture) est reportée au `flush()` / `terminate()`.

Le `FsWriter` bufferise déjà en RAM (Vec<u8>). La seule modification est de
déplacer la vérification d'existence et la création des répertoires du
`open_write()` vers le `flush()`.

### Ce qui change dans StdFsDirectory

```rust
// AVANT (I/O immédiate dans open_write)
fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
    let full = self.resolve(path);
    if full.exists() {                          // ← I/O OPFS
        return Err(OpenWriteError::FileAlreadyExists(full));
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;            // ← I/O OPFS
    }
    Ok(BufWriter::new(Box::new(FsWriter::new(full))))
}

// APRÈS (zéro I/O dans open_write)
fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
    let full = self.resolve(path);
    // Pas de vérification d'existence ici — reportée au flush.
    // Le contrat WORM (Write-Once-Read-Many) garantit que les callers
    // ne créent pas deux fois le même fichier. Si ça arrive, flush() 
    // échouera en écrivant sur un fichier existant.
    Ok(BufWriter::new(Box::new(FsWriter::new(full))))
}
```

### Ce qui change dans FsWriter

```rust
// AVANT
fn flush(&mut self) -> io::Result<()> {
    self.is_flushed = true;
    fs::write(&self.path, &self.buffer)
}

// APRÈS
fn flush(&mut self) -> io::Result<()> {
    self.is_flushed = true;
    // Create parent directories on first flush (lazy).
    if let Some(parent) = self.path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&self.path, &self.buffer)
}
```

### Invariants préservés

1. **WORM (Write-Once-Read-Many)** — le contrat du Directory est que les callers
   ne créent pas deux fois le même fichier. `ManagedDirectory` (wrapper) empêche
   les duplicatas via son registre. Le check `exists()` dans `open_write` était
   une vérification redondante.

2. **Erreur sur fichier existant** — si un fichier existe déjà au moment du
   `flush()`, `fs::write()` l'écrasera silencieusement. Pour conserver le check
   d'erreur, on peut utiliser `OpenOptions::create_new(true)` dans flush au lieu
   de `fs::write`.

3. **Persistance** — les données finissent toujours sur OPFS. Juste pas au
   moment du `open_write`, mais au `flush()` / `terminate()`.

4. **ManagedDirectory** — le wrapper fait un `register_file_as_managed()` dans
   son `open_write()`. Cet enregistrement est in-memory (HashMap), pas d'I/O.
   Aucun changement nécessaire.

### Quand flush() est-il appelé ?

| Chemin | Quand | Thread |
|--------|-------|--------|
| StoreWriter blocs | Pendant add_document (quand bloc complet) | scheduler (handler) |
| SegmentSerializer.close() | Pendant finalize() | task thread (background) |
| BufWriter auto-flush | Quand buffer 8KB plein | scheduler (handler) |

**Problème potentiel** : le StoreWriter flush des blocs pendant `add_document()`,
qui tourne dans un handler d'actor. Si le flush OPFS est lent, ça bloque aussi.

### Solution complète — FsWriter::flush() ne fait pas d'I/O

Pour éliminer TOUTE I/O du handler, le `FsWriter` doit :
- Accumuler dans `self.buffer` pendant `write()` et `flush()` — pas d'I/O
- Écrire sur le filesystem uniquement au `terminate()` — qui est appelé
  pendant `SegmentSerializer::close()` → pendant `finalize()` → sur un task thread

```rust
struct FsWriter {
    path: PathBuf,
    buffer: Vec<u8>,
    written_to_disk: bool,
}

impl FsWriter {
    fn new(path: PathBuf) -> Self {
        Self { path, buffer: Vec::new(), written_to_disk: false }
    }
}

impl Write for FsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Pas d'I/O ici — tout reste en RAM.
        // Le contrat Write::flush() dit "flush buffers", mais notre
        // buffer interne c'est le Vec<u8> qui est déjà "flushed" en RAM.
        // Le write réel se fait au terminate().
        Ok(())
    }
}

impl TerminatingWrite for FsWriter {
    fn terminate_ref(&mut self, _: AntiCallToken) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(&self.path, &self.buffer)?;
        self.written_to_disk = true;
        Ok(())
    }
}

impl Drop for FsWriter {
    fn drop(&mut self) {
        if !self.written_to_disk && !self.buffer.is_empty() {
            eprintln!(
                "Warning: FsWriter for {:?} dropped with {} bytes unwritten.",
                self.path, self.buffer.len()
            );
        }
    }
}
```

### Impact mémoire

Le buffer accumule tout en RAM jusqu'au `terminate()`. Pour un segment de 500 docs :

| Composant | Taille typique | Notes |
|-----------|---------------|-------|
| Store (doc bodies compressés) | 2-10 MB | Gros fichiers .c compressés |
| Postings (termes + positions) | 5-20 MB | Dépend du vocabulaire |
| Fast fields | 1-5 MB | |
| Field norms | ~1 KB | Négligeable |
| SFX | 5-15 MB | Dépend du nombre de tokens |

**Total par segment** : ~15-50 MB, borné par le `mem_budget` existant (~60 MB par
défaut). Pas de changement de profil mémoire — les postings, fast fields et SFX
étaient DÉJÀ en RAM. Seul le Store (qui flushait des blocs incrémentalement) reste
désormais en RAM jusqu'au terminate. Surcoût : ~2-10 MB par segment.

### Ce qui NE change PAS

- `SegmentWriter` — aucun changement
- `SegmentSerializer` — aucun changement
- `StoreWriter` — aucun changement
- `ManagedDirectory` — aucun changement
- `RamDirectory` — aucun changement (déjà tout en RAM)
- `MmapDirectory` — pas concerné (natif, I/O rapide)
- `MemoryDirectory` — pas concerné (déjà tout en RAM)
- Toute la logique d'indexation, de finalize, de commit

### Ce qui change

1. **`StdFsDirectory::open_write()`** — supprime le check `exists()` et `create_dir_all()`
2. **`FsWriter::flush()`** — devient un no-op (pas d'I/O)
3. **`FsWriter::terminate_ref()`** — crée les répertoires + écrit le fichier
4. **`FsWriter` struct** — remplace `is_flushed: bool` par `written_to_disk: bool`

### Diagnostic / observabilité

Le changement n'impacte pas le diagnostic :
- Les activity labels sur les handlers montrent toujours ce que fait l'indexer
- Le finalize (task thread) montrera le temps d'écriture réel
- Si `terminate()` est lent (OPFS), ça sera visible dans les logs `[finalize]`
  mais ça ne bloquera pas le scheduler (c'est sur un task thread)

### Risques

1. **flush() menteur** — le caller pense que flush() persiste, mais on ne fait
   rien. En pratique, personne ne dépend de flush() pour la durabilité — le
   contrat du Directory dit explicitement "writes may be aggressively buffered".
   La durabilité est garantie par le commit (qui appelle terminate via finalize).

2. **Crash avant terminate** — si le process crash entre open_write et terminate,
   les données sont perdues. C'est identique au comportement actuel : un crash
   entre open_write et la fin du finalize perd le segment. Le commit atomique
   (meta.json) garantit la cohérence.

3. **Mémoire** — borné par mem_budget existant. Pas de changement de profil.

### Plan d'implémentation

1. Modifier `FsWriter` dans `lucivy_core/src/directory.rs` (~20 lignes)
2. Modifier `StdFsDirectory::open_write()` dans le même fichier (~5 lignes)
3. Run `cargo test --lib` (1200 tests)
4. Build emscripten + test playground avec Linux kernel
5. Vérifier que les WARNING `segment_writer_init` disparaissent
