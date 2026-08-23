//! sfxpost V2 format: binary-searchable doc_ids for filtered access.
//!
//! Format:
//!   [4 bytes] magic: "SFP2"
//!   [4 bytes] num_terms: u32 LE
//!   [4 bytes × (num_terms + 1)] offset table: byte offsets into entry_data
//!   Entry data (per ordinal):
//!     [4 bytes] num_unique_docs: u32 LE
//!     [4 bytes × num_unique_docs] doc_ids: u32 LE, sorted ascending
//!     [4 bytes × num_unique_docs] payload_offsets: u32 LE (relative to payload start)
//!     [2 bytes × num_unique_docs] entry_counts: u16 LE
//!     Payload (VInt packed, per doc):
//!       [VInt token_index, VInt byte_from, VInt byte_to] × entry_count
//!
//! Access patterns:
//!   - Full resolve: iterate all docs, decode all payload → same as V1
//!   - Filtered resolve: binary search doc_ids, decode only matching payload
//!   - Single doc lookup: binary search → O(log n) + decode one doc's entries
//!   - Existence check: binary search only → O(log n), zero decode

use std::collections::HashSet;

use super::collector::encode_vint;
use super::file::SfxPostingEntry;

const MAGIC_V2: &[u8; 4] = b"SFP2";

// ─── Writer ──────────────────────────────────────────────────────────────────

/// Builds sfxpost V2 data from collected posting entries.
pub struct SfxPostWriterV2 {
    /// Per ordinal: list of (doc_id, token_index, byte_from, byte_to).
    ordinals: Vec<Vec<(u32, u32, u32, u32)>>,
}

impl SfxPostWriterV2 {
    /// Creates a new SFX posting writer for the specified number of terms.
    pub fn new(num_terms: usize) -> Self {
        Self {
            ordinals: vec![Vec::new(); num_terms],
        }
    }

    /// Add a posting entry for the given ordinal.
    pub fn add_entry(&mut self, ordinal: u32, doc_id: u32, token_index: u32, byte_from: u32, byte_to: u32) {
        if (ordinal as usize) < self.ordinals.len() {
            self.ordinals[ordinal as usize].push((doc_id, token_index, byte_from, byte_to));
        }
    }

    /// Build the V2 binary data.
    pub fn finish(mut self) -> Vec<u8> {
        let num_terms = self.ordinals.len();
        let mut entry_data = Vec::new();
        let mut offset_table: Vec<u32> = Vec::with_capacity(num_terms + 1);

        for entries in &mut self.ordinals {
            offset_table.push(entry_data.len() as u32);

            // Sort by (doc_id, token_index)
            entries.sort_unstable();

            // Group by doc_id (already sorted)
            let mut docs: Vec<(u32, Vec<(u32, u32, u32)>)> = Vec::new();
            for &(doc_id, ti, bf, bt) in entries.iter() {
                if docs.last().is_none_or(|d| d.0 != doc_id) {
                    docs.push((doc_id, Vec::new()));
                }
                docs.last_mut().unwrap().1.push((ti, bf, bt));
            }

            let num_unique_docs = docs.len() as u32;
            entry_data.extend_from_slice(&num_unique_docs.to_le_bytes());

            // Doc IDs (sorted, binary searchable)
            for &(doc_id, _) in &docs {
                entry_data.extend_from_slice(&doc_id.to_le_bytes());
            }

            // Encode payloads to compute offsets
            let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(docs.len());
            for (_, doc_entries) in &docs {
                let mut payload = Vec::new();
                for &(ti, bf, bt) in doc_entries {
                    encode_vint(ti, &mut payload);
                    encode_vint(bf, &mut payload);
                    encode_vint(bt, &mut payload);
                }
                payloads.push(payload);
            }

            // Payload offsets (relative to payload start)
            let mut cumulative = 0u32;
            for payload in &payloads {
                entry_data.extend_from_slice(&cumulative.to_le_bytes());
                cumulative += payload.len() as u32;
            }

            // Entry counts per doc
            for (_, doc_entries) in &docs {
                entry_data.extend_from_slice(&(doc_entries.len() as u16).to_le_bytes());
            }

            // Payload data
            for payload in &payloads {
                entry_data.extend_from_slice(payload);
            }
        }
        offset_table.push(entry_data.len() as u32);

        // Assemble final binary
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC_V2);
        out.extend_from_slice(&(num_terms as u32).to_le_bytes());
        for &off in &offset_table {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&entry_data);
        out
    }
}

/// Build sfxpost V2 data from pre-sorted entries per ordinal.
/// Convenience for the collector which already has entries grouped by ordinal.
pub fn build_sfxpost_v2(
    sorted_entries_per_ordinal: &[&[(u32, u32, u32, u32)]],
) -> Vec<u8> {
    let mut writer = SfxPostWriterV2::new(sorted_entries_per_ordinal.len());
    for (ord, entries) in sorted_entries_per_ordinal.iter().enumerate() {
        for &(doc_id, ti, bf, bt) in *entries {
            writer.add_entry(ord as u32, doc_id, ti, bf, bt);
        }
    }
    writer.finish()
}

// ─── Reader ──────────────────────────────────────────────────────────────────

/// Reads sfxpost V2 format with binary-searchable doc_ids.
///
/// Owns its data (Vec<u8>) — Send + Sync, no lifetimes.
/// Can be constructed from OwnedBytes (mmap) or Vec<u8> (in-memory).
pub struct SfxPostReaderV2 {
    data: common::OwnedBytes,
    num_terms: u32,
    offsets_start: usize,
    entry_data_start: usize,
}

impl SfxPostReaderV2 {
    /// Open a sfxpost V2 file from owned bytes.
    /// Returns None if the data is not V2 format (no "SFP2" magic).
    pub fn open(data: Vec<u8>) -> Option<Self> {
        Self::open_owned(common::OwnedBytes::new(data))
    }

    /// Open without copying, from bytes a `FileSlice` already holds.
    ///
    /// The reader was made owning to keep it `Send + Sync` and free of lifetime
    /// propagation, and reached that through `Vec<u8>` — with `.to_vec()` named
    /// in the commit as the bridge from `OwnedBytes`. `OwnedBytes` has those same
    /// properties on its own (it is `'static`, `Send + Sync`, and owns its
    /// backing through an `Arc`), so the copy bought nothing. It was paid once
    /// per segment per query: 72 copies of the postings file per query on a
    /// 50k-document bench index.
    pub fn open_owned(data: common::OwnedBytes) -> Option<Self> {
        if data.len() < 8 || &data[0..4] != MAGIC_V2 {
            return None;
        }
        let num_terms = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let offsets_size = (num_terms as usize + 1) * 4;
        if data.len() < 8 + offsets_size {
            return None;
        }
        let offsets_start = 8;
        let entry_data_start = 8 + offsets_size;
        Some(Self { data, num_terms, offsets_start, entry_data_start })
    }

    /// Open from a byte slice (copies into owned Vec).
    pub fn open_slice(data: &[u8]) -> Option<Self> {
        Self::open(data.to_vec())
    }

    /// Indices of `doc_ids` matching the filter, in ascending index order.
    ///
    /// The doc-comment on `entries_filtered` promised a binary search per
    /// filtered document; the code scanned every document in the ordinal and did
    /// a hash lookup for each. For a frequent ordinal spanning 50 000 documents
    /// with 5 active ones that is 50 000 lookups where 5 searches suffice.
    ///
    /// Whichever side is smaller drives the loop. Indices come back sorted, so
    /// the entries land in the same order the linear scan produced — callers
    /// that stop at the first match still stop at the same one.
    fn filtered_indices(header: &OrdinalHeader<'_>, filter: &HashSet<u32>) -> Vec<usize> {
        let n = header.num_docs;
        // Rough break-even: a binary search costs ~log2(n) probes.
        let probe_cost = (usize::BITS - n.leading_zeros()) as usize;
        if filter.len().saturating_mul(probe_cost) < n {
            let mut idx: Vec<usize> = filter.iter()
                .filter_map(|&d| header.find_doc(d))
                .collect();
            idx.sort_unstable();
            idx
        } else {
            (0..n).filter(|&i| filter.contains(&header.doc_id(i))).collect()
        }
    }

    fn offsets(&self) -> &[u8] {
        &self.data[self.offsets_start..self.entry_data_start]
    }

    fn entry_data(&self) -> &[u8] {
        &self.data[self.entry_data_start..]
    }

    /// Number of terms.
    pub fn num_terms(&self) -> u32 {
        self.num_terms
    }

    /// Get all posting entries for a given ordinal.
    pub fn entries(&self, ordinal: u32) -> Vec<SfxPostingEntry> {
        self.entries_filtered(ordinal, None)
    }

    /// Get posting entries for a given ordinal, optionally filtered by doc_ids.
    /// When filter is Some, only entries whose doc_id is in the set are returned.
    /// Uses binary search on the doc_id array — O(log n) per filtered doc.
    pub fn entries_filtered(
        &self,
        ordinal: u32,
        filter: Option<&HashSet<u32>>,
    ) -> Vec<SfxPostingEntry> {
        if ordinal >= self.num_terms {
            return Vec::new();
        }
        let Some(header) = self.read_ordinal_header(ordinal) else {
            return Vec::new();
        };

        let mut result = Vec::new();
        let indices: Vec<usize> = match filter {
            Some(f) => Self::filtered_indices(&header, f),
            None => (0..header.num_docs).collect(),
        };
        for i in indices {
            let doc_id = header.doc_id(i);
            let entries = self.decode_doc_payload(&header, i);
            for (ti, bf, bt) in entries {
                result.push(SfxPostingEntry {
                    doc_id,
                    token_index: ti,
                    byte_from: bf,
                    byte_to: bt,
                });
            }
        }
        result
    }

    /// Check if a specific doc_id has entries for the given ordinal.
    /// O(log n) binary search, zero payload decode.
    pub fn has_doc(&self, ordinal: u32, doc_id: u32) -> bool {
        if ordinal >= self.num_terms { return false; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return false };
        header.find_doc(doc_id).is_some()
    }

    /// Get entries for a single doc_id. O(log n) search + decode only that doc's payload.
    pub fn entries_for_doc(&self, ordinal: u32, target_doc: u32) -> Vec<SfxPostingEntry> {
        if ordinal >= self.num_terms { return Vec::new(); }
        let Some(header) = self.read_ordinal_header(ordinal) else { return Vec::new() };
        let Some(idx) = header.find_doc(target_doc) else { return Vec::new() };
        self.decode_doc_payload(&header, idx)
            .into_iter()
            .map(|(ti, bf, bt)| SfxPostingEntry {
                doc_id: target_doc,
                token_index: ti,
                byte_from: bf,
                byte_to: bt,
            })
            .collect()
    }

    /// The entry of (`ordinal`, `doc`) at token `position`, without
    /// materialising the document's payload. Entries are written in token
    /// order, so the scan stops at the first position past the target.
    /// Rebuilding a fuzzy window asked `entries_for_doc` once per position
    /// and kept one entry of ~50: 675 M decoded for 14 M used on `inclde`.
    pub fn entry_at(&self, ordinal: u32, doc_id: u32, position: u32) -> Option<(u32, u32)> {
        if ordinal >= self.num_terms { return None; }
        let header = self.read_ordinal_header(ordinal)?;
        let idx = header.find_doc(doc_id)?;
        let offset = header.payload_offset(idx) as usize;
        let count = header.entry_count(idx) as usize;
        let data = &header.payload_data[offset..];
        let mut pos = 0;
        for _ in 0..count {
            let (ti, n) = decode_vint(&data[pos..]); pos += n;
            let (bf, n) = decode_vint(&data[pos..]); pos += n;
            let (bt, n) = decode_vint(&data[pos..]); pos += n;
            if ti == position { return Some((bf, bt)); }
            if ti > position { return None; }
        }
        None
    }

    /// doc_freq: number of unique docs for an ordinal. O(1) — just read the header.
    pub fn doc_freq(&self, ordinal: u32) -> u32 {
        if ordinal >= self.num_terms { return 0; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return 0 };
        header.num_docs as u32
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn read_offset(&self, idx: u32) -> u32 {
        let offsets = self.offsets();
        let pos = idx as usize * 4;
        u32::from_le_bytes(offsets[pos..pos + 4].try_into().unwrap())
    }

    fn read_ordinal_header(&self, ordinal: u32) -> Option<OrdinalHeader<'_>> {
        let off_start = self.read_offset(ordinal) as usize;
        let off_end = self.read_offset(ordinal + 1) as usize;
        let entry_data = self.entry_data();
        if off_start >= off_end || off_start >= entry_data.len() {
            return None;
        }
        let data = &entry_data[off_start..off_end.min(entry_data.len())];
        if data.len() < 4 { return None; }

        let num_docs = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
        let header_size = 4 + num_docs * 4 + num_docs * 4 + num_docs * 2;
        if data.len() < header_size { return None; }

        let d0 = 4;
        let p0 = d0 + num_docs * 4;
        let c0 = p0 + num_docs * 4;
        let payload_start = c0 + num_docs * 2;

        Some(OrdinalHeader {
            num_docs,
            doc_ids: &data[d0..p0],
            payload_offsets: &data[p0..c0],
            entry_counts: &data[c0..payload_start],
            payload_data: &data[payload_start..],
        })
    }

    fn decode_doc_payload(&self, header: &OrdinalHeader, doc_idx: usize) -> Vec<(u32, u32, u32)> {
        let offset = header.payload_offset(doc_idx) as usize;
        let count = header.entry_count(doc_idx) as usize;
        let data = &header.payload_data[offset..];

        let mut pos = 0;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let (ti, n) = decode_vint(&data[pos..]); pos += n;
            let (bf, n) = decode_vint(&data[pos..]); pos += n;
            let (bt, n) = decode_vint(&data[pos..]); pos += n;
            entries.push((ti, bf, bt));
        }
        entries
    }
}

/// View over one ordinal's block. Borrows; decodes fields on access.
///
/// It used to materialise `doc_ids`, `payload_offsets` and `entry_counts` as
/// three `Vec`s on every call — including from `has_doc`, `doc_freq` and
/// `entries_for_doc`, which need one lookup. Over a merged segment an ordinal
/// spans thousands of documents, and `include` fetches one posting per emitted
/// match, a million times: ~40 GB of allocation traffic to read 1 M u32s.
struct OrdinalHeader<'a> {
    num_docs: usize,
    doc_ids: &'a [u8],
    payload_offsets: &'a [u8],
    entry_counts: &'a [u8],
    payload_data: &'a [u8],
}

impl<'a> OrdinalHeader<'a> {
    #[inline]
    fn doc_id(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.doc_ids[i * 4..i * 4 + 4].try_into().unwrap())
    }
    #[inline]
    fn payload_offset(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.payload_offsets[i * 4..i * 4 + 4].try_into().unwrap())
    }
    #[inline]
    fn entry_count(&self, i: usize) -> u16 {
        u16::from_le_bytes(self.entry_counts[i * 2..i * 2 + 2].try_into().unwrap())
    }
    /// Binary search over the little-endian doc_id array.
    fn find_doc(&self, doc_id: u32) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.num_docs);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.doc_id(mid) < doc_id { lo = mid + 1; } else { hi = mid; }
        }
        if lo < self.num_docs && self.doc_id(lo) == doc_id { Some(lo) } else { None }
    }
}

fn decode_vint(data: &[u8]) -> (u32, usize) {
    let mut result = 0u32;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        shift += 7;
    }
    (result, data.len())
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

/// Index file wrapper for SFX posting lists (SfxIndexFile trait).
pub struct SfxPostIndex;

impl super::index_registry::SfxIndexFile for SfxPostIndex {
    fn id(&self) -> &'static str { "sfxpost" }
    fn extension(&self) -> &'static str { "sfxpost" }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy { super::index_registry::MergeStrategy::ExternalDagNode }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v2_roundtrip_single_ordinal() {
        let mut writer = SfxPostWriterV2::new(1);
        writer.add_entry(0, 10, 0, 0, 5);
        writer.add_entry(0, 10, 1, 6, 12);
        writer.add_entry(0, 20, 0, 0, 8);
        let data = writer.finish();

        let reader = SfxPostReaderV2::open(data.clone()).unwrap();
        assert_eq!(reader.num_terms(), 1);

        let entries = reader.entries(0);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].doc_id, 10);
        assert_eq!(entries[0].token_index, 0);
        assert_eq!(entries[1].doc_id, 10);
        assert_eq!(entries[1].token_index, 1);
        assert_eq!(entries[2].doc_id, 20);
    }

    #[test]
    fn test_v2_filtered_resolve() {
        let mut writer = SfxPostWriterV2::new(1);
        for doc in 0..100u32 {
            writer.add_entry(0, doc, 0, 0, 5);
        }
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        // Filter to only 3 docs
        let filter: HashSet<u32> = [10, 50, 99].into();
        let entries = reader.entries_filtered(0, Some(&filter));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].doc_id, 10);
        assert_eq!(entries[1].doc_id, 50);
        assert_eq!(entries[2].doc_id, 99);
    }

    #[test]
    fn test_v2_single_doc_lookup() {
        let mut writer = SfxPostWriterV2::new(1);
        writer.add_entry(0, 10, 0, 0, 5);
        writer.add_entry(0, 10, 2, 10, 15);
        writer.add_entry(0, 20, 1, 5, 10);
        writer.add_entry(0, 30, 0, 0, 3);
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        let entries = reader.entries_for_doc(0, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].token_index, 0);
        assert_eq!(entries[1].token_index, 2);

        let entries = reader.entries_for_doc(0, 20);
        assert_eq!(entries.len(), 1);

        let entries = reader.entries_for_doc(0, 99);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_v2_has_doc() {
        let mut writer = SfxPostWriterV2::new(1);
        writer.add_entry(0, 10, 0, 0, 5);
        writer.add_entry(0, 20, 0, 0, 5);
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        assert!(reader.has_doc(0, 10));
        assert!(reader.has_doc(0, 20));
        assert!(!reader.has_doc(0, 15));
        assert!(!reader.has_doc(0, 99));
    }

    #[test]
    fn test_v2_doc_freq() {
        let mut writer = SfxPostWriterV2::new(2);
        writer.add_entry(0, 10, 0, 0, 5);
        writer.add_entry(0, 10, 1, 5, 10);
        writer.add_entry(0, 20, 0, 0, 5);
        writer.add_entry(1, 30, 0, 0, 5);
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        assert_eq!(reader.doc_freq(0), 2); // docs 10, 20
        assert_eq!(reader.doc_freq(1), 1); // doc 30
    }

    #[test]
    fn test_v2_multiple_ordinals() {
        let mut writer = SfxPostWriterV2::new(3);
        writer.add_entry(0, 1, 0, 0, 5);
        writer.add_entry(1, 2, 0, 0, 5);
        writer.add_entry(2, 3, 0, 0, 5);
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        assert_eq!(reader.entries(0).len(), 1);
        assert_eq!(reader.entries(0)[0].doc_id, 1);
        assert_eq!(reader.entries(1)[0].doc_id, 2);
        assert_eq!(reader.entries(2)[0].doc_id, 3);
    }

    #[test]
    fn test_v2_empty_ordinal() {
        let writer = SfxPostWriterV2::new(2);
        // Don't add any entries to ordinal 0
        let data = writer.finish();
        let reader = SfxPostReaderV2::open(data.clone()).unwrap();

        assert!(reader.entries(0).is_empty());
        assert!(reader.entries(1).is_empty());
    }

    #[test]
    fn test_v2_not_v2_format() {
        // V1 data doesn't start with "SFP2"
        let v1_data = vec![0u8; 100];
        assert!(SfxPostReaderV2::open(v1_data).is_none());
    }
}
