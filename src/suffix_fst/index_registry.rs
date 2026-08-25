//! SFX index file abstraction: unified trait + registry.
//!
//! Every per-field SFX index file implements `SfxIndexFile`.
//! Two build strategies:
//!
//! - **EventDriven**: built by single-pass events (`on_token`/`on_posting`)
//!   during one loop over tokens + sfxpost. (posmap, bytemap, termtexts)
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

    /// Whether a segment written by the `sfx_version` pipeline carries this
    /// file. Most do in both; three belong to v2 only (`gapmap`, `sepmap`,
    /// `sibling`) and three to v3 only (`word_sfxpost`, `word_pos_map`,
    /// `sibling_v3`).
    ///
    /// This is what keeps `SegmentMeta::list_files` honest. Naming a file that
    /// the pipeline never wrote costs an open that always fails — nine per
    /// segment on a v3 index, on every garbage collection pass, every checksum
    /// walk and every measurement of an index's size — and it removes the only
    /// signal that would tell a partial read (a filesystem not ready yet) from
    /// a component that was never there.
    fn written_for(&self, _sfx_version: u8) -> bool { true }

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
        // Word position map (prebuilt by collector / merge)
        Box::new(super::word_pos_map::WordPosMapIndex::new()),
        // Word-level sfxpost (prebuilt by DAG, loaded by segment reader)
        Box::new(super::word_sfxpost::WordSfxPostIndex),
        // Sibling table v3 (prebuilt by DAG, chunk + word siblings)
        Box::new(SiblingV3Index),
    ]
}

/// Index file entry for the v3 sibling table.
struct SiblingV3Index;
impl SfxIndexFile for SiblingV3Index {
    fn id(&self) -> &'static str { "sibling_v3" }
    fn extension(&self) -> &'static str { "sibling_v3" }
    /// v3 only: v2 segments carry `.sibling`.
    fn written_for(&self, sfx_version: u8) -> bool { sfx_version >= 3 }
    fn merge_strategy(&self) -> MergeStrategy { MergeStrategy::ExternalDagNode }
    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc: u32, _ti: u32, _bf: u32, _bt: u32) {}
    fn serialize(&self) -> Vec<u8> { Vec::new() }
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

/// V3 variant: build derived indexes with own_len metadata.
///
/// For ByteMap, passes only `token[..own_len]` (without overlap bytes)
/// so the bitmap doesn't include bytes from the next token.
///
/// `token_own_lens[i]` = own_len for the i-th token in sorted order.
/// If None, falls back to full token text (v2 compat).
pub fn build_derived_indexes_v3(
    tokens: &[String],
    sfxpost_data: Option<&[u8]>,
    token_own_lens: Option<&[u16]>,
) -> Vec<(String, Vec<u8>)> {
    let sfxpost_reader = sfxpost_data
        .and_then(crate::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_slice);

    // Exclude v2 termtexts and sibling/sepmap — v3 writes its own termtexts (TTX3 format).
    let mut indexes: Vec<Box<dyn SfxIndexFile>> = all_indexes()
        .into_iter()
        .filter(|idx| idx.id() != "termtexts" && idx.id() != "sibling" && idx.id() != "sepmap")
        .collect();

    for (ord, token) in tokens.iter().enumerate() {
        let ord_u32 = ord as u32;
        // For ByteMap: truncate token to own_len (exclude overlap bytes)
        let effective_text = if let Some(lens) = token_own_lens {
            let own_len = lens.get(ord).copied().unwrap_or(token.len() as u16) as usize;
            let end = own_len.min(token.len());
            // Snap to char boundary
            let mut e = end;
            while e < token.len() && !token.is_char_boundary(e) {
                e += 1;
            }
            &token[..e.min(token.len())]
        } else {
            token.as_str()
        };

        for idx in indexes.iter_mut() {
            if matches!(idx.merge_strategy(), MergeStrategy::EventDriven) {
                idx.on_token(ord_u32, effective_text);
            }
        }
        if let Some(ref reader) = sfxpost_reader {
            for entry in reader.entries(ord_u32) {
                for idx in indexes.iter_mut() {
                    if matches!(idx.merge_strategy(), MergeStrategy::EventDriven) {
                        idx.on_posting(ord_u32, entry.doc_id, entry.token_index,
                                       entry.byte_from, entry.byte_to);
                    }
                }
            }
        }
    }

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
