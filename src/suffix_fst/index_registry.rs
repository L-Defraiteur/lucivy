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
        // Box::new(super::termtexts::TermTextsIndex),  // TODO: next
    ]
}

/// Get an index definition by id.
pub fn get_index(id: &str) -> Option<Box<dyn SfxIndexFile>> {
    all_indexes().into_iter().find(|i| i.id() == id)
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
