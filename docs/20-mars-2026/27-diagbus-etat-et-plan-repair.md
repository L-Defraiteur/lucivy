# Doc 27 — DiagBus : état des lieux et plan de réparation

Date : 22 mars 2026

## État actuel

### Ce qui existe

Le DiagBus (`src/diag.rs`) est un event bus global à coût zéro (atomic bool fast-path).

**6 types d'events définis** :

| Event | Rôle | Émis ? |
|-------|------|--------|
| `TokenCaptured` | Token capturé pendant l'indexation | **Oui** (segment_writer.rs) |
| `SuffixAdded` | Suffixe ajouté au FST pendant le build | Non |
| `SfxWalk` | Résultat du prefix_walk pendant une recherche | Non |
| `SfxResolve` | Ordinal résolu en doc_ids via sfxpost | Non |
| `SearchMatch` | Document matché dans un segment | Non |
| `SearchComplete` | Recherche terminée pour un segment | Non |
| `MergeDocRemapped` | Doc remappé pendant un merge | Non |

**5 filtres** : `All`, `Tokenization`, `Sfx`, `SfxTerm(term)`, `Merge`

### Ce qui ne marche pas

5 des 6 events ne sont **jamais émis** — les `emit()` correspondants n'ont jamais été câblés dans le code. Le bench utilisait `SearchMatch` et `SearchComplete` pour le ground truth → résultat : 0 docs.

### Infra en place

- `diag_bus()` : singleton global
- `diag_emit!($event)` : macro convenience (check `is_active()` + emit)
- `is_active()` : check atomique, zéro coût quand pas d'abonnés
- `subscribe(filter)` → `mpsc::Receiver<DiagEvent>`
- `clear()` : désinscrit tout, remet `active = false`

## Plan de réparation

### 1. `SfxWalk` — dans `suffix_contains_single_token_inner`

**Où** : `src/query/phrase_query/suffix_contains.rs`, après le `prefix_walk` (ligne ~110)

```rust
// Après let walk_results = ...
diag_emit!(DiagEvent::SfxWalk {
    query: query.to_string(),
    segment_id: String::new(), // pas dispo ici, voir note
    si0_entries: walk_results.iter()
        .filter(|(_, parents)| parents.iter().any(|p| p.si == 0))
        .count(),
    si_rest_entries: walk_results.iter()
        .filter(|(_, parents)| parents.iter().any(|p| p.si > 0))
        .count(),
    total_parents: walk_results.iter()
        .map(|(_, parents)| parents.len())
        .sum(),
});
```

**Problème** : `suffix_contains_single_token_inner` n'a pas accès au `segment_id`. Deux options :
- A. Passer `segment_id: Option<&str>` en paramètre (change la signature)
- B. Laisser `segment_id: String::new()` et le renseigner au niveau supérieur

**Recommandation** : Option A, c'est une info critique pour le debug.

### 2. `SfxResolve` — dans la boucle de résolution

**Où** : `suffix_contains_single_token_inner`, dans la boucle `for parent in parents` (ligne ~116)

```rust
diag_emit!(DiagEvent::SfxResolve {
    query: query.to_string(),
    segment_id: segment_id.to_string(),
    ordinal: parent.raw_ordinal as u32,
    token: suffix_term.clone(),
    si: parent.si,
    doc_count: postings.len(),
});
```

**Attention perf** : cette boucle est hot path. Le `diag_emit!` check `is_active()` (atomic), mais la construction du `DiagEvent` alloue des `String`. Il faut garder la construction derrière le check :

```rust
if crate::diag::diag_bus().is_active() {
    crate::diag::diag_bus().emit(DiagEvent::SfxResolve { ... });
}
```

Ou mieux : la macro `diag_emit!` fait déjà ce check. Mais elle évalue quand même les arguments. Pour être vraiment zero-cost :

```rust
let bus = crate::diag::diag_bus();
if bus.is_active() {
    bus.emit(DiagEvent::SfxResolve { ... });
}
```

### 3. `SearchMatch` — dans `run_sfx_walk` ou le scorer

**Où** : Deux options :
- Dans `run_sfx_walk()` (`suffix_contains_query.rs` ligne ~288), pour chaque match
- Dans `SuffixContainsScorer::advance()`, pour chaque doc émis

**Recommandation** : Dans `run_sfx_walk()`, car c'est là qu'on a les byte offsets. Le scorer n'a que `(doc_id, tf)`.

```rust
// Dans run_sfx_walk, après construction de highlights
for m in &matches {
    diag_emit!(DiagEvent::SearchMatch {
        query: query_text.to_string(),
        segment_id: String::new(), // besoin du segment_id
        doc_id: m.doc_id,
        byte_from: m.byte_from,
        byte_to: m.byte_to,
        cross_token: false, // ou vrai si continuation match
    });
}
```

**Même problème** : `run_sfx_walk` n'a pas accès au `segment_id`.

### 4. `SearchComplete` — dans `run_sfx_walk` ou le scorer

**Où** : En fin de `run_sfx_walk()`, après le count_tf_sorted.

```rust
diag_emit!(DiagEvent::SearchComplete {
    query: query_text.to_string(),
    segment_id: String::new(),
    total_docs: doc_tf.len() as u32,
});
```

### 5. `SuffixAdded` — dans le SfxCollector

**Où** : `src/suffix_fst/collector.rs` ou équivalent, quand un suffixe est ajouté au FST.

Moins prioritaire — utile pour debug build, pas search.

### 6. `MergeDocRemapped` — dans le merger sfxpost

**Où** : `src/indexer/merger.rs` ou le merge sfxpost, quand un doc_id est remappé.

Moins prioritaire — utile pour debug merge.

## Décision architecturale : segment_id

Le problème récurrent est que les fonctions bas-niveau (`suffix_contains_single_token_inner`, `run_sfx_walk`) n'ont pas accès au `segment_id`.

### Option A : Passer segment_id en paramètre

```rust
pub fn run_sfx_walk<F>(
    sfx_reader: &SfxFileReader<'_>,
    resolver: &F,
    query_text: &str,
    ...
    segment_id: Option<&str>,  // NEW
) -> (Vec<(DocId, u32)>, Vec<(DocId, usize, usize)>)
```

**Pro** : propre, info disponible partout
**Con** : change la signature publique, tous les call sites à mettre à jour

### Option B : Thread-local segment context

```rust
thread_local! {
    static DIAG_SEGMENT_ID: RefCell<String> = RefCell::new(String::new());
}
```

Le scorer set le context avant d'appeler `run_sfx_walk`, les emit lisent depuis le thread-local.

**Pro** : pas de changement de signature
**Con** : thread-local = implicite, fragile

### Option C : Émettre sans segment_id dans les fonctions bas-niveau

Les fonctions bas-niveau émettent avec `segment_id: ""`. Le caller (scorer, prescan) enrichit les events a posteriori ou souscrit avec un wrapper.

**Pro** : minimal
**Con** : events incomplets

### Recommandation

Option A. Le segment_id est une info fondamentale pour le debug. C'est un breaking change mais ces fonctions sont internes (pas d'API publique externe).

## Priorité

1. **`SearchMatch` + `SearchComplete`** dans `run_sfx_walk` — pour que le bench ground truth fonctionne
2. **`SfxWalk`** dans `suffix_contains_single_token_inner` — pour profiling du FST walk
3. **`SfxResolve`** — pour debug de posting resolution
4. **`SuffixAdded` + `MergeDocRemapped`** — basse priorité

## Note sur la perf

Le DiagBus a un fast-path atomique (`is_active()`). Quand personne ne souscrit, le coût est un `load(Relaxed)` par event site = essentiellement gratuit. Les allocations String ne se font que quand un subscriber est actif.

Pour les boucles hot path (SfxResolve), on peut batch les events et émettre un seul `SfxWalk` summary au lieu d'un `SfxResolve` par ordinal.
