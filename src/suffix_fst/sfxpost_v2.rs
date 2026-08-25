//! sfxpost: binary-searchable doc_ids for filtered access.
//!
//! `SFP3` (written since 25 August 2026) is `SFP2` with the per-document
//! header delta-encoded and varint-packed, and the payload delta-encoded
//! within a document. V2 spends ten fixed bytes per document — `doc_id`
//! (u32), `payload_offset` (u32), `entry_count` (u16) — and stores the
//! payload's `(token_index, byte_from, byte_to)` as absolute varints, though
//! all three only grow inside a document. Measured over 1.24 M documents and
//! 2.95 M entries of a real index (`lucivy_core/tests/test_sfxpost_density.rs`):
//! header 12.0 -> 4.0 B/doc, payload 6.80 -> 4.47 B/entry, 1.91x overall.
//!
//! `SFP3` block, per ordinal:
//! ```text
//!   [varint] num_docs
//!   [c * 12] checkpoints, c = (num_docs - 1) / 32:
//!            doc_id (u32), cumulative payload offset (u32), header offset (u32)
//!   [num_docs * varints] doc headers: d_doc, payload_len, entry_count
//!   [payloads] per document: (d_token_index, d_byte_from, byte_to - byte_from)
//! ```
//!
//! The reader accepts both; `SegmentComponent` names this file for either
//! pipeline version, so an index written before the change keeps working with
//! nothing to migrate.
//!
//! V2 format:
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

use super::file::SfxPostingEntry;
use super::varint::{read_varint, write_varint};

const MAGIC_V2: &[u8; 4] = b"SFP2";
const MAGIC_V3: &[u8; 4] = b"SFP3";
/// Documents between two checkpoints in an `SFP3` block. `find_doc` is a
/// binary search over fixed records in V2; varints break that, so a checkpoint
/// every `CHECKPOINT_EVERY` documents restores it — one search over the
/// checkpoints, then at most that many header decodes.
///
/// 8, not 32: the run length is what a lookup pays, and `entry_at` runs one
/// lookup per emitted match (a fuzzy query does about a million). Checkpoints
/// are 12 bytes each, so 8 costs 1.5 B/doc against the 8 B/doc the encoding
/// saves — on a real index, 0.4 MB where the encoding saves 16.7
/// (`test_sfxpost_density`).
const CHECKPOINT_EVERY: usize = 8;
/// doc_id, cumulative payload offset, header offset.
const CHECKPOINT_SIZE: usize = 12;

fn checkpoints_for(n: usize) -> usize {
    if n == 0 { 0 } else { (n - 1) / CHECKPOINT_EVERY }
}

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
        // Scratch buffers reused across ordinals. A `Vec` per ordinal — three
        // million on a kernel segment — is what made the browser abort on a
        // 402 MB allocation during a commit: the churn and the fragmentation,
        // not the bytes.
        let mut payload_data: Vec<u8> = Vec::new();
        let mut payload_ends: Vec<usize> = Vec::new();
        let mut headers: Vec<u8> = Vec::new();
        let mut checkpoints: Vec<u8> = Vec::new();

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

            // Payloads first, all in one buffer: their lengths are the
            // header's, and a document's three fields only grow inside it.
            payload_data.clear();
            payload_ends.clear();
            for (_, doc_entries) in &docs {
                let (mut prev_ti, mut prev_bf) = (0u32, 0u32);
                for &(ti, bf, bt) in doc_entries {
                    write_varint(&mut payload_data, ti.wrapping_sub(prev_ti) as u64);
                    write_varint(&mut payload_data, bf.wrapping_sub(prev_bf) as u64);
                    write_varint(&mut payload_data, bt.wrapping_sub(bf) as u64);
                    prev_ti = ti;
                    prev_bf = bf;
                }
                payload_ends.push(payload_data.len());
            }

            // Document headers, with a checkpoint every CHECKPOINT_EVERY so
            // `find_doc` keeps a binary search (see the module header).
            headers.clear();
            checkpoints.clear();
            let (mut prev_doc, mut cumulative) = (0u32, 0u32);
            for (i, (doc_id, doc_entries)) in docs.iter().enumerate() {
                let start = if i == 0 { 0 } else { payload_ends[i - 1] };
                let len = payload_ends[i] - start;
                if i > 0 && i % CHECKPOINT_EVERY == 0 {
                    checkpoints.extend_from_slice(&prev_doc.to_le_bytes());
                    checkpoints.extend_from_slice(&cumulative.to_le_bytes());
                    checkpoints.extend_from_slice(&(headers.len() as u32).to_le_bytes());
                }
                write_varint(&mut headers, doc_id.wrapping_sub(prev_doc) as u64);
                write_varint(&mut headers, len as u64);
                write_varint(&mut headers, doc_entries.len() as u64);
                prev_doc = *doc_id;
                cumulative += len as u32;
            }

            write_varint(&mut entry_data, docs.len() as u64);
            entry_data.extend_from_slice(&checkpoints);
            entry_data.extend_from_slice(&headers);
            entry_data.extend_from_slice(&payload_data);
        }
        offset_table.push(entry_data.len() as u32);

        // Assemble final binary
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC_V3);
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
    /// `SFP3`: varint blocks. `SFP2` files keep their fixed-width arrays.
    v3: bool,
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
        if data.len() < 8 {
            return None;
        }
        let v3 = match &data[0..4] {
            m if m == MAGIC_V3 => true,
            m if m == MAGIC_V2 => false,
            _ => return None,
        };
        let num_terms = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let offsets_size = (num_terms as usize + 1) * 4;
        if data.len() < 8 + offsets_size {
            return None;
        }
        let offsets_start = 8;
        let entry_data_start = 8 + offsets_size;
        Some(Self { data, num_terms, offsets_start, entry_data_start, v3 })
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
            let mut idx = Vec::new();
            header.for_each_doc(|i, doc_id, _, _| {
                if filter.contains(&doc_id) { idx.push(i); }
                true
            });
            idx
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
        let Some(filter) = filter else {
            // Every document: one pass over the headers, not one restart per
            // document (see `for_each_doc`).
            header.for_each_doc(|_, doc_id, offset, count| {
                header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
                    result.push(SfxPostingEntry { doc_id, token_index: ti, byte_from: bf, byte_to: bt });
                    true
                });
                true
            });
            return result;
        };
        for i in Self::filtered_indices(&header, filter) {
            let (doc_id, offset, count) = header.doc_at(i);
            header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
                result.push(SfxPostingEntry { doc_id, token_index: ti, byte_from: bf, byte_to: bt });
                true
            });
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
        let Some((_, offset, count)) = header.find_doc_full(target_doc) else { return Vec::new() };
        let mut out = Vec::with_capacity(count as usize);
        header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
            out.push((ti, bf, bt));
            true
        });
        out.into_iter()
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
        let (_, offset, count) = header.find_doc_full(doc_id)?;
        let mut found = None;
        header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
            if ti == position { found = Some((bf, bt)); return false; }
            // Entries are written in token order: past the target, stop.
            ti < position
        });
        found
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

        if self.v3 {
            let mut pos = 0usize;
            let num_docs = read_varint(data, &mut pos)? as usize;
            let cp_start = pos;
            let cp_len = checkpoints_for(num_docs) * CHECKPOINT_SIZE;
            let headers_start = cp_start + cp_len;
            if headers_start > data.len() { return None; }
            // The header region ends where the first payload begins, which the
            // last header's cumulative length gives — but the reader never needs
            // that boundary: it decodes exactly `num_docs` triples and stops.
            // Payloads are addressed from the end of the headers, so find it by
            // decoding the headers once, at open, which is `num_docs` varint
            // triples and nothing more.
            let mut pos = headers_start;
            for _ in 0..num_docs {
                read_varint(data, &mut pos)?;
                read_varint(data, &mut pos)?;
                read_varint(data, &mut pos)?;
            }
            if pos > data.len() { return None; }
            return Some(OrdinalHeader {
                num_docs,
                layout: HeaderLayout::V3 {
                    checkpoints: &data[cp_start..headers_start],
                    headers: &data[headers_start..pos],
                },
                payload_data: &data[pos..],
            });
        }

        let num_docs = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;
        let header_size = 4 + num_docs * 4 + num_docs * 4 + num_docs * 2;
        if data.len() < header_size { return None; }

        let d0 = 4;
        let p0 = d0 + num_docs * 4;
        let c0 = p0 + num_docs * 4;
        let payload_start = c0 + num_docs * 2;

        Some(OrdinalHeader {
            num_docs,
            layout: HeaderLayout::V2 {
                doc_ids: &data[d0..p0],
                payload_offsets: &data[p0..c0],
                entry_counts: &data[c0..payload_start],
            },
            payload_data: &data[payload_start..],
        })
    }

    /// Visit every entry of an ordinal as `(doc_id, token_index, byte_from,
    /// byte_to)` without allocating. The merge walks every ordinal of every
    /// source segment this way — `entries()` built one `Vec` per ordinal
    /// per segment on that path.
    pub fn for_each_entry(&self, ordinal: u32, mut f: impl FnMut(u32, u32, u32, u32)) {
        if ordinal >= self.num_terms { return; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return };
        header.for_each_doc(|_, doc_id, offset, count| {
            header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
                f(doc_id, ti, bf, bt);
                true
            });
            true
        });
    }

    fn decode_doc_payload(&self, header: &OrdinalHeader, doc_idx: usize) -> Vec<(u32, u32, u32)> {
        let (_, offset, count) = header.doc_at(doc_idx);
        let mut entries = Vec::with_capacity(count as usize);
        header.walk_payload(offset as usize, count as usize, |ti, bf, bt| {
            entries.push((ti, bf, bt));
            true
        });
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
    /// V2: three fixed-width arrays. V3: checkpoints and varint headers.
    layout: HeaderLayout<'a>,
    payload_data: &'a [u8],
}

enum HeaderLayout<'a> {
    V2 {
        doc_ids: &'a [u8],
        payload_offsets: &'a [u8],
        entry_counts: &'a [u8],
    },
    V3 {
        checkpoints: &'a [u8],
        headers: &'a [u8],
    },
}

impl<'a> OrdinalHeader<'a> {
    /// `(doc_id, payload offset, entry count)` of the `i`-th document.
    ///
    /// V2 reads three fixed slots. V3 restarts from the checkpoint before `i`
    /// and decodes at most `CHECKPOINT_EVERY` header triples — the deltas are
    /// cumulative, so there is no way to read the `i`-th without the ones
    /// before it in its run, and the run is bounded on purpose.
    fn doc_at(&self, i: usize) -> (u32, u32, u16) {
        match &self.layout {
            HeaderLayout::V2 { doc_ids, payload_offsets, entry_counts } => (
                u32::from_le_bytes(doc_ids[i * 4..i * 4 + 4].try_into().unwrap()),
                u32::from_le_bytes(payload_offsets[i * 4..i * 4 + 4].try_into().unwrap()),
                u16::from_le_bytes(entry_counts[i * 2..i * 2 + 2].try_into().unwrap()),
            ),
            HeaderLayout::V3 { checkpoints, headers } => {
                let k = i / CHECKPOINT_EVERY;
                let (mut doc, mut offset, mut pos) = if k == 0 {
                    (0u32, 0u32, 0usize)
                } else {
                    let c = (k - 1) * CHECKPOINT_SIZE;
                    let g = |j: usize| u32::from_le_bytes(
                        checkpoints[c + j * 4..c + j * 4 + 4].try_into().unwrap());
                    (g(0), g(1), g(2) as usize)
                };
                let mut count = 0u32;
                for j in (k * CHECKPOINT_EVERY)..=i {
                    let Some(d_doc) = read_varint(headers, &mut pos) else { break };
                    let Some(len) = read_varint(headers, &mut pos) else { break };
                    let Some(n) = read_varint(headers, &mut pos) else { break };
                    doc = doc.wrapping_add(d_doc as u32);
                    count = n as u32;
                    if j == i {
                        break;
                    }
                    // `offset` is where the *next* document's payload starts,
                    // so a length is added only once its document is passed.
                    offset = offset.wrapping_add(len as u32);
                }
                (doc, offset, count.min(u16::MAX as u32) as u16)
            }
        }
    }

    /// Visit every document as `(doc_id, payload offset, entry count)`,
    /// decoding the headers once.
    ///
    /// `doc_at(i)` restarts from the checkpoint before `i`, which is what makes
    /// random access bounded — and what makes a sequential walk quadratic in the
    /// run length if it is used index by index. Measured on the 21-query panel:
    /// 2 405 ms with the fixed-width headers, 2 721 ms once `entries_filtered`
    /// and `for_each_entry` reached V3 through `doc_at`. They stream instead.
    fn for_each_doc(&self, mut f: impl FnMut(usize, u32, u32, u16) -> bool) {
        match &self.layout {
            HeaderLayout::V2 { .. } => {
                for i in 0..self.num_docs {
                    let (doc, off, n) = self.doc_at(i);
                    if !f(i, doc, off, n) { return; }
                }
            }
            HeaderLayout::V3 { headers, .. } => {
                let (mut doc, mut offset, mut pos) = (0u32, 0u32, 0usize);
                for i in 0..self.num_docs {
                    let (Some(d_doc), Some(len), Some(n)) = (
                        read_varint(headers, &mut pos),
                        read_varint(headers, &mut pos),
                        read_varint(headers, &mut pos),
                    ) else { return };
                    doc = doc.wrapping_add(d_doc as u32);
                    if !f(i, doc, offset, n.min(u16::MAX as u64) as u16) { return; }
                    offset = offset.wrapping_add(len as u32);
                }
            }
        }
    }

    /// Walk one document's payload, stopping when `f` returns `false`.
    ///
    /// V2 stores `(token_index, byte_from, byte_to)` absolute; V3 stores them
    /// delta-encoded within the document, where all three only grow. Both are
    /// decoded here so the three callers — full decode, the merge walk and the
    /// single-position lookup — cannot drift apart.
    fn walk_payload(&self, offset: usize, count: usize, mut f: impl FnMut(u32, u32, u32) -> bool) {
        let data = &self.payload_data[offset.min(self.payload_data.len())..];
        let mut pos = 0usize;
        let v3 = matches!(self.layout, HeaderLayout::V3 { .. });
        // The payload is the hot loop — one pass per matching document, and
        // `entry_at` runs one per emitted match. It uses `decode_vint`, the
        // same tight decoder V2 always used; the bounds-checked `read_varint`
        // of the shared module costs 14 % on the 21-query panel here, and this
        // loop cannot read past its slice anyway: a short read decodes a
        // truncated value and the `pos >= data.len()` guard ends the walk.
        let (mut prev_ti, mut prev_bf) = (0u32, 0u32);
        for _ in 0..count {
            if pos >= data.len() { return }
            let (a, n) = decode_vint(&data[pos..]); pos += n;
            if pos >= data.len() { return }
            let (b, n) = decode_vint(&data[pos..]); pos += n;
            if pos > data.len() { return }
            let (c, n) = decode_vint(&data[pos..]); pos += n;
            let (ti, bf, bt) = if v3 {
                let ti = prev_ti.wrapping_add(a);
                let bf = prev_bf.wrapping_add(b);
                prev_ti = ti;
                prev_bf = bf;
                (ti, bf, bf.wrapping_add(c))
            } else {
                (a, b, c)
            };
            if !f(ti, bf, bt) {
                return;
            }
        }
    }

    #[inline]
    fn doc_id(&self, i: usize) -> u32 { self.doc_at(i).0 }
    #[inline]
    fn payload_offset(&self, i: usize) -> u32 { self.doc_at(i).1 }
    #[inline]
    fn entry_count(&self, i: usize) -> u16 { self.doc_at(i).2 }

    #[inline]
    fn find_doc(&self, doc_id: u32) -> Option<usize> {
        self.find_doc_full(doc_id).map(|(i, _, _)| i)
    }

    /// Binary search on doc_id, returning the document's index, payload offset
    /// and entry count. V2 probes three arrays; V3 narrows to one run of
    /// `CHECKPOINT_EVERY` documents through the checkpoints, then decodes it —
    /// once. Returning the triple is what keeps a lookup to a single scan:
    /// every caller needs the payload right after, and `doc_at(i)` would
    /// restart the same run from its checkpoint.
    fn find_doc_full(&self, doc_id: u32) -> Option<(usize, u32, u16)> {
        match &self.layout {
            HeaderLayout::V2 { .. } => {
                let (mut lo, mut hi) = (0usize, self.num_docs);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if self.doc_id(mid) < doc_id { lo = mid + 1; } else { hi = mid; }
                }
                if lo < self.num_docs {
                    let (d, off, n) = self.doc_at(lo);
                    if d == doc_id { return Some((lo, off, n)); }
                }
                None
            }
            HeaderLayout::V3 { checkpoints, headers } => {
                // Largest checkpoint whose document is below the target: the
                // answer, if any, is in the 32 documents that follow it.
                let c = checkpoints_for(self.num_docs);
                let (mut lo, mut hi) = (0usize, c);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2 + 1;
                    let at = (mid - 1) * CHECKPOINT_SIZE;
                    let cp_doc = u32::from_le_bytes(checkpoints[at..at + 4].try_into().unwrap());
                    if cp_doc < doc_id { lo = mid; } else { hi = mid - 1; }
                }
                let start = lo * CHECKPOINT_EVERY;
                let end = (start + CHECKPOINT_EVERY).min(self.num_docs);
                // Decode the run once. Calling `doc_id(i)` here would restart
                // from the checkpoint at every step — 32 x 32 decodes for a
                // lookup that needs 32, and `entry_at` runs one per emitted
                // match (measured: the 21-query panel went 2 405 -> 2 781 ms).
                let (mut doc, mut offset, mut pos) = if lo == 0 {
                    (0u32, 0u32, 0usize)
                } else {
                    let at = (lo - 1) * CHECKPOINT_SIZE;
                    let g = |j: usize| u32::from_le_bytes(
                        checkpoints[at + j * 4..at + j * 4 + 4].try_into().unwrap());
                    (g(0), g(1), g(2) as usize)
                };
                for i in start..end {
                    let (Some(d_doc), Some(len), Some(n)) = (
                        read_varint(headers, &mut pos),
                        read_varint(headers, &mut pos),
                        read_varint(headers, &mut pos),
                    ) else { return None };
                    doc = doc.wrapping_add(d_doc as u32);
                    match doc.cmp(&doc_id) {
                        std::cmp::Ordering::Less => {
                            offset = offset.wrapping_add(len as u32);
                            continue;
                        }
                        std::cmp::Ordering::Equal => return Some((i, offset, n.min(u16::MAX as u64) as u16)),
                        std::cmp::Ordering::Greater => return None,
                    }
                }
                None
            }
        }
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

    /// The V2 writer, kept in the tests only: SFP3 has to keep reading the
    /// segments written before 25 August 2026.
    fn write_v2(ordinals: &[Vec<(u32, u32, u32, u32)>]) -> Vec<u8> {
        use super::super::collector::encode_vint;
        let mut entry_data = Vec::new();
        let mut offset_table: Vec<u32> = Vec::new();
        for entries in ordinals {
            offset_table.push(entry_data.len() as u32);
            let mut sorted = entries.clone();
            sorted.sort_unstable();
            let mut docs: Vec<(u32, Vec<(u32, u32, u32)>)> = Vec::new();
            for &(doc_id, ti, bf, bt) in sorted.iter() {
                if docs.last().is_none_or(|d| d.0 != doc_id) {
                    docs.push((doc_id, Vec::new()));
                }
                docs.last_mut().unwrap().1.push((ti, bf, bt));
            }
            entry_data.extend_from_slice(&(docs.len() as u32).to_le_bytes());
            for &(doc_id, _) in &docs {
                entry_data.extend_from_slice(&doc_id.to_le_bytes());
            }
            let mut payloads: Vec<Vec<u8>> = Vec::new();
            for (_, de) in &docs {
                let mut payload = Vec::new();
                for &(ti, bf, bt) in de {
                    encode_vint(ti, &mut payload);
                    encode_vint(bf, &mut payload);
                    encode_vint(bt, &mut payload);
                }
                payloads.push(payload);
            }
            let mut cumulative = 0u32;
            for payload in &payloads {
                entry_data.extend_from_slice(&cumulative.to_le_bytes());
                cumulative += payload.len() as u32;
            }
            for (_, de) in &docs {
                entry_data.extend_from_slice(&(de.len() as u16).to_le_bytes());
            }
            for payload in &payloads {
                entry_data.extend_from_slice(payload);
            }
        }
        offset_table.push(entry_data.len() as u32);
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC_V2);
        out.extend_from_slice(&(ordinals.len() as u32).to_le_bytes());
        for &off in &offset_table {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&entry_data);
        out
    }

    /// `n` documents, several entries each, spanning more than one checkpoint
    /// run, with a token index that jumps and a byte offset that occasionally
    /// goes backwards — so the wrapping deltas are exercised.
    fn sample(n: u32) -> Vec<(u32, u32, u32, u32)> {
        let mut v = Vec::new();
        for d in 0..n {
            let doc = d * 3 + 1;
            for k in 0..(1 + d % 4) {
                let ti = k * 5 + d % 7;
                let bf = if d % 29 == 28 { 3 } else { ti * 9 + 11 };
                v.push((doc, ti, bf, bf + 4 + k % 3));
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    #[test]
    fn v3_matches_v2_exactly() {
        for n in [1u32, 2, 31, 32, 33, 64, 65, 200, 1000] {
            let entries = sample(n);
            let mut w = SfxPostWriterV2::new(2);
            for &(d, ti, bf, bt) in &entries {
                w.add_entry(1, d, ti, bf, bt);
            }
            let v3 = w.finish();
            assert_eq!(&v3[0..4], MAGIC_V3, "the writer must emit SFP3");
            let v2 = write_v2(&[Vec::new(), entries.clone()]);

            let r3 = SfxPostReaderV2::open(v3.clone()).unwrap();
            let r2 = SfxPostReaderV2::open(v2.clone()).unwrap();

            assert_eq!(r3.entries(1), r2.entries(1), "n={n}: entries()");
            assert_eq!(r3.doc_freq(1), r2.doc_freq(1), "n={n}: doc_freq()");
            assert!(r3.entries(0).is_empty(), "n={n}: empty ordinal");

            let mut w3 = Vec::new();
            let mut w2 = Vec::new();
            r3.for_each_entry(1, |d, ti, bf, bt| w3.push((d, ti, bf, bt)));
            r2.for_each_entry(1, |d, ti, bf, bt| w2.push((d, ti, bf, bt)));
            assert_eq!(w3, w2, "n={n}: for_each_entry");
            assert_eq!(w3, entries, "n={n}: for_each_entry against the source");

            // Every document, present and absent, through every lookup path.
            for d in 0..(n * 3 + 4) {
                assert_eq!(r3.has_doc(1, d), r2.has_doc(1, d), "n={n}: has_doc({d})");
                assert_eq!(r3.entries_for_doc(1, d), r2.entries_for_doc(1, d), "n={n}: entries_for_doc({d})");
                for pos in 0..8 {
                    assert_eq!(r3.entry_at(1, d, pos), r2.entry_at(1, d, pos), "n={n}: entry_at({d}, {pos})");
                }
            }

            // Filtered resolve takes the binary-search branch or the scan
            // depending on the filter's size: exercise both.
            for take in [1usize, 3, n as usize] {
                let filter: HashSet<u32> = entries.iter().map(|e| e.0).take(take).collect();
                assert_eq!(
                    r3.entries_filtered(1, Some(&filter)), r2.entries_filtered(1, Some(&filter)),
                    "n={n}: entries_filtered with {} docs", filter.len()
                );
            }

            if n > CHECKPOINT_EVERY as u32 {
                assert!(v3.len() < v2.len(), "n={n}: SFP3 {} B is not smaller than SFP2 {} B", v3.len(), v2.len());
            }
        }
    }

    #[test]
    fn v3_reader_still_reads_v2() {
        let entries = sample(100);
        let v2 = write_v2(&[entries.clone()]);
        let r = SfxPostReaderV2::open(v2).unwrap();
        let mut got = Vec::new();
        r.for_each_entry(0, |d, ti, bf, bt| got.push((d, ti, bf, bt)));
        assert_eq!(got, entries);
    }

    #[test]
    fn v3_survives_a_truncated_file() {
        let mut w = SfxPostWriterV2::new(1);
        for &(d, ti, bf, bt) in &sample(200) {
            w.add_entry(0, d, ti, bf, bt);
        }
        let data = w.finish();
        for cut in [data.len() / 3, data.len() / 2, data.len() - 1] {
            if let Some(r) = SfxPostReaderV2::open(data[..cut].to_vec()) {
                let _ = r.entries(0);
                let _ = r.doc_freq(0);
                let _ = r.has_doc(0, 7);
                let _ = r.entry_at(0, 7, 2);
                r.for_each_entry(0, |_, _, _, _| {});
            }
        }
    }

    #[test]
    fn test_v2_not_v2_format() {
        // V1 data doesn't start with "SFP2"
        let v1_data = vec![0u8; 100];
        assert!(SfxPostReaderV2::open(v1_data).is_none());
    }
}
