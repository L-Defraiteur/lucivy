# 10 — Design : abstraction index SFX

Date : 28 mars 2026

## Problème

Ajouter un nouveau fichier d'index (ex: `.termtexts`) nécessite de toucher
**~10 fichiers** :

```
suffix_fst/xxx.rs          — Writer + Reader structs
suffix_fst/collector.rs    — SfxBuildOutput field + build logic
indexer/segment_writer.rs  — write during segment creation
indexer/segment_serializer.rs — write_xxx() method
index/segment_component.rs — enum variant + extension string
index/segment_reader.rs    — load_sfx_files() + accessor
indexer/merger.rs           — merge_sfx_deferred + merge_sfx_legacy
indexer/sfx_merge.rs        — merge helpers
index/index_meta.rs         — sfx_field_ids
```

C'est fragile, sujet aux oublis (cf. posmap absent du merge, sibling links
pas appelé dans legacy, sfx_field_ids pas propagé dans segment_updater).

## Objectif

1. **Une seule source de vérité par index** — un fichier Rust unique définit
   tout : format, build, read, merge, extension, dépendances
2. **Index manager** — enregistre les index, gère write/load/merge/GC
3. **Queries requièrent des index** — erreur explicite si feature manquante

## Design

### Trait `SfxIndexFile`

Chaque fichier d'index implémente ce trait. Un seul fichier par index.

```rust
// src/suffix_fst/index_trait.rs

/// A per-field index file that participates in the SFX ecosystem.
pub trait SfxIndexFile: Send + Sync + 'static {
    /// Unique identifier (used for registration and feature checking).
    fn id(&self) -> &'static str;

    /// File extension without the dot (e.g. "posmap", "termtexts").
    fn extension(&self) -> &'static str;

    /// Build this index from the collector's data during segment creation.
    /// Returns the serialized bytes, or empty vec to skip writing.
    fn build(&self, ctx: &SfxBuildContext) -> Vec<u8>;

    /// Merge this index from multiple source segments.
    /// `sources[i]` = bytes from segment i (None if segment didn't have it).
    /// `merge_ctx` provides doc mapping, term iteration, etc.
    fn merge(&self, sources: &[Option<&[u8]>], ctx: &SfxMergeContext) -> Vec<u8>;

    /// Validate that the loaded bytes are well-formed (optional).
    fn validate(&self, _bytes: &[u8]) -> Result<(), String> { Ok(()) }
}
```

### Contextes

```rust
/// Data available during segment creation (from SfxCollector).
pub struct SfxBuildContext<'a> {
    /// Tokens sorted by final ordinal. Index = ordinal.
    pub token_texts: &'a [&'a str],
    /// Posting entries per ordinal: (doc_id, token_index, byte_from, byte_to).
    pub token_postings: &'a [Vec<(u32, u32, u32, u32)>],
    /// Number of documents in this segment.
    pub num_docs: u32,
}

/// Data available during merge.
pub struct SfxMergeContext<'a> {
    /// (new_ordinal, token_text) for each merged term, in order.
    pub merged_terms: &'a [(u32, String)],
    /// Per source segment: old_ordinal → new_ordinal mapping.
    pub ordinal_maps: &'a [HashMap<u32, u32>],
    /// Doc id mapping: new_doc → (seg_ord, old_doc).
    pub doc_mapping: &'a [DocAddress],
    /// Reverse: (seg_ord, old_doc) → new_doc.
    pub reverse_doc_map: &'a [HashMap<u32, u32>],
    /// Sfxpost readers per source segment (for entry iteration).
    pub sfxpost_readers: &'a [Option<SfxPostReaderV2<'a>>],
}
```

### Implémentations

Chaque index dans son propre fichier, auto-contenu :

```rust
// src/suffix_fst/posmap.rs — en bas du fichier existant

pub struct PosMapIndex;

impl SfxIndexFile for PosMapIndex {
    fn id(&self) -> &'static str { "posmap" }
    fn extension(&self) -> &'static str { "posmap" }

    fn build(&self, ctx: &SfxBuildContext) -> Vec<u8> {
        let mut writer = PosMapWriter::new();
        for (ord, postings) in ctx.token_postings.iter().enumerate() {
            for &(doc_id, ti, _, _) in postings {
                writer.add(doc_id, ti, ord as u32);
            }
        }
        writer.serialize()
    }

    fn merge(&self, _sources: &[Option<&[u8]>], ctx: &SfxMergeContext) -> Vec<u8> {
        let mut writer = PosMapWriter::new();
        for &(new_ord, _) in ctx.merged_terms {
            for (seg_ord, reader) in ctx.sfxpost_readers.iter().enumerate() {
                if let Some(reader) = reader {
                    // find old_ord for this term in this segment...
                    // remap doc_ids...
                }
            }
        }
        writer.serialize()
    }
}
```

```rust
// src/suffix_fst/termtexts.rs (NOUVEAU)

pub struct TermTextsIndex;

impl SfxIndexFile for TermTextsIndex {
    fn id(&self) -> &'static str { "termtexts" }
    fn extension(&self) -> &'static str { "termtexts" }

    fn build(&self, ctx: &SfxBuildContext) -> Vec<u8> {
        let mut writer = TermTextsWriter::new();
        for (ord, text) in ctx.token_texts.iter().enumerate() {
            writer.add(ord as u32, text);
        }
        writer.serialize()
    }

    fn merge(&self, _sources: &[Option<&[u8]>], ctx: &SfxMergeContext) -> Vec<u8> {
        let mut writer = TermTextsWriter::new();
        for &(new_ord, ref text) in ctx.merged_terms {
            writer.add(new_ord, text);
        }
        writer.serialize()
    }
}
```

### Index Registry

```rust
// src/suffix_fst/registry.rs

use std::sync::OnceLock;

static REGISTRY: OnceLock<Vec<Box<dyn SfxIndexFile>>> = OnceLock::new();

pub fn sfx_index_registry() -> &'static [Box<dyn SfxIndexFile>] {
    REGISTRY.get_or_init(|| vec![
        Box::new(posmap::PosMapIndex),
        Box::new(bytemap::ByteMapIndex),
        Box::new(termtexts::TermTextsIndex),
        // Ajouter un nouvel index = ajouter UNE ligne ici
    ])
}

/// Get an index by id.
pub fn get_index(id: &str) -> Option<&'static dyn SfxIndexFile> {
    sfx_index_registry().iter().find(|i| i.id() == id).map(|i| i.as_ref())
}
```

### Intégration dans le segment lifecycle

#### Segment creation (segment_writer.rs)

```rust
// AVANT : 15 lignes par index file, dupliquées
if !output.posmap.is_empty() {
    self.segment_serializer.write_posmap(field_id, &output.posmap)?;
}
if !output.bytemap.is_empty() {
    self.segment_serializer.write_bytemap(field_id, &output.bytemap)?;
}

// APRÈS : 3 lignes, automatique pour tous les index enregistrés
for index in sfx_index_registry() {
    let data = index.build(&build_ctx);
    if !data.is_empty() {
        serializer.write_custom_index(field_id, index.extension(), &data)?;
    }
}
```

#### Segment reader (segment_reader.rs)

```rust
// AVANT : HashMap par type, manuellement
let mut posmap_files = FnvHashMap::default();
let mut bytemap_files = FnvHashMap::default();
// ... pour chaque nouveau type

// APRÈS : une seule HashMap<(&str, Field), FileSlice>
let mut sfx_index_files: HashMap<(&str, Field), FileSlice> = HashMap::new();
for field_id in &sfx_field_ids {
    let field = Field::from_field_id(*field_id);
    for index in sfx_index_registry() {
        if let Ok(slice) = segment.open_read_custom(&format!("{}.{}", field_id, index.extension())) {
            sfx_index_files.insert((index.id(), field), slice);
        }
    }
}

// Accessor générique :
pub fn sfx_index_file(&self, id: &str, field: Field) -> Option<&FileSlice> {
    self.sfx_index_files.get(&(id, field))
}
```

#### Merge (merger.rs)

```rust
// AVANT : code dupliqué par index type dans merge_sfx_deferred/legacy

// APRÈS : boucle sur le registry
for index in sfx_index_registry() {
    let sources: Vec<Option<&[u8]>> = self.readers.iter()
        .map(|r| r.sfx_index_file(index.id(), field)
            .and_then(|fs| fs.read_bytes().ok())
            .map(|b| b.as_ref()))
        .collect();
    let merged = index.merge(&sources, &merge_ctx);
    if !merged.is_empty() {
        serializer.write_custom_index(field.field_id(), index.extension(), &merged)?;
    }
}
```

#### GC (segment_component.rs)

```rust
// AVANT : variante enum par type
pub enum SegmentComponent {
    SuffixFst { field_id: u32 },
    SuffixPost { field_id: u32 },
    PosMap { field_id: u32 },    // ajouté manuellement
    ByteMap { field_id: u32 },   // ajouté manuellement
}

// APRÈS : enum dynamique ou génération depuis le registry
pub fn all_components(sfx_field_ids: &[u32]) -> Vec<SegmentComponent> {
    let mut components = Self::fixed_components().to_vec();
    for &fid in sfx_field_ids {
        // .sfx et .sfxpost sont spéciaux (le cœur du SFX)
        components.push(SegmentComponent::SuffixFst { field_id: fid });
        components.push(SegmentComponent::SuffixPost { field_id: fid });
        // Tous les index enregistrés
        for index in sfx_index_registry() {
            components.push(SegmentComponent::CustomSfxIndex {
                field_id: fid,
                extension: index.extension().to_string(),
            });
        }
    }
    components
}
```

### Query feature requirements

```rust
// src/suffix_fst/features.rs

/// An index feature that a query can require.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexFeature {
    SuffixFst,
    SuffixPost,
    SiblingTable,
    Custom(&'static str),  // id from SfxIndexFile
}

impl IndexFeature {
    pub const POSMAP: Self = Self::Custom("posmap");
    pub const BYTEMAP: Self = Self::Custom("bytemap");
    pub const TERMTEXTS: Self = Self::Custom("termtexts");
}

/// Check that all required features are available for a field in a segment.
pub fn check_features(
    reader: &SegmentReader,
    field: Field,
    required: &[IndexFeature],
) -> Result<(), LucivyError> {
    for feature in required {
        let available = match feature {
            IndexFeature::SuffixFst => reader.sfx_file(field).is_some(),
            IndexFeature::SuffixPost => reader.sfxpost_file(field).is_some(),
            IndexFeature::SiblingTable => reader.sfx_file(field)
                .and_then(|f| f.read_bytes().ok())
                .and_then(|b| SfxFileReader::open(b.as_ref()).ok())
                .map_or(false, |r| r.sibling_table().is_some()),
            IndexFeature::Custom(id) => reader.sfx_index_file(id, field).is_some(),
        };
        if !available {
            return Err(LucivyError::MissingIndexFeature(format!(
                "Query requires {:?} for field {:?} in segment {} — rebuild index",
                feature, field, reader.segment_id().short_uuid_string(),
            )));
        }
    }
    Ok(())
}
```

Usage dans un scorer :

```rust
impl Weight for SuffixContainsWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> Result<Box<dyn Scorer>> {
        // Crash explicite si feature manquante
        check_features(reader, self.field, &[
            IndexFeature::SuffixFst,
            IndexFeature::SuffixPost,
            IndexFeature::TERMTEXTS,   // cross-token a besoin de term texts
        ])?;

        // ... build scorer
    }
}
```

## Ajouter un nouvel index : checklist AVANT vs APRÈS

### AVANT (10 fichiers à toucher)

1. ☐ Créer `suffix_fst/xxx.rs` (Writer + Reader)
2. ☐ Ajouter champ dans `SfxBuildOutput`
3. ☐ Ajouter build dans `SfxCollector::build()`
4. ☐ Ajouter `write_xxx()` dans `segment_serializer.rs`
5. ☐ Ajouter write dans `segment_writer.rs` (2 paths: single + parallel)
6. ☐ Ajouter variante dans `SegmentComponent` enum
7. ☐ Ajouter dans `all_components()`
8. ☐ Ajouter `xxx_files` HashMap dans `segment_reader.rs`
9. ☐ Ajouter load dans `load_sfx_files()`
10. ☐ Ajouter merge dans `merger.rs` (deferred + legacy)
11. ☐ Ajouter merge dans `sfx_merge.rs`
12. ☐ Prier pour ne rien oublier

### APRÈS (2 fichiers à toucher)

1. ☐ Créer `suffix_fst/xxx.rs` avec Writer + Reader + `impl SfxIndexFile`
2. ☐ Ajouter `Box::new(XxxIndex)` dans `sfx_index_registry()`

**C'est tout.** Le registry gère automatiquement write, load, merge, GC.

## Ordre d'implémentation

1. **Phase 1** : `SfxIndexFile` trait + `SfxBuildContext` + `SfxMergeContext`
2. **Phase 2** : Implémenter le trait pour PosMap et ByteMap (migration)
3. **Phase 3** : Registry + intégration segment_writer/reader/merger
4. **Phase 4** : `TermTextsIndex` — nouveau fichier, résout le bug ordinal
5. **Phase 5** : `IndexFeature` + `check_features` dans les scorers
6. **Phase 6** : Supprimer le code dupliqué (write_posmap, write_bytemap, etc.)
7. **Phase 7** : Retirer `ord_to_term` du term dict partout
