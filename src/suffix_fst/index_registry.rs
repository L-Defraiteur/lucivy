//! SFX index file abstraction: unified trait + registry.
//!
//! Every per-field SFX index file implements `SfxIndexFile`.
//! Three kinds exist:
//!
//! - **Primary**: built by dedicated DAG nodes (sfxpost, gapmap, sibling).
//!   The trait is used only for GC protection + file loading.
//! - **Derived**: built by single-pass events (posmap, bytemap, termtexts).
//!   `on_token`/`on_posting` called during one loop over tokens + sfxpost.
//! - **DerivedWithDeps**: built after Derived, with access to their data (sepmap).
//!   `build_from_deps()` receives already-serialized Derived indexes.
//!
//! Adding a new index = implement the trait + add one line to `all_indexes()`.

use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────
// IndexKind
// ─────────────────────────────────────────────────────────────────────

/// Role of an index in the build/merge pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// Built by a dedicated DAG node. The trait serves GC + loading only.
    Primary,
    /// Built by single-pass events (on_token / on_posting).
    Derived,
    /// Built after Derived indexes, with access to their serialized data.
    DerivedWithDeps,
}

// ─────────────────────────────────────────────────────────────────────
// Context for DerivedWithDeps
// ─────────────────────────────────────────────────────────────────────

/// Data available to DerivedWithDeps indexes after the event pass.
pub struct SfxDeriveContext<'a> {
    /// Already-serialized Derived indexes, keyed by id.
    pub derived: &'a HashMap<String, Vec<u8>>,
    /// Primary gapmap data (from CopyGapmapNode or SfxCollector).
    pub gapmap_data: &'a [u8],
    /// Number of documents in this segment.
    pub num_docs: u32,
}

// ─────────────────────────────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────────────────────────────

/// A per-field index file in the SFX ecosystem.
///
/// Unified abstraction for GC, loading, and build/merge.
/// Primary indexes only need `id`/`extension`/`kind`.
/// Derived indexes add event callbacks.
/// DerivedWithDeps indexes add dependency resolution.
pub trait SfxIndexFile: Send {
    /// Unique identifier (e.g. "posmap", "termtexts").
    fn id(&self) -> &'static str;

    /// File extension without the dot (e.g. "posmap").
    fn extension(&self) -> &'static str;

    /// Role in the pipeline.
    fn kind(&self) -> IndexKind;

    // ── Events (Derived + DerivedWithDeps) ───────────────────────

    /// Called once per token in ordinal order.
    fn on_token(&mut self, _ord: u32, _text: &str) {}

    /// Called for each sfxpost entry.
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _position: u32,
                  _byte_from: u32, _byte_to: u32) {}

    // ── Dependencies (DerivedWithDeps only) ──────────────────────

    /// IDs of indexes this depends on (must be Derived, no circular deps).
    fn depends_on(&self) -> Vec<&'static str> { vec![] }

    /// Called after Derived indexes are serialized.
    fn build_from_deps(&mut self, _ctx: &SfxDeriveContext) {}

    // ── Output ───────────────────────────────────────────────────

    /// Serialize accumulated data. Primary indexes return empty (data managed externally).
    fn serialize(&self) -> Vec<u8> { Vec::new() }
}

// ─────────────────────────────────────────────────────────────────────
// Registry
// ─────────────────────────────────────────────────────────────────────

/// All registered SFX index files.
/// Adding a new index = add one line here.
pub fn all_indexes() -> Vec<Box<dyn SfxIndexFile>> {
    vec![
        // Primary (built by DAG nodes)
        Box::new(super::sfxpost_v2::SfxPostIndex),
        Box::new(super::gapmap::GapMapIndex),
        Box::new(super::sibling_table::SiblingIndex),
        // Derived (single-pass events)
        Box::new(super::posmap::PosMapIndex::new()),
        Box::new(super::bytemap::ByteMapIndex::new()),
        Box::new(super::termtexts::TermTextsIndex::new()),
        Box::new(super::freqmap::FreqMapIndex::new()),
        // DerivedWithDeps
        Box::new(super::sepmap::SepMapIndex::new()),
    ]
}

// ─────────────────────────────────────────────────────────────────────
// Single-pass build for Derived + DerivedWithDeps
// ─────────────────────────────────────────────────────────────────────

/// Build all Derived and DerivedWithDeps indexes in a single pass.
///
/// Used by both WriteSfxNode (merge) and SfxCollector (segment creation).
/// Primary indexes are skipped — they are managed by DAG nodes or the collector.
pub fn build_derived_indexes(
    tokens: &std::collections::BTreeSet<String>,
    sfxpost_data: Option<&[u8]>,
    gapmap_data: &[u8],
    num_docs: u32,
) -> Vec<(String, Vec<u8>)> {
    let t0 = std::time::Instant::now();
    let sfxpost_reader = sfxpost_data
        .and_then(crate::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_slice);

    let mut indexes = all_indexes();

    // Phase 1: single-pass events (Derived + DerivedWithDeps)
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

    let phase1_ms = t0.elapsed().as_millis();

    // Phase 2: serialize Derived (no dependencies)
    let t1 = std::time::Instant::now();
    let mut built: HashMap<String, Vec<u8>> = HashMap::new();
    for idx in indexes.iter() {
        if matches!(idx.kind(), IndexKind::Derived) {
            let ts = std::time::Instant::now();
            let data = idx.serialize();
            let ms = ts.elapsed().as_millis();
            if ms > 5 { eprintln!("[derive-timing] serialize {} = {}ms ({} bytes)", idx.id(), ms, data.len()); }
            if !data.is_empty() {
                built.insert(idx.id().to_string(), data);
            }
        }
    }
    let phase2_ms = t1.elapsed().as_millis();

    // Phase 3: build DerivedWithDeps (have dependencies on Derived)
    let t2 = std::time::Instant::now();
    for idx in indexes.iter_mut() {
        if matches!(idx.kind(), IndexKind::DerivedWithDeps) {
            let ctx = SfxDeriveContext {
                derived: &built,
                gapmap_data,
                num_docs,
            };
            let ts = std::time::Instant::now();
            idx.build_from_deps(&ctx);
            let dep_ms = ts.elapsed().as_millis();
            let data = idx.serialize();
            let total_ms = ts.elapsed().as_millis();
            if total_ms > 5 { eprintln!("[derive-timing] deps {} = {}ms build + {}ms serialize ({} bytes)",
                idx.id(), dep_ms, total_ms - dep_ms, data.len()); }
            if !data.is_empty() {
                built.insert(idx.id().to_string(), data);
            }
        }
    }
    let phase3_ms = t2.elapsed().as_millis();

    let total_ms = t0.elapsed().as_millis();
    if total_ms > 10 {
        eprintln!("[derive-timing] total={}ms (events={}ms serialize={}ms deps={}ms) tokens={} num_docs={}",
            total_ms, phase1_ms, phase2_ms, phase3_ms, tokens.len(), num_docs);
    }

    // Return (extension, data) for non-Primary indexes
    indexes.iter()
        .filter(|idx| !matches!(idx.kind(), IndexKind::Primary))
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
