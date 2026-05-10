# Design : auto doc_id avec allocateur par ranges libres (BTree)

## Principe

Un `BTreeMap<u64, u64>` de ranges libres `[start, end)`. L'auto-alloc prend le plus petit ID libre. Les IDs manuels "perforent" les ranges. Les deletes réinsèrent l'ID et les ranges adjacents fusionnent.

## Structures

```rust
struct IdAllocator {
    /// Ranges libres [start, end). BTreeMap<start, end>.
    free: BTreeMap<u64, u64>,
}
```

## Opérations

### Création

```
new() → free = {0: u64::MAX}   // tout l'espace est libre
```

### Auto-alloc (add sans doc_id)

```
allocate() → u64:
    let (start, end) = free.first()   // plus petit range
    let id = start
    if start + 1 == end:
        free.remove(start)            // range épuisé
    else:
        free.remove(start)
        free.insert(start + 1, end)   // shrink range
    return id
```

### Réservation manuelle (add avec doc_id=42)

```
reserve(42):
    // Trouver le range qui contient 42
    let (start, end) = range contenant 42
    free.remove(start)
    if start < 42:
        free.insert(start, 42)        // range gauche
    if 42 + 1 < end:
        free.insert(43, end)          // range droit
    // Si 42 n'est dans aucun range libre → déjà occupé → warning
```

### Delete

```
release(5):
    free.insert(5, 6)                 // réinsère [5, 6)
    merge_adjacent()                  // fusionne avec voisins
```

Fusion : si `free` contient `[3, 5)` et `[5, 6)` → merge en `[3, 6)`.
Si aussi `[6, 9)` → merge en `[3, 9)`.

### Delta apply

```
apply_delta_ids(ids: &[u64]):
    for id in ids:
        reserve(id)                   // marque comme occupé
```

## Exemple complet

```
new()                           → {0: MAX}
allocate() → 0                  → {1: MAX}
allocate() → 1                  → {2: MAX}
allocate() → 2                  → {3: MAX}
reserve(9)                      → {3: 9, 10: MAX}
allocate() → 3                  → {4: 9, 10: MAX}
allocate() → 4                  → {5: 9, 10: MAX}
reserve(7)                      → {5: 7, 8: 9, 10: MAX}
allocate() → 5                  → {6: 7, 8: 9, 10: MAX}
allocate() → 6                  → {8: 9, 10: MAX}
delete(2)                       → {2: 3, 8: 9, 10: MAX}
allocate() → 2                  → {8: 9, 10: MAX}
allocate() → 8                  → {10: MAX}
allocate() → 10                 → {11: MAX}
delete(0)                       → {0: 1, 11: MAX}
delete(1)                       → {0: 2, 11: MAX}  // fusion [0,1) + [1,2) → [0,2)
```

## Persistance : `_id_alloc.json`

```json
{"free": [[5, 7], [8, 9], [10, 18446744073709551615]]}
```

- Écrit à chaque `commit()`
- Lu à l'`open()`
- Si absent (vieux index) : scan max _node_id → `free = {max+1: MAX}`
  (on ne recycle pas les anciens trous — safe default)

## Warning on overwrite

```rust
fn reserve(&mut self, id: u64) -> bool {
    // returns true if was free, false if already occupied
    if !self.is_free(id) {
        eprintln!("[lucivy] warning: doc_id={id} already exists, overwriting");
        return false;
    }
    // ... remove from free ranges ...
    true
}
```

## API bindings

### Python

```python
# Auto-ID (recommandé)
doc_id = index.add(title="Hello", body="World")       # → 0
doc_id = index.add(title="Foo", body="Bar")            # → 1

# Explicit ID (avancé)
doc_id = index.add(title="Custom", body=".", doc_id=9) # → 9

doc_id = index.add(title="Next", body=".")             # → 2 (pas 10!)

# Delete libère l'ID
index.delete(0)
doc_id = index.add(title="Reuse", body=".")            # → 0 (recyclé)
```

### Node.js

```js
const id = index.add({title: "Hello"})          // auto → 0
const id2 = index.add({title: "X"}, {docId: 9}) // manual → 9
const id3 = index.add({title: "Y"})             // auto → 1
```

### C++ / Emscripten

```
lucivy_add(ctx, 0, 0, fields)  → auto (doc_id high=0, low=0 → auto)
lucivy_add(ctx, 9, 0, fields)  → manual id=9
```

Convention C : `doc_id_hi=0, doc_id_lo=0` → auto-alloc. Retourne l'ID alloué.
Ou un flag séparé / fonction `lucivy_add_auto`.

## Implémentation

### lucivy_core/src/id_allocator.rs (nouveau)

```rust
use std::collections::BTreeMap;

pub struct IdAllocator {
    free: BTreeMap<u64, u64>,
}

impl IdAllocator {
    pub fn new() -> Self {
        let mut free = BTreeMap::new();
        free.insert(0, u64::MAX);
        Self { free }
    }

    pub fn from_next_id(next: u64) -> Self {
        let mut free = BTreeMap::new();
        if next < u64::MAX {
            free.insert(next, u64::MAX);
        }
        Self { free }
    }

    pub fn allocate(&mut self) -> u64 {
        let (&start, &end) = self.free.iter().next()
            .expect("IdAllocator exhausted");
        self.free.remove(&start);
        if start + 1 < end {
            self.free.insert(start + 1, end);
        }
        start
    }

    pub fn reserve(&mut self, id: u64) -> bool {
        // Find the range containing id
        let range = self.free.range(..=id).next_back()
            .and_then(|(&s, &e)| if id < e { Some((s, e)) } else { None });

        let Some((start, end)) = range else {
            return false; // already occupied
        };

        self.free.remove(&start);
        if start < id {
            self.free.insert(start, id);
        }
        if id + 1 < end {
            self.free.insert(id + 1, end);
        }
        true
    }

    pub fn release(&mut self, id: u64) {
        // Insert [id, id+1) then merge adjacent
        let mut new_start = id;
        let mut new_end = id + 1;

        // Merge with range ending at id
        if let Some((&s, &e)) = self.free.range(..id).next_back() {
            if e == id {
                new_start = s;
                self.free.remove(&s);
            }
        }
        // Merge with range starting at id+1
        if let Some(&e) = self.free.get(&(id + 1)) {
            new_end = e;
            self.free.remove(&(id + 1));
        }
        self.free.insert(new_start, new_end);
    }

    pub fn is_free(&self, id: u64) -> bool {
        self.free.range(..=id).next_back()
            .is_some_and(|(&s, &e)| id >= s && id < e)
    }

    pub fn to_ranges(&self) -> Vec<(u64, u64)> {
        self.free.iter().map(|(&s, &e)| (s, e)).collect()
    }

    pub fn from_ranges(ranges: Vec<(u64, u64)>) -> Self {
        let free: BTreeMap<u64, u64> = ranges.into_iter().collect();
        Self { free }
    }
}
```

### Intégration ShardedHandle

- `ShardedHandle` : ajouter `id_alloc: Mutex<IdAllocator>`
- `create()` : `IdAllocator::new()`
- `open()` : lire `_id_alloc.json`, fallback `from_next_id(scan_max + 1)`
- `commit()` : écrire `_id_alloc.json`
- `add_document()` : si auto → `allocate()`, si manuel → `reserve()` + warning
- `delete_by_node_id()` : `release(id)`
- `apply_sharded_delta()` : pour chaque doc ajouté → `reserve(id)`

## Compatibilité

- **LUCID/LUCIDS** : pas affectés (basés sur segments)
- **Vieux index** : scan max _node_id → `from_next_id(max+1)`, pas de recyclage des trous
- **Snapshot import** : reconstruire l'allocateur depuis les docs importés
- **Multi-writer** : pas supporté (lock exclusif), pas de conflit
