//! SFX index file abstraction: unified trait + registry.
//!
//! Every per-field SFX index file implements `SfxIndexFile`.
//! Two build strategies:
//!
//! - **EventDriven**: built by single-pass events (`on_token`/`on_posting`)
//!   during one loop over tokens + sfxpost. (posmap, bytemap, termtexts, freqmap)
//! - **OrMergeWithRemap**: OR-merge source data with ordinal remapping at merge,
//!   pre-built by the SfxCollector at segment creation. (sibling, sepmap)
//! - **ExternalDagNode**: managed by dedicated DAG nodes, too complex to generalize.
//!   (sfxpost, gapmap)
//!
//! Adding a new index = implement the trait + add one line to `all_indexes()`.

use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────
// MergeStrategy
// ─────────────────────────────────────────────────────────────────────

/// How an index is built during merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Built from sfxpost + tokens via on_token/on_posting events.
    EventDriven,
    /// OR-merge source data with ordinal remapping via token text.
    OrMergeWithRemap,
    /// Managed by a dedicated DAG node (too complex for generic merge).
    ExternalDagNode,
}

// ─────────────────────────────────────────────────────────────────────
// Trait
// ─────────────────────────────────────────────────────────────────────

/// A per-field index file in the SFX ecosystem.
pub trait SfxIndexFile: Send {
    /// Unique identifier (e.g. "posmap", "termtexts").
    fn id(&self) -> &'static str;

    /// File extension without the dot (e.g. "posmap").
    fn extension(&self) -> &'static str;

    /// How this index is merged.
    fn merge_strategy(&self) -> MergeStrategy;

    /// If true, the SfxCollector pre-builds this index during indexation
    /// and passes it as serialized data. If false, built by events or DAG.
    fn prebuilt_by_collector(&self) -> bool { false }

    // ── Events (EventDriven) ─────────────────────────────────────

    /// Called once per token in ordinal order.
    fn on_token(&mut self, _ord: u32, _text: &str) {}

    /// Called for each sfxpost entry.
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _position: u32,
                  _byte_from: u32, _byte_to: u32) {}

    // ── OR-merge (OrMergeWithRemap) ──────────────────────────────

    /// Merge data from source segments with ordinal remapping.
    /// Called by OrMergeNode during merge DAG execution.
    ///
    /// For each source segment, `sources[i]` contains this index's bytes
    /// (None if absent). `source_termtexts[i]` provides ordinal→text mapping
    /// for the source segment. `token_to_new_ord` maps token text to the
    /// new ordinal in the merged segment.
    fn merge_from_sources(
        &mut self,
        _sources: &[Option<&[u8]>],
        _source_termtexts: &[Option<&[u8]>],
        _token_to_new_ord: &dyn Fn(&str) -> Option<u32>,
    ) {}

    // ── Output ───────────────────────────────────────────────────

    /// Serialize accumulated data.
    fn serialize(&self) -> Vec<u8> { Vec::new() }
}

// ─────────────────────────────────────────────────────────────────────
// Registry
// ─────────────────────────────────────────────────────────────────────

/// All registered SFX index files.
/// Adding a new index = add one line here.
pub fn all_indexes() -> Vec<Box<dyn SfxIndexFile>> {
    vec![
        // ExternalDagNode (dedicated DAG nodes)
        Box::new(super::sfxpost_v2::SfxPostIndex),
        Box::new(super::gapmap::GapMapIndex),
        // OrMergeWithRemap (prebuilt by collector, OR-merged at merge)
        Box::new(super::sibling_table::SiblingIndex::new()),
        Box::new(super::sepmap::SepMapIndex::new()),
        // EventDriven (single-pass events)
        Box::new(super::posmap::PosMapIndex::new()),
        Box::new(super::bytemap::ByteMapIndex::new()),
        Box::new(super::termtexts::TermTextsIndex::new()),
        Box::new(super::freqmap::FreqMapIndex::new()),
    ]
}

// ─────────────────────────────────────────────────────────────────────
// Single-pass build for EventDriven indexes
// ─────────────────────────────────────────────────────────────────────

/// Build all EventDriven indexes in a single pass over tokens + sfxpost.
///
/// Used by both AssembleSfxNode (segment creation) and WriteSfxNode (merge).
/// OrMergeWithRemap and ExternalDagNode indexes are skipped.
pub fn build_derived_indexes(
    tokens: &std::collections::BTreeSet<String>,
    sfxpost_data: Option<&[u8]>,
) -> Vec<(String, Vec<u8>)> {
    let sfxpost_reader = sfxpost_data
        .and_then(crate::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_slice);

    let mut indexes = all_indexes();

    // Single-pass events
    for (ord, token) in tokens.iter().enumerate() {
        let ord = ord as u32;
        for idx in indexes.iter_mut() {
            if matches!(idx.merge_strategy(), MergeStrategy::EventDriven) {
                idx.on_token(ord, token);
            }
        }
        if let Some(ref reader) = sfxpost_reader {
            for entry in reader.entries(ord) {
                for idx in indexes.iter_mut() {
                    if matches!(idx.merge_strategy(), MergeStrategy::EventDriven) {
                        idx.on_posting(ord, entry.doc_id, entry.token_index,
                                       entry.byte_from, entry.byte_to);
                    }
                }
            }
        }
    }

    // Serialize
    indexes.iter()
        .filter(|idx| matches!(idx.merge_strategy(), MergeStrategy::EventDriven))
        .filter_map(|idx| {
            let data = idx.serialize();
            if data.is_empty() { None }
            else { Some((idx.extension().to_string(), data)) }
        })
        .collect()
}

/// Run the OR-merge for all OrMergeWithRemap indexes.
///
/// Used by OrMergeNode in the merge DAG.
pub fn or_merge_indexes(
    readers: &[crate::SegmentReader],
    field: crate::schema::Field,
    tokens: &std::collections::BTreeSet<String>,
) -> Vec<(String, Vec<u8>)> {
    // Build token → new ordinal map
    let token_to_ord: HashMap<&str, u32> = tokens.iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i as u32))
        .collect();

    // Load source termtexts for ordinal remapping
    let source_termtexts: Vec<Option<Vec<u8>>> = readers.iter().map(|r| {
        r.sfx_index_file("termtexts", field)
            .and_then(|f| f.read_bytes().ok())
            .map(|b| b.to_vec())
    }).collect();
    let tt_refs: Vec<Option<&[u8]>> = source_termtexts.iter()
        .map(|opt| opt.as_deref())
        .collect();

    let mut indexes = all_indexes();
    let mut results = Vec::new();

    for idx in indexes.iter_mut() {
        if !matches!(idx.merge_strategy(), MergeStrategy::OrMergeWithRemap) {
            continue;
        }

        // Load this index's data from each source segment
        let source_data: Vec<Option<Vec<u8>>> = readers.iter().map(|r| {
            r.sfx_index_file(idx.id(), field)
                .and_then(|f| f.read_bytes().ok())
                .map(|b| b.to_vec())
        }).collect();
        let src_refs: Vec<Option<&[u8]>> = source_data.iter()
            .map(|opt| opt.as_deref())
            .collect();

        idx.merge_from_sources(&src_refs, &tt_refs, &|text| {
            token_to_ord.get(text).copied()
        });

        let data = idx.serialize();
        if !data.is_empty() {
            results.push((idx.extension().to_string(), data));
        }
    }

    results
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
