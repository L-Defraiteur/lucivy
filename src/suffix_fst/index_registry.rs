//! SFX index file abstraction: trait + registry.
//!
//! Each per-field index file (posmap, bytemap, termtexts, ...) implements
//! `SfxIndexFile`. The registry provides automatic write/load/merge/GC
//! for all registered index types.
//!
//! Adding a new index = implement the trait + add one line to `all_indexes()`.

use std::collections::HashMap;
use crate::DocId;

// ─────────────────────────────────────────────────────────────────────
// Contexts
// ─────────────────────────────────────────────────────────────────────

/// Data available during segment creation (from SfxCollector::build).
pub struct SfxBuildContext<'a> {
    /// Token texts in final ordinal order. Index = ordinal.
    pub token_texts: &'a [&'a str],
    /// Posting entries per ordinal: Vec of (doc_id, token_index, byte_from, byte_to).
    pub token_postings: &'a [&'a [(u32, u32, u32, u32)]],
    /// Number of documents in this segment.
    pub num_docs: u32,
    /// Pre-built gapmap data (built by collector during indexation).
    pub gapmap_data: Option<&'a [u8]>,
    /// Pre-built sibling table data (built by collector during indexation).
    pub sibling_data: Option<&'a [u8]>,
    /// Pre-built separator bytemap data (built by collector during indexation).
    pub sepmap_data: Option<&'a [u8]>,
}

/// Data available during merge.
pub struct SfxMergeContext<'a> {
    /// Merged terms in ordinal order: (new_ordinal, token_text).
    pub merged_terms: &'a [(u32, &'a str)],
    /// Per source segment: old_ordinal → new_ordinal.
    pub ordinal_maps: &'a [HashMap<u32, u32>],
    /// Per source segment: old_doc_id → new_doc_id.
    pub reverse_doc_map: &'a [HashMap<DocId, DocId>],
    /// Sfxpost readers per source segment.
    pub sfxpost_readers: &'a [Option<&'a crate::suffix_fst::sfxpost_v2::SfxPostReaderV2>],
    /// Doc address mapping for gapmap copy (new_doc_idx → old segment+doc).
    pub doc_mapping: &'a [crate::DocAddress],
    /// Source gapmap bytes per segment.
    pub source_gapmaps: &'a [Option<&'a [u8]>],
    /// Source sibling table bytes per segment.
    pub source_siblings: &'a [Option<&'a [u8]>],
}

// ─────────────────────────────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────────────────────────────

/// A per-field index file in the SFX ecosystem.
///
/// Each implementation lives in its own module (posmap.rs, bytemap.rs, etc.)
/// and defines everything about that index: format, build, merge, extension.
pub trait SfxIndexFile: Send + Sync {
    /// Unique identifier (e.g. "posmap", "termtexts").
    fn id(&self) -> &'static str;

    /// File extension without the dot (e.g. "posmap").
    fn extension(&self) -> &'static str;

    /// Build this index during segment creation.
    /// Returns serialized bytes (empty = skip writing).
    fn build(&self, ctx: &SfxBuildContext) -> Vec<u8>;

    /// Merge this index from source segments.
    /// `sources[i]` = bytes from segment i (None if absent).
    fn merge(&self, sources: &[Option<&[u8]>], ctx: &SfxMergeContext) -> Vec<u8>;
}

// ─────────────────────────────────────────────────────────────────────
// Registry
// ─────────────────────────────────────────────────────────────────────

/// All registered SFX index file types.
/// Adding a new index = add one line here.
pub fn all_indexes() -> Vec<Box<dyn SfxIndexFile>> {
    vec![
        Box::new(super::sfxpost_v2::SfxPostIndex),
        Box::new(super::gapmap::GapMapIndex),
        Box::new(super::sibling_table::SiblingIndex),
        Box::new(super::posmap::PosMapIndex),
        Box::new(super::bytemap::ByteMapIndex),
        Box::new(super::termtexts::TermTextsIndex),
        Box::new(super::sepmap::SepMapIndex),
    ]
}

/// Get an index definition by id.
pub fn get_index(id: &str) -> Option<Box<dyn SfxIndexFile>> {
    all_indexes().into_iter().find(|i| i.id() == id)
}

// ─────────────────────────────────────────────────────────────────────
// Derived index trait — single-pass event-driven + dependency-driven
// ─────────────────────────────────────────────────────────────────────

/// Context for dependency-driven build (after the single-pass events).
pub struct SfxDeriveContext<'a> {
    /// Already-serialized derived indexes, keyed by id.
    pub derived: &'a HashMap<String, Vec<u8>>,
    /// Primary gapmap data (from CopyGapmapNode).
    pub gapmap_data: &'a [u8],
    /// Number of documents in this segment.
    pub num_docs: u32,
}

/// A derived index built from primary SFX data in a single pass.
///
/// Events (`on_token`, `on_posting`) are called once per token/posting
/// during a single loop over tokens + sfxpost entries. Indexes that need
/// data from other derived indexes declare `depends_on()` and receive
/// the serialized data via `build_from_deps()` after the event pass.
///
/// Adding a new derived index = implement this trait + add to `all_derived_indexes()`.
pub trait SfxDerivedIndex: Send {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;

    /// Called once per token in ordinal order.
    fn on_token(&mut self, _ord: u32, _text: &str) {}

    /// Called for each sfxpost entry (doc_id, position, byte_from, byte_to).
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _position: u32,
                  _byte_from: u32, _byte_to: u32) {}

    /// IDs of derived indexes this index depends on.
    /// Must be a subset of indexes with empty depends_on (no circular deps).
    fn depends_on(&self) -> Vec<&'static str> { vec![] }

    /// Called after the event pass, with access to already-built data.
    /// Only called if `depends_on()` is non-empty.
    fn build_from_deps(&mut self, _ctx: &SfxDeriveContext) {}

    /// Serialize accumulated data to bytes.
    fn serialize(&self) -> Vec<u8>;
}

/// All derived indexes. Order doesn't matter — dependencies are resolved automatically.
pub fn all_derived_indexes() -> Vec<Box<dyn SfxDerivedIndex>> {
    vec![
        Box::new(super::posmap::DerivedPosMap::new()),
        Box::new(super::bytemap::DerivedByteMap::new()),
        Box::new(super::termtexts::DerivedTermTexts::new()),
        Box::new(super::sepmap::DerivedSepMap::new()),
    ]
}

/// Run the single-pass build for all derived indexes.
///
/// Used by both WriteSfxNode (merge) and SfxCollector (segment creation).
pub fn build_derived_indexes(
    tokens: &std::collections::BTreeSet<String>,
    sfxpost_data: Option<&[u8]>,
    gapmap_data: &[u8],
    num_docs: u32,
) -> Vec<(String, Vec<u8>)> {
    let sfxpost_reader = sfxpost_data
        .and_then(crate::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_slice);

    let mut indexes = all_derived_indexes();

    // Phase 1: single-pass events (tokens + sfxpost entries)
    for (ord, token) in tokens.iter().enumerate() {
        let ord = ord as u32;
        for idx in indexes.iter_mut() {
            idx.on_token(ord, token);
        }
        if let Some(ref reader) = sfxpost_reader {
            for entry in reader.entries(ord) {
                for idx in indexes.iter_mut() {
                    idx.on_posting(ord, entry.doc_id, entry.token_index,
                                   entry.byte_from, entry.byte_to);
                }
            }
        }
    }

    // Phase 2: serialize indexes without dependencies
    let mut built: HashMap<String, Vec<u8>> = HashMap::new();
    for idx in indexes.iter() {
        if idx.depends_on().is_empty() {
            let data = idx.serialize();
            if !data.is_empty() {
                built.insert(idx.id().to_string(), data);
            }
        }
    }

    // Phase 3: build indexes with dependencies
    // Snapshot the already-built data so we can mutate `built` after build_from_deps.
    for idx in indexes.iter_mut() {
        if !idx.depends_on().is_empty() {
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

    // Return as vec of (extension, data)
    indexes.iter()
        .filter_map(|idx| {
            built.remove(idx.id()).map(|data| (idx.extension().to_string(), data))
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Feature checking
// ─────────────────────────────────────────────────────────────────────

/// An index feature that a query can require.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IndexFeature {
    /// The core .sfx file (suffix FST + gapmap).
    SuffixFst,
    /// The .sfxpost posting index.
    SuffixPost,
    /// The sibling table (inside .sfx, but may be absent).
    SiblingTable,
    /// A custom per-field index file, identified by SfxIndexFile::id().
    Custom(&'static str),
}

impl IndexFeature {
    pub const POSMAP: Self = Self::Custom("posmap");
    pub const BYTEMAP: Self = Self::Custom("bytemap");
    pub const TERMTEXTS: Self = Self::Custom("termtexts");
}

impl std::fmt::Display for IndexFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SuffixFst => write!(f, "SuffixFst (.sfx)"),
            Self::SuffixPost => write!(f, "SuffixPost (.sfxpost)"),
            Self::SiblingTable => write!(f, "SiblingTable (in .sfx)"),
            Self::Custom(id) => write!(f, "{} (.{})", id, id),
        }
    }
}
