# Doc 09 — Design : événements diagnostiques souscriptibles

Date : 20 mars 2026

## Le problème

Pour debugger les 20 docs manquants de "function", on a dû :
1. Écrire du code custom dans le bench
2. Compiler en release
3. Relancer 5 fois pour des erreurs de compilation
4. Analyser les résultats manuellement

On veut : **souscrire à des événements de diagnostic**, voir exactement ce qui se passe pour un doc/token/query donné, sans écrire de code custom.

## Architecture : DiagBus

Un bus d'événements typés, souscriptible par catégorie. Zéro overhead quand personne ne souscrit (atomic check).

```rust
// Souscrire
let rx = lucivy.subscribe_diag(DiagFilter::Sfx);
// ou plus fin :
let rx = lucivy.subscribe_diag(DiagFilter::SfxSearch { term: "function".into() });

// Recevoir
while let Some(event) = rx.try_recv() {
    match event {
        DiagEvent::SfxWalk { term, suffix_key, parents, partition } => { ... }
        DiagEvent::SfxResolve { ordinal, token, doc_ids } => { ... }
        _ => {}
    }
}
```

## Catégories d'événements

### Tokenization
```rust
DiagEvent::TokenProduced {
    doc_id: u32,
    field_id: u32,
    token: String,
    offset_from: usize,
    offset_to: usize,
}
DiagEvent::TokenCapturedBySfx {
    doc_id: u32,
    field_id: u32,
    token: String,
}
```

Émis par : SfxTokenInterceptor dans segment_writer.

Utilité : vérifier que le tokenizer produit les bons tokens pour chaque doc.
Pour notre bug : on verrait si "afgfunction" est bien capturé pour doc_370.

### SFX Build
```rust
DiagEvent::SfxSuffixAdded {
    token: String,
    ordinal: u64,
    suffix: String,
    si: u16,
}
```

Émis par : SuffixFstBuilder::add_token.

Utilité : vérifier que les suffixes sont bien générés.

### SFX Search
```rust
DiagEvent::SfxWalk {
    query: String,
    segment_id: String,
    partition: &str,  // "si0" ou "si_rest"
    entries: Vec<(String, Vec<ParentEntry>)>,
}
DiagEvent::SfxResolve {
    query: String,
    segment_id: String,
    ordinal: u32,
    token: String,
    doc_ids: Vec<u32>,
}
DiagEvent::SfxSearchComplete {
    query: String,
    total_docs: u32,
    total_parents: u32,
}
```

Émis par : SuffixContainsQuery, SfxFileReader::prefix_walk, PostingResolver.

Utilité : tracer le chemin complet d'un contains search.
Pour notre bug : on verrait si prefix_walk("function") trouve le suffix
de "afgfunction", et si le resolver retourne doc_370.

### Merge SFX
```rust
DiagEvent::MergeSfxToken {
    field_id: u32,
    token: String,
    source_segments: Vec<String>,
    merged_doc_ids: Vec<u32>,
}
DiagEvent::MergeSfxDocRemapped {
    token: String,
    old_segment: String,
    old_doc_id: u32,
    new_doc_id: u32,
}
```

Émis par : sfx_merge::merge_sfxpost.

Utilité : tracer les remappings de doc_ids pendant le merge.

## Implémentation

### Étape 1 : DiagBus dans ld-lucivy

```rust
// src/diagnostics/diag_bus.rs
pub struct DiagBus {
    subscribers: Mutex<Vec<(DiagFilter, Sender<DiagEvent>)>>,
    active: AtomicBool,  // fast check — skip emit if no subscribers
}

impl DiagBus {
    pub fn emit(&self, event: DiagEvent) {
        if !self.active.load(Ordering::Relaxed) { return; }
        // dispatch to matching subscribers
    }

    pub fn subscribe(&self, filter: DiagFilter) -> Receiver<DiagEvent> {
        // ...
    }
}

// Global instance (like the scheduler)
static DIAG_BUS: OnceLock<DiagBus> = OnceLock::new();
pub fn diag_bus() -> &'static DiagBus { ... }
```

### Étape 2 : Instrumenter les points clés

1. `SfxTokenInterceptor` → `TokenCapturedBySfx`
2. `SuffixFstBuilder::add_token` → `SfxSuffixAdded`
3. `SfxFileReader::prefix_walk` → `SfxWalk`
4. `PostingResolver::resolve` → `SfxResolve`
5. `sfx_merge::merge_sfxpost` → `MergeSfxToken`, `MergeSfxDocRemapped`

### Étape 3 : Exposer dans lucivy_core

```rust
// lucivy_core API
handle.subscribe_diag(DiagFilter::SfxSearch { term: "function".into() })
```

## Zéro overhead

- `DiagBus::emit()` check `active` (AtomicBool, Relaxed) → 1 ns si pas de subscribers
- Les événements ne sont construits que si `active` est true
- Pattern : `if diag_bus().is_active() { diag_bus().emit(DiagEvent::...) }`
- Ou macro : `diag_emit!(SfxWalk { ... })`

## Pour le bug actuel

Avec ce système, on pourrait faire :

```rust
let rx = handle.subscribe_diag(DiagFilter::Sfx);

// Index 5K docs
// ...

// Search "function"
let results = handle.search(query);

// Check events
for event in rx.drain() {
    if let DiagEvent::SfxResolve { ordinal, doc_ids, .. } = event {
        if !doc_ids.contains(&370) && token == "afgfunction" {
            println!("BUG: doc 370 missing from sfxpost for 'afgfunction'");
        }
    }
}
```
