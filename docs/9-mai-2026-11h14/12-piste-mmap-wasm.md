# Piste : mmap dans WASM linear memory

## Source

Trouvé sur le web — ncruces/go-sqlite3 avec wazero :
https://github.com/ncruces/go-sqlite3/blob/v0.14.0/internal/util/alloc.go

> "I had a requirement to map files read-write to guest linear memory, for my
> Go bindings to SQLite using wazero. This was achieved, and is working on
> Linux/macOS by using a custom linear memory allocator, then doing an
> aligned_alloc with wasi-libc (to hide some memory from malloc) and mmapping
> the file into that memory."

## Principe

La mémoire linéaire WASM est un gros tableau contigu. Si le runtime WASM
permet de contrôler l'allocation de cette mémoire (ex: wazero en Go), on peut :

1. Réserver une zone via `aligned_alloc` (wasi-libc) — la cache de `malloc`
2. Demander au host de `mmap` un fichier directement dans cette zone
3. Le code WASM accède au fichier via des pointeurs normaux — zero-copy

## Applicabilité à lucivy

### Emscripten / Browser : NON (pour l'instant)

- Le SharedArrayBuffer est géré par le browser, pas par nous
- OPFS n'expose pas de mmap (c'est une API async read/write)
- On ne contrôle pas l'allocateur de la linear memory
- Pas de syscall mmap disponible dans le browser sandbox

### Runtimes WASM côté serveur : POTENTIELLEMENT

- **wasmtime** (Rust) : supporte les custom memory allocators via `Config::with_host_memory`
- **wasmer** (Rust) : supporte les custom linear memory via `LinearMemory` trait
- **wazero** (Go) : c'est exactement ce que ncruces fait

Si un jour lucivy tourne en WASI (serveur), on pourrait :
1. Compiler lucivy en WASM target WASI
2. Utiliser un runtime qui expose mmap dans la linear memory
3. Le Directory impl utiliserait des pointeurs raw dans la linear memory
4. Zero-copy reads sur les segments d'index

## Pourquoi c'est intéressant

Le problème actuel en WASM : tout passe par `StdFsDirectory` → `fs::read` →
copie complète en Vec<u8>. Pour un index de 90K docs, ça peut dépasser les 4GB
de linear memory WASM32.

Avec mmap dans la linear memory : les segments seraient mappés directement,
le runtime gère le paging, et on reste sous la limite mémoire tant que les
working sets sont raisonnables.

## Pour plus tard

Ce n'est pas actionnable maintenant. A garder en tête pour :
- Un mode serveur WASI (wasmtime/wasmer)
- Si OPFS expose un jour une API de memory-mapping
- Si les browsers permettent de contrôler la linear memory allocation
