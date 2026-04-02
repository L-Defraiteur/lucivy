# 01 — Unification du trait SfxIndexFile

Date : 2 avril 2026

## Problème

Deux abstractions coexistent pour les index SFX :

1. `SfxIndexFile` (ancien) — `id()`, `extension()`, `build()`, `merge()`
   - Utilisé pour GC (`all_components`), chargement (`load_sfx_files`),
     et anciennement pour build/merge (remplacé par le single-pass)
   - `build()` et `merge()` ne sont plus appelés nulle part

2. `SfxDerivedIndex` (nouveau) — `id()`, `extension()`, `on_token()`,
   `on_posting()`, `depends_on()`, `build_from_deps()`, `serialize()`
   - Utilisé par `build_derived_indexes()` dans WriteSfxNode et SfxCollector
   - Fonctionne bien mais c'est un trait séparé

Conséquences :
- Deux registres (`all_indexes()` et `all_derived_indexes()`)
- Redondance (id/extension déclarés deux fois pour posmap, bytemap, etc.)
- Confusion : quel trait utiliser pour quoi ?

## Solution : un seul trait avec `IndexKind`

### Le trait unifié

```rust
pub enum IndexKind {
    /// Index géré par un node DAG dédié (sfxpost, gapmap, sibling).
    /// Le trait sert pour GC + chargement + serialize.
    /// Pas d'events — les données viennent du DAG.
    Primary,
    /// Index construit par la single-pass events (posmap, bytemap, termtexts).
    /// on_token/on_posting appelés pendant la boucle tokens+sfxpost.
    Derived,
    /// Index construit après les dérivés (sepmap).
    /// build_from_deps() reçoit les données des Derived déjà sérialisés.
    DerivedWithDeps,
}

pub trait SfxIndexFile: Send {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;
    fn kind(&self) -> IndexKind;

    // ── Events (Derived + DerivedWithDeps) ──
    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _pos: u32,
                  _byte_from: u32, _byte_to: u32) {}

    // ── Deps (DerivedWithDeps seulement) ──
    fn depends_on(&self) -> Vec<&'static str> { vec![] }
    fn build_from_deps(&mut self, _ctx: &SfxDeriveContext) {}

    // ── Output ──
    fn serialize(&self) -> Vec<u8>;
}
```

### Un seul registre

```rust
pub fn all_indexes() -> Vec<Box<dyn SfxIndexFile>> {
    vec![
        // Primary (gérés par nodes DAG, pas d'events)
        Box::new(SfxPostIndex::new()),
        Box::new(GapMapIndex::new()),
        Box::new(SiblingIndex::new()),
        // Derived (events: on_token / on_posting)
        Box::new(PosMapIndex::new()),
        Box::new(ByteMapIndex::new()),
        Box::new(TermTextsIndex::new()),
        // DerivedWithDeps (depends_on + build_from_deps)
        Box::new(SepMapIndex::new()),
    ]
}
```

### Utilisateurs

| Utilisateur | Ce qu'il fait |
|-------------|--------------|
| `segment_component.rs` (GC) | `all_indexes()` → `id()`, `extension()` |
| `segment_reader.rs` (chargement) | `all_indexes()` → `id()`, `extension()` |
| `build_derived_indexes()` | filtre `kind() != Primary` → events → deps → serialize |
| WriteSfxNode | écrit les Primary directement, appelle `build_derived_indexes()` |
| SfxCollector | construit sfxpost directement, appelle `build_derived_indexes()` |

### `build_derived_indexes()` adapté

```rust
pub fn build_derived_indexes(
    tokens: &BTreeSet<String>,
    sfxpost_data: Option<&[u8]>,
    gapmap_data: &[u8],
    num_docs: u32,
) -> Vec<(String, Vec<u8>)> {
    let mut indexes = all_indexes();

    // Phase 1: events (Derived + DerivedWithDeps)
    let sfxpost_reader = sfxpost_data.and_then(SfxPostReaderV2::open_slice);
    for (ord, token) in tokens.iter().enumerate() {
        let ord = ord as u32;
        for idx in indexes.iter_mut() {
            if matches!(idx.kind(), IndexKind::Derived | IndexKind::DerivedWithDeps) {
                idx.on_token(ord, token);
            }
        }
        if let Some(ref reader) = sfxpost_reader {
            for entry in reader.entries(ord) {
                for idx in indexes.iter_mut() {
                    if matches!(idx.kind(), IndexKind::Derived | IndexKind::DerivedWithDeps) {
                        idx.on_posting(ord, entry.doc_id, entry.token_index,
                                       entry.byte_from, entry.byte_to);
                    }
                }
            }
        }
    }

    // Phase 2: serialize Derived
    let mut built: HashMap<String, Vec<u8>> = HashMap::new();
    for idx in indexes.iter() {
        if matches!(idx.kind(), IndexKind::Derived) {
            let data = idx.serialize();
            if !data.is_empty() {
                built.insert(idx.id().to_string(), data);
            }
        }
    }

    // Phase 3: build DerivedWithDeps
    for idx in indexes.iter_mut() {
        if matches!(idx.kind(), IndexKind::DerivedWithDeps) {
            let ctx = SfxDeriveContext {
                derived: &built,
                gapmap_data,
                num_docs,
            };
            idx.build_from_deps(&ctx);
            let data = idx.serialize();
            if !data.is_empty() {
                built.insert(idx.id().to_string(), data);
            }
        }
    }

    // Return (extension, data) for non-Primary
    indexes.iter()
        .filter(|idx| !matches!(idx.kind(), IndexKind::Primary))
        .filter_map(|idx| built.remove(idx.id()).map(|d| (idx.extension().to_string(), d)))
        .collect()
}
```

## Ce qui est supprimé

- Trait `SfxDerivedIndex` (fusionné dans `SfxIndexFile`)
- Fonction `all_derived_indexes()` (fusionnée dans `all_indexes()`)
- Structs `DerivedPosMap`, `DerivedByteMap`, `DerivedTermTexts`, `DerivedSepMap`
  (fusionnées dans `PosMapIndex`, `ByteMapIndex`, `TermTextsIndex`, `SepMapIndex`)
- `SfxBuildContext` et `SfxMergeContext` (plus utilisés)
- Ancien `build()` et `merge()` sur le trait (remplacés par events + deps)

## Ce qui change par struct

| Struct | Avant | Après |
|--------|-------|-------|
| SfxPostIndex | `SfxIndexFile { build, merge }` | `SfxIndexFile { kind: Primary, serialize }` |
| GapMapIndex | `SfxIndexFile { build, merge }` | `SfxIndexFile { kind: Primary, serialize }` |
| SiblingIndex | `SfxIndexFile { build, merge }` | `SfxIndexFile { kind: Primary, serialize }` |
| PosMapIndex | `SfxIndexFile { build, merge }` + `DerivedPosMap` | `SfxIndexFile { kind: Derived, on_posting, serialize }` |
| ByteMapIndex | `SfxIndexFile { build, merge }` + `DerivedByteMap` | `SfxIndexFile { kind: Derived, on_token, serialize }` |
| TermTextsIndex | `SfxIndexFile { build, merge }` + `DerivedTermTexts` | `SfxIndexFile { kind: Derived, on_token, serialize }` |
| SepMapIndex | `SfxIndexFile { build, merge }` + `DerivedSepMap` | `SfxIndexFile { kind: DerivedWithDeps, depends_on, build_from_deps, serialize }` |

## Étapes

1. Modifier le trait `SfxIndexFile` : ajouter `kind`, events, deps, `serialize`; retirer `build`, `merge`
2. Supprimer `SfxBuildContext`, `SfxMergeContext`, `SfxDerivedIndex`, `all_derived_indexes()`
3. Fusionner chaque paire (ex: `PosMapIndex` + `DerivedPosMap` → `PosMapIndex` unique)
4. Adapter `build_derived_indexes()` pour filtrer par `kind()`
5. Adapter les Primary (SfxPostIndex, GapMapIndex, SiblingIndex) : `kind: Primary`, `serialize` vide
6. Adapter `segment_component.rs` et `segment_reader.rs` si nécessaire (probablement rien à changer)
7. Tests
