# Plan : Writer Thread dédié pour WASM/emscripten

## Problème

Sur emscripten avec pthreads, tout appel `ccall` bloquant sur le thread event-loop
du Web Worker cause un deadlock. L'`IndexWriter` de ld-lucivy utilise en interne :

- 1 thread indexeur (reçoit docs via channel, écrit les segments)
- 1 thread segment_updater (gère commits, segments, merges)
- 4 threads de merge (rayon pool)

`commit()` fait : join indexer → respawn indexer → `schedule_commit().wait()`.
Tout ça bloque le thread appelant en attendant les autres threads. Si ce thread
est l'event loop JS, les messages `postMessage` pour la coordination pthread ne
passent plus → deadlock.

Tentatives échouées : ASYNCIFY, PROXY_TO_PTHREAD, setTimeout yields, commit
non-bloquant begin/poll — aucune n'a résolu le problème de manière fiable.

## Architecture cible

**Writer thread permanent + accès reader direct.**

```
JS Worker event loop                 Rust writer thread (pthread permanent)
────────────────────                 ─────────────────────────────────────
                                     loop { Atomics.wait(cmd_ready) }

index.add(doc) →
  write cmd to SharedArrayBuffer
  Atomics.notify(cmd_ready)     →    wake, read cmd
  Atomics.waitAsync(result_ready)    writer.add_document(...)
     ↓ (Promise, non-bloquant)       store result
                                     Atomics.notify(result_ready)  →  Promise resolves
  return result                      Atomics.wait(cmd_ready)

index.search(query) →
  Module.ccall('lucivy_search')      (direct, pas de thread intermédiaire)
  return result
```

### Principes

1. **L'event loop JS n'est jamais bloqué.** Toutes les mutations passent par le
   writer thread via SharedArrayBuffer + Atomics. Le JS utilise
   `Atomics.waitAsync()` qui retourne une Promise native — zéro polling.

2. **Le hot path (search) est direct.** Les opérations de lecture (search,
   numDocs, schema, export) ne font pas de coordination de threads — pas de
   join, pas de spawn. On peut `ccall` directement sans deadlock.

3. **Le writer thread est un pthread normal.** Il peut bloquer, joindre des
   threads, en créer — l'IndexWriter fonctionne sans modification.

4. **Threads permanents et réutilisables.** Le writer thread vit pour toute la
   session. Les threads internes d'IndexWriter (indexer, merger, segment_updater)
   vivent sous lui normalement.

## Répartition des opérations

### Via writer thread (mutations)

| Opération       | Pourquoi                                          |
|-----------------|---------------------------------------------------|
| `create`        | Crée IndexWriter → spawne threads internes        |
| `open`          | Idem                                              |
| `add`           | Écrit dans le channel de l'indexeur                |
| `addMany`       | Idem                                              |
| `remove`        | Modifie le delete queue                            |
| `update`        | remove + add                                      |
| `commit`        | Join indexeur, spawn nouveau, wait segment_updater |
| `rollback`      | Similaire à commit                                |
| `destroy`       | Drop IndexWriter → join threads                   |
| `importSnapshot`| Crée un nouvel IndexWriter                        |

### Via ccall direct (lectures)

| Opération          | Pourquoi                                     |
|--------------------|----------------------------------------------|
| `search`           | Lecture seule via Searcher/IndexReader        |
| `searchFiltered`   | Idem                                         |
| `numDocs`          | Lecture reader                               |
| `schema`           | Lecture statique                              |
| `exportSnapshot`   | Lecture segments                              |
| `exportDirty`      | Lecture directory diff                        |
| `exportAll`        | Lecture directory                             |

## Implémentation Rust

### Nouvelles structures

```rust
/// Canal de commande entre JS et le writer thread.
struct CommandChannel {
    /// SharedArrayBuffer exposé au JS pour les signaux atomiques.
    cmd_ready: *mut i32,     // JS écrit 1 + Atomics.notify → writer wakes
    result_ready: *mut i32,  // writer écrit 1 + atomic_notify → JS promise resolves
    /// Commande courante (type + args sérialisés).
    cmd_buf: Mutex<Option<Command>>,
    /// Résultat courant.
    result_buf: Mutex<Option<String>>,
}

enum Command {
    Create { path: String, fields_json: String, stemmer: String },
    Add { doc_id: u32, fields_json: String },
    AddMany { docs_json: String },
    Remove { doc_id: u32 },
    Commit,
    Rollback,
    Destroy,
    ImportSnapshot { data: Vec<u8>, path: String },
    // open = OpenBegin + ImportFile* + OpenFinish groupés
    Open { path: String, files: Vec<(String, Vec<u8>)> },
}
```

### Writer thread loop

```rust
fn writer_thread_main(channel: Arc<CommandChannel>) {
    let mut ctx: Option<LucivyContext> = None;

    loop {
        // Dormir jusqu'au prochain signal JS
        unsafe { core::arch::wasm32::memory_atomic_wait32(channel.cmd_ready, 0, -1); }
        (*channel.cmd_ready).store(0, Ordering::SeqCst);

        let cmd = channel.cmd_buf.lock().unwrap().take();
        let result = match cmd {
            Some(Command::Create { path, fields_json, stemmer }) => {
                // Crée l'index — peut spawner des threads, c'est OK
                match do_create(&path, &fields_json, &stemmer) {
                    Ok(new_ctx) => { ctx = Some(new_ctx); Ok("ok".into()) }
                    Err(e) => Err(e),
                }
            }
            Some(Command::Add { doc_id, fields_json }) => {
                do_add(ctx.as_mut().unwrap(), doc_id, &fields_json)
            }
            Some(Command::Commit) => {
                do_commit(ctx.as_mut().unwrap())  // peut bloquer, joindre, etc.
            }
            // ... autres commandes
            None => continue,
        };

        // Stocker le résultat et signaler JS
        *channel.result_buf.lock().unwrap() = Some(match result {
            Ok(s) => s,
            Err(e) => format!(r#"{{"error":"{}"}}"#, e),
        });
        (*channel.result_ready).store(1, Ordering::SeqCst);
        unsafe { core::arch::wasm32::memory_atomic_notify(channel.result_ready, 1); }
    }
}
```

### FFI exports

```rust
/// Initialise le writer thread. Appelé une seule fois au démarrage.
/// Retourne un pointeur vers le SharedArrayBuffer des signaux.
#[no_mangle]
pub extern "C" fn lucivy_init_writer_thread() -> *mut i32 { ... }

/// Envoie une commande au writer thread. Non-bloquant.
#[no_mangle]
pub unsafe extern "C" fn lucivy_send_command(cmd_json: *const c_char) { ... }

/// Lit le dernier résultat. Appelé par JS après que Atomics.waitAsync resolve.
#[no_mangle]
pub unsafe extern "C" fn lucivy_read_result() -> *const c_char { ... }

// Lectures directes (inchangées)
#[no_mangle]
pub unsafe extern "C" fn lucivy_search(...) -> *const c_char { ... }
// etc.
```

## Implémentation JS (lucivy-worker.js)

```js
let signalBuf;  // Int32Array over SharedArrayBuffer [cmd_ready, result_ready]

async function initWriterThread() {
    const ptr = Module.ccall('lucivy_init_writer_thread', 'number', [], []);
    // ptr pointe vers 2 i32 dans la mémoire WASM partagée
    signalBuf = new Int32Array(Module.HEAPU8.buffer, ptr, 2);
}

async function sendCommand(cmd) {
    const json = JSON.stringify(cmd);
    // Écrire la commande
    Module.ccall('lucivy_send_command', null, ['string'], [json]);

    // Signaler le writer thread
    Atomics.store(signalBuf, 0, 1);   // cmd_ready = 1
    Atomics.notify(signalBuf, 0, 1);  // wake writer thread

    // Attendre le résultat (non-bloquant, retourne une Promise)
    await Atomics.waitAsync(signalBuf, 1, 0).value;
    Atomics.store(signalBuf, 1, 0);   // reset result_ready

    // Lire le résultat
    const resultPtr = Module.ccall('lucivy_read_result', 'number', [], []);
    return Module.UTF8ToString(resultPtr);
}

// Handler simplifié
self.onmessage = async (e) => {
    const { type, id, ...args } = e.data;
    let result;

    switch (type) {
        case 'init':
            // ... import module ...
            await initWriterThread();
            result = true;
            break;

        // Mutations → writer thread
        case 'create':
        case 'add':
        case 'commit':
        case 'rollback':
            const res = await sendCommand({ type, ...args });
            result = JSON.parse(res);
            break;

        // Lectures → ccall direct
        case 'search':
            result = JSON.parse(callStr('lucivy_search', ...));
            break;
    }

    self.postMessage({ id, result });
};
```

## Avantages de cette architecture

| Aspect         | Avant (ccall direct)      | Après (writer thread)          |
|----------------|---------------------------|--------------------------------|
| Event loop     | Bloqué par commit/destroy | Jamais bloqué                  |
| Search latence | ~identique                | Identique (ccall direct)       |
| Polling        | —                         | Aucun (Atomics.waitAsync)      |
| Threads        | Deadlock possible         | Aucun deadlock possible        |
| Code lucivy    | Workarounds partout       | IndexWriter inchangé           |
| Overhead       | —                         | 1 atomic notify par mutation   |

## Étapes d'implémentation

1. **Rust : `CommandChannel` + writer thread loop** — les structures, le spawn
   du thread au init, la boucle de traitement.

2. **Rust : FFI `init_writer_thread`, `send_command`, `read_result`** — les
   exports C pour la communication JS↔Rust.

3. **Rust : refactor des fonctions existantes** — extraire la logique de
   `lucivy_create`, `lucivy_add`, `lucivy_commit` etc. dans des fonctions
   `do_create`, `do_add`, `do_commit` réutilisables par le writer thread.

4. **JS : `sendCommand` + `Atomics.waitAsync`** — le côté JS du canal.

5. **JS : refactor du worker** — mutations via `sendCommand`, lectures via
   `ccall` direct.

6. **build.sh** — retirer ASYNCIFY et PROXY_TO_PTHREAD (plus nécessaires),
   exporter les nouvelles fonctions. Garder `-pthread` et `PTHREAD_POOL_SIZE`.

7. **Test** — importer un .md dans le playground, vérifier que commit passe.

## Notes

- `Atomics.waitAsync` : Chrome 87+, Safari 16.4+, Firefox 80+ (flag).
  Suffisant pour un playground et rag3db en production.

- Le `SharedArrayBuffer` est déjà disponible car on compile avec `-pthread`
  (emscripten met les bons headers COOP/COEP).

- Les lectures (search) accèdent au `LucivyContext` depuis le thread JS
  pendant que le writer thread peut aussi y accéder. Il faut s'assurer que
  le reader est `Sync` (c'est le cas : `IndexReader` est thread-safe).
  On ne fait jamais search pendant un commit car le JS est séquentiel
  côté caller.

- Pour les index multiples (source + user dans le playground), le writer
  thread gère une `HashMap<String, LucivyContext>` par path.
