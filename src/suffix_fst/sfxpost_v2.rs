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
//!   [varint] headers_len — bytes of the doc headers, so the payloads are
//!            addressable without decoding every header first
//!   [c * 12] checkpoints, c = (num_docs - 1) / CHECKPOINT_EVERY:
//!            doc_id (u32), cumulative payload offset (u32), header offset (u32)
//!   [headers_len] doc headers, varints: d_doc, payload_len, entry_count
//!   [payloads] per document: (d_token_index, d_byte_from, byte_to - byte_from)
//! ```
//!
//! `SFP5` (written since 5 September 2026 by the v3 pipeline) drops the
//! byte spans: an entry is `d_token_index` alone. The offset of a position
//! derives from the `.posmap`'s byte checkpoints and the tokens' `own_len`
//! (`PosMapReader::byte_at`), the span of a chunk is its `own_len`. The
//! spans were 37 % of the postings and 15 % of a kernel index. A reader
//! over an `SFP5` file answers `has_byte_spans() == false`, and the
//! span-carrying accessors (`entries*`, `entry_at`) hand back zero spans:
//! the v3 briques only ask for positions (`positions*`, `has_position`).
//! The v2 pipeline (`sfx_version` 2) keeps writing `SFP4` with its spans.
//!
//! `headers_len` was missing from the first `SFP3` (25 August, afternoon):
//! the reader found the payloads by decoding all `num_docs` headers on every
//! lookup, which made `has_doc` / `entry_at` linear in the ordinal's document
//! count — measured 245 ms for 2 000 lookups on 1 000 documents, 249 s on a
//! million. No such file was ever published.
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


use super::block_offsets::{self, BlockOffsets};
use super::file::SfxPostingEntry;
use super::varint::{read_varint_u32, write_varint};

const MAGIC_V2: &[u8; 4] = b"SFP2";
const MAGIC_V3: &[u8; 4] = b"SFP3";
/// `SFP3` blocks behind a block-coded offset table (`block_offsets`);
/// written since 4 September 2026 at night.
const MAGIC_V4: &[u8; 4] = b"SFP4";
/// `SFP4` without the byte spans: one varint per entry. Written since
/// 5 September 2026 by the v3 pipeline (see the module header).
const MAGIC_V5: &[u8; 4] = b"SFP5";

/// One occurrence, by position only — what `SFP5` stores and what the v3
/// briques ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositionEntry {
    /// Document containing this occurrence.
    pub doc_id: u32,
    /// Token position within the document.
    pub token_index: u32,
}
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
    /// Whether the entries carry byte spans (`SFP4`) or positions only (`SFP5`).
    spans: bool,
}

impl SfxPostWriterV2 {
    /// A writer of `SFP4`: entries with their byte spans (the v2 pipeline).
    pub fn new(num_terms: usize) -> Self {
        Self {
            ordinals: vec![Vec::new(); num_terms],
            spans: true,
        }
    }

    /// A writer of `SFP5`: positions only, the spans derive from `.posmap`
    /// (the v3 pipeline). `add_entry` ignores the spans it is given.
    pub fn positions_only(num_terms: usize) -> Self {
        Self {
            ordinals: vec![Vec::new(); num_terms],
            spans: false,
        }
    }

    /// Add a posting entry for the given ordinal.
    pub fn add_entry(&mut self, ordinal: u32, doc_id: u32, token_index: u32, byte_from: u32, byte_to: u32) {
        if (ordinal as usize) < self.ordinals.len() {
            let (bf, bt) = if self.spans { (byte_from, byte_to) } else { (0, 0) };
            self.ordinals[ordinal as usize].push((doc_id, token_index, bf, bt));
        }
    }

    /// Add an occurrence by position (a `positions_only` writer).
    pub fn add_position(&mut self, ordinal: u32, doc_id: u32, token_index: u32) {
        self.add_entry(ordinal, doc_id, token_index, 0, 0);
    }

    /// Build the V2 binary data.
    pub fn finish(mut self) -> Vec<u8> {
        let num_terms = self.ordinals.len();
        let mut entry_data = Vec::new();
        let mut offset_table: Vec<u32> = Vec::with_capacity(num_terms + 1);
        // Scratch buffers reused across ordinals, and nothing allocated per
        // document: a `Vec` per ordinal — three million on a kernel segment —
        // is what made the browser abort on a 402 MB allocation during a
        // commit, and a `Vec` per document per ordinal was the same churn one
        // level down.
        let mut payload_data: Vec<u8> = Vec::new();
        // `(doc_id, entry count, end of its payload in `payload_data`)`.
        let mut doc_runs: Vec<(u32, u32, usize)> = Vec::new();
        let mut headers: Vec<u8> = Vec::new();
        let mut checkpoints: Vec<u8> = Vec::new();

        for entries in &mut self.ordinals {
            offset_table.push(entry_data.len() as u32);

            // Sort by (doc_id, token_index): a document is then one run.
            entries.sort_unstable();

            // Payloads first, all in one buffer: their lengths are the
            // header's, and a document's three fields only grow inside it.
            payload_data.clear();
            doc_runs.clear();
            let mut i = 0;
            while i < entries.len() {
                let doc_id = entries[i].0;
                let (mut prev_ti, mut prev_bf) = (0u32, 0u32);
                let mut count = 0u32;
                while i < entries.len() && entries[i].0 == doc_id {
                    let (_, ti, bf, bt) = entries[i];
                    write_varint(&mut payload_data, ti.wrapping_sub(prev_ti) as u64);
                    if self.spans {
                        write_varint(&mut payload_data, bf.wrapping_sub(prev_bf) as u64);
                        write_varint(&mut payload_data, bt.wrapping_sub(bf) as u64);
                    }
                    prev_ti = ti;
                    prev_bf = bf;
                    count += 1;
                    i += 1;
                }
                doc_runs.push((doc_id, count, payload_data.len()));
            }

            // Document headers, with a checkpoint every CHECKPOINT_EVERY so
            // `find_doc` keeps a binary search (see the module header).
            headers.clear();
            checkpoints.clear();
            let (mut prev_doc, mut cumulative) = (0u32, 0u32);
            for (i, &(doc_id, count, end)) in doc_runs.iter().enumerate() {
                let start = if i == 0 { 0 } else { doc_runs[i - 1].2 };
                let len = end - start;
                if i > 0 && i % CHECKPOINT_EVERY == 0 {
                    checkpoints.extend_from_slice(&prev_doc.to_le_bytes());
                    checkpoints.extend_from_slice(&cumulative.to_le_bytes());
                    checkpoints.extend_from_slice(&(headers.len() as u32).to_le_bytes());
                }
                write_varint(&mut headers, doc_id.wrapping_sub(prev_doc) as u64);
                write_varint(&mut headers, len as u64);
                write_varint(&mut headers, count as u64);
                prev_doc = doc_id;
                cumulative += len as u32;
            }

            write_varint(&mut entry_data, doc_runs.len() as u64);
            // Where the payloads start, so a lookup never decodes headers it
            // does not need (see the module header).
            write_varint(&mut entry_data, headers.len() as u64);
            entry_data.extend_from_slice(&checkpoints);
            entry_data.extend_from_slice(&headers);
            entry_data.extend_from_slice(&payload_data);
        }
        // The offset table is u32: a sidecar past 4 GB cannot be addressed,
        // and writing a wrapped table would be silent corruption. A segment
        // is bounded well below this (LUCIVY_SFX_HEAP); refuse loudly if not.
        assert!(
            entry_data.len() <= u32::MAX as usize,
            "sfxpost: {} bytes of entry data exceed the 32-bit offset table",
            entry_data.len()
        );
        offset_table.push(entry_data.len() as u32);

        // Assemble final binary
        let table = block_offsets::encode(&offset_table);
        let mut out = Vec::with_capacity(8 + table.len() + entry_data.len());
        out.extend_from_slice(if self.spans { MAGIC_V4 } else { MAGIC_V5 });
        out.extend_from_slice(&(num_terms as u32).to_le_bytes());
        out.extend_from_slice(&table);
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
    /// `SFP3`/`SFP4`: varint blocks. `SFP2` files keep their fixed-width arrays.
    v3: bool,
    /// `SFP4`: the offset table is block-coded; `(directory start,
    /// blocks start)` in `data`, the directory ending where the blocks
    /// start and the blocks at `entry_data_start`.
    block_table: Option<(usize, usize)>,
    /// Whether the entries carry byte spans (`SFP2`-`SFP4`) or positions
    /// only (`SFP5`).
    spans: bool,
    /// Shard dictionary mode: the segment's `.gmap`, so that callers ask
    /// by global id and the file answers by local ordinal.
    gmap: Option<common::OwnedBytes>,
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
        let (v3, block, spans) = match &data[0..4] {
            m if m == MAGIC_V5 => (true, true, false),
            m if m == MAGIC_V4 => (true, true, true),
            m if m == MAGIC_V3 => (true, false, true),
            m if m == MAGIC_V2 => (false, false, true),
            _ => return None,
        };
        let num_terms = u32::from_le_bytes(data[4..8].try_into().ok()?);
        if block {
            let (table, used) = BlockOffsets::parse(&data[8..])?;
            if table.len() != num_terms + 1 {
                return None;
            }
            // Directory after the 12-byte table header; blocks after the directory.
            let num_blocks = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
            let dir_start = 8 + 12;
            let blocks_start = dir_start + num_blocks * 4;
            return Some(Self {
                data, num_terms, offsets_start: 8, entry_data_start: 8 + used, v3,
                block_table: Some((dir_start, blocks_start)),
                spans,
                gmap: None,
            });
        }
        let offsets_size = (num_terms as usize + 1) * 4;
        if data.len() < 8 + offsets_size {
            return None;
        }
        let offsets_start = 8;
        let entry_data_start = 8 + offsets_size;
        Some(Self { data, num_terms, offsets_start, entry_data_start, v3, block_table: None, spans, gmap: None })
    }

    /// Whether the entries carry byte spans. `false` on an `SFP5` file: the
    /// span-carrying accessors then return zero spans, and the offset of a
    /// position comes from `.posmap` (`PosMapReader::byte_at`).
    pub fn has_byte_spans(&self) -> bool {
        self.spans
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
    fn filtered_indices(header: &OrdinalHeader<'_>, filter: &dyn crate::query::posting_resolver::DocFilter) -> Vec<usize> {
        let n = header.num_docs;
        // Rough break-even: a binary search costs ~log2(n) probes.
        let probe_cost = (usize::BITS - n.leading_zeros()) as usize;
        if filter.len().saturating_mul(probe_cost) < n {
            let mut idx: Vec<usize> = Vec::new();
            filter.for_each(&mut |d| if let Some(i) = header.find_doc(d) { idx.push(i); });
            idx.sort_unstable();
            idx
        } else {
            let mut idx = Vec::new();
            header.for_each_doc(|i, doc_id, _, _| {
                if filter.contains(doc_id) { idx.push(i); }
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

    /// Take global ids (dictionary segment).
    pub fn with_gmap(mut self, gmap: common::OwnedBytes) -> Self {
        self.gmap = Some(gmap);
        self
    }

    /// The local ordinal a caller's id names: itself without a gmap.
    #[inline]
    fn local(&self, ordinal: u32) -> Option<u32> {
        match &self.gmap {
            Some(bytes) => super::gmap::GmapReader::open(bytes)?.local(ordinal),
            None => Some(ordinal),
        }
    }

    /// Number of terms.
    pub fn num_terms(&self) -> u32 {
        self.num_terms
    }

    /// Get all posting entries for a given ordinal.
    pub fn entries(&self, ordinal: u32) -> Vec<SfxPostingEntry> {
        // `entries_filtered` maps the ordinal itself.
        self.entries_filtered(ordinal, None)
    }

    /// Get posting entries for a given ordinal, optionally filtered by doc_ids.
    /// When filter is Some, only entries whose doc_id is in the set are returned.
    /// Uses binary search on the doc_id array — O(log n) per filtered doc.
    pub fn entries_filtered(
        &self,
        ordinal: u32,
        filter: Option<&dyn crate::query::posting_resolver::DocFilter>,
    ) -> Vec<SfxPostingEntry> {
        let Some(ordinal) = self.local(ordinal) else { return Vec::new(); };
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
        let Some(ordinal) = self.local(ordinal) else { return false; };
        if ordinal >= self.num_terms { return false; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return false };
        header.find_doc(doc_id).is_some()
    }

    /// Get entries for a single doc_id. O(log n) search + decode only that doc's payload.
    pub fn entries_for_doc(&self, ordinal: u32, target_doc: u32) -> Vec<SfxPostingEntry> {
        let Some(ordinal) = self.local(ordinal) else { return Vec::new(); };
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
        let Some(ordinal) = self.local(ordinal) else { return None; };
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

    /// The occurrences of `ordinal` by position, optionally restricted to
    /// the documents of `filter` — the v3 briques' resolution. Works on
    /// every layout; on `SFP5` it is the whole of what the file holds.
    pub fn positions_filtered(
        &self,
        ordinal: u32,
        filter: Option<&dyn crate::query::posting_resolver::DocFilter>,
    ) -> Vec<PositionEntry> {
        let Some(ordinal) = self.local(ordinal) else { return Vec::new(); };
        if ordinal >= self.num_terms {
            return Vec::new();
        }
        let Some(header) = self.read_ordinal_header(ordinal) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let Some(filter) = filter else {
            header.for_each_doc(|_, doc_id, offset, count| {
                header.walk_payload(offset as usize, count as usize, |ti, _, _| {
                    result.push(PositionEntry { doc_id, token_index: ti });
                    true
                });
                true
            });
            return result;
        };
        for i in Self::filtered_indices(&header, filter) {
            let (doc_id, offset, count) = header.doc_at(i);
            header.walk_payload(offset as usize, count as usize, |ti, _, _| {
                result.push(PositionEntry { doc_id, token_index: ti });
                true
            });
        }
        result
    }

    /// The positions of `ordinal` in one document, in order; empty when
    /// the document has none.
    pub fn positions_for_doc(&self, ordinal: u32, target_doc: u32) -> Vec<u32> {
        let Some(ordinal) = self.local(ordinal) else { return Vec::new(); };
        if ordinal >= self.num_terms { return Vec::new(); }
        let Some(header) = self.read_ordinal_header(ordinal) else { return Vec::new() };
        let Some((_, offset, count)) = header.find_doc_full(target_doc) else { return Vec::new() };
        let mut out = Vec::with_capacity(count as usize);
        header.walk_payload(offset as usize, count as usize, |ti, _, _| {
            out.push(ti);
            true
        });
        out
    }

    /// Whether `ordinal` occurs at `position` in `doc_id`: one document
    /// lookup and a scan that stops at the first position past the target.
    pub fn has_position(&self, ordinal: u32, doc_id: u32, position: u32) -> bool {
        let Some(ordinal) = self.local(ordinal) else { return false; };
        if ordinal >= self.num_terms { return false; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return false };
        let Some((_, offset, count)) = header.find_doc_full(doc_id) else { return false };
        let mut found = false;
        header.walk_payload(offset as usize, count as usize, |ti, _, _| {
            if ti == position { found = true; return false; }
            ti < position
        });
        found
    }

    /// Visit every occurrence of an ordinal as `(doc_id, token_index)`
    /// without allocating (the merge path).
    pub fn for_each_position(&self, ordinal: u32, mut f: impl FnMut(u32, u32)) {
        let Some(ordinal) = self.local(ordinal) else { return; };
        if ordinal >= self.num_terms { return; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return };
        header.for_each_doc(|_, doc_id, offset, count| {
            header.walk_payload(offset as usize, count as usize, |ti, _, _| {
                f(doc_id, ti);
                true
            });
            true
        });
    }

    /// doc_freq: number of unique docs for an ordinal. O(1) — just read the header.
    pub fn doc_freq(&self, ordinal: u32) -> u32 {
        let Some(ordinal) = self.local(ordinal) else { return 0; };
        if ordinal >= self.num_terms { return 0; }
        let Some(header) = self.read_ordinal_header(ordinal) else { return 0 };
        header.num_docs as u32
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn read_offset(&self, idx: u32) -> u32 {
        if let Some((dir_start, blocks_start)) = self.block_table {
            let table = BlockOffsets::from_parts(
                self.num_terms + 1,
                &self.data[dir_start..blocks_start],
                &self.data[blocks_start..self.entry_data_start],
            );
            return table.get(idx);
        }
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
            // Both counts come from the file: bound them by the block before
            // any arithmetic on them. A document costs at least three header
            // bytes, so `num_docs` cannot exceed the block's length — and the
            // checkpoint arithmetic below then cannot overflow either.
            let num_docs = read_varint_u32(data, &mut pos)? as usize;
            if num_docs > data.len() { return None; }
            let headers_len = read_varint_u32(data, &mut pos)? as usize;
            let cp_start = pos;
            let cp_len = checkpoints_for(num_docs) * CHECKPOINT_SIZE;
            let headers_start = cp_start.checked_add(cp_len)?;
            let payload_start = headers_start.checked_add(headers_len)?;
            if payload_start > data.len() { return None; }
            return Some(OrdinalHeader {
                num_docs,
                layout: HeaderLayout::V3 {
                    checkpoints: &data[cp_start..headers_start],
                    headers: &data[headers_start..payload_start],
                },
                payload_data: &data[payload_start..],
                spans: self.spans,
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
            spans: true,
        })
    }

    /// Visit every entry of an ordinal as `(doc_id, token_index, byte_from,
    /// byte_to)` without allocating. The merge walks every ordinal of every
    /// source segment this way — `entries()` built one `Vec` per ordinal
    /// per segment on that path.
    pub fn for_each_entry(&self, ordinal: u32, mut f: impl FnMut(u32, u32, u32, u32)) {
        let Some(ordinal) = self.local(ordinal) else { return (); };
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
    /// Whether an entry is three varints (spans) or one (`SFP5`).
    spans: bool,
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
    fn doc_at(&self, i: usize) -> (u32, u32, u32) {
        match &self.layout {
            // V2 stored the count in a `u16` (and wrapped above it); V3
            // stores the full count, so the count is a `u32` throughout.
            HeaderLayout::V2 { doc_ids, payload_offsets, entry_counts } => (
                u32::from_le_bytes(doc_ids[i * 4..i * 4 + 4].try_into().unwrap()),
                u32::from_le_bytes(payload_offsets[i * 4..i * 4 + 4].try_into().unwrap()),
                u16::from_le_bytes(entry_counts[i * 2..i * 2 + 2].try_into().unwrap()) as u32,
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
                    let Some(d_doc) = read_varint_u32(headers, &mut pos) else { break };
                    let Some(len) = read_varint_u32(headers, &mut pos) else { break };
                    let Some(n) = read_varint_u32(headers, &mut pos) else { break };
                    doc = doc.wrapping_add(d_doc);
                    count = n;
                    if j == i {
                        break;
                    }
                    // `offset` is where the *next* document's payload starts,
                    // so a length is added only once its document is passed.
                    offset = offset.wrapping_add(len);
                }
                (doc, offset, count)
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
    fn for_each_doc(&self, mut f: impl FnMut(usize, u32, u32, u32) -> bool) {
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
                        read_varint_u32(headers, &mut pos),
                        read_varint_u32(headers, &mut pos),
                        read_varint_u32(headers, &mut pos),
                    ) else { return };
                    doc = doc.wrapping_add(d_doc);
                    if !f(i, doc, offset, n) { return; }
                    offset = offset.wrapping_add(len);
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
        // of the shared module costs 14 % on the 21-query panel here. The
        // loop cannot read past its slice: a short read decodes a truncated
        // value and the `pos >= data.len()` guard ends the walk; and
        // `decode_vint` stops after five bytes, so a corrupt run of
        // continuation bits cannot shift past 32 bits.
        let (mut prev_ti, mut prev_bf) = (0u32, 0u32);
        if !self.spans {
            // `SFP5`: one delta per entry, no span to hand back.
            for _ in 0..count {
                if pos >= data.len() { return }
                let (a, n) = decode_vint(&data[pos..]); pos += n;
                prev_ti = prev_ti.wrapping_add(a);
                if !f(prev_ti, 0, 0) {
                    return;
                }
            }
            return;
        }
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
    fn find_doc(&self, doc_id: u32) -> Option<usize> {
        self.find_doc_full(doc_id).map(|(i, _, _)| i)
    }

    /// Binary search on doc_id, returning the document's index, payload offset
    /// and entry count. V2 probes three arrays; V3 narrows to one run of
    /// `CHECKPOINT_EVERY` documents through the checkpoints, then decodes it —
    /// once. Returning the triple is what keeps a lookup to a single scan:
    /// every caller needs the payload right after, and `doc_at(i)` would
    /// restart the same run from its checkpoint.
    fn find_doc_full(&self, doc_id: u32) -> Option<(usize, u32, u32)> {
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
                // answer, if any, is in the CHECKPOINT_EVERY documents that
                // follow it.
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
                // from the checkpoint at every step — a quadratic run for a
                // lookup that needs one, and `entry_at` runs one per emitted
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
                        read_varint_u32(headers, &mut pos),
                        read_varint_u32(headers, &mut pos),
                        read_varint_u32(headers, &mut pos),
                    ) else { return None };
                    doc = doc.wrapping_add(d_doc);
                    match doc.cmp(&doc_id) {
                        std::cmp::Ordering::Less => {
                            offset = offset.wrapping_add(len);
                            continue;
                        }
                        std::cmp::Ordering::Equal => return Some((i, offset, n)),
                        std::cmp::Ordering::Greater => return None,
                    }
                }
                None
            }
        }
    }
}


/// Decode one `u32` vint. At most five bytes are consumed: a sixth byte
/// with the continuation bit set would shift by 35, which a corrupt file
/// can ask for and which panics in debug builds.
fn decode_vint(data: &[u8]) -> (u32, usize) {
    let mut result = 0u32;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate().take(5) {
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        shift += 7;
    }
    (result, data.len().min(5))
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
    use std::collections::HashSet;
    use crate::query::posting_resolver::DocFilter;

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

    /// `SFP5` keeps every position of `SFP4` and answers the position
    /// accessors alike; it holds no span, says so, and is smaller.
    #[test]
    fn positions_only_layout_reads_like_the_spanned_one() {
        let mut full = SfxPostWriterV2::new(3);
        let mut bare = SfxPostWriterV2::positions_only(3);
        let mut expect: Vec<Vec<(u32, u32)>> = vec![Vec::new(); 3];
        for doc in 0..50u32 {
            for k in 0..(doc % 4) {
                let ti = doc * 3 + k * 7;
                let bf = ti * 5;
                let ord = (doc + k) % 3;
                full.add_entry(ord, doc, ti, bf, bf + 4);
                bare.add_entry(ord, doc, ti, bf, bf + 4);
                expect[ord as usize].push((doc, ti));
            }
        }
        let (a, b) = (full.finish(), bare.finish());
        assert_eq!(&a[0..4], MAGIC_V4);
        assert_eq!(&b[0..4], MAGIC_V5);
        assert!(b.len() < a.len(), "SFP5 {} B is not smaller than SFP4 {} B", b.len(), a.len());
        let (ra, rb) = (SfxPostReaderV2::open(a).unwrap(), SfxPostReaderV2::open(b).unwrap());
        assert!(ra.has_byte_spans());
        assert!(!rb.has_byte_spans());
        for ord in 0..3u32 {
            expect[ord as usize].sort_unstable();
            let pos = |r: &SfxPostReaderV2| r.positions_filtered(ord, None).iter()
                .map(|e| (e.doc_id, e.token_index)).collect::<Vec<_>>();
            assert_eq!(pos(&ra), expect[ord as usize], "ord {ord}: SFP4 positions");
            assert_eq!(pos(&rb), expect[ord as usize], "ord {ord}: SFP5 positions");
            assert_eq!(ra.doc_freq(ord), rb.doc_freq(ord));
            let filter: HashSet<u32> = [3, 17, 46].into();
            assert_eq!(
                rb.positions_filtered(ord, Some(&filter as &dyn DocFilter)),
                ra.positions_filtered(ord, Some(&filter as &dyn DocFilter)),
            );
            for doc in [0u32, 7, 17, 49, 50] {
                assert_eq!(rb.positions_for_doc(ord, doc), ra.positions_for_doc(ord, doc), "ord {ord} doc {doc}");
                assert_eq!(rb.has_doc(ord, doc), ra.has_doc(ord, doc));
                for ti in [0u32, 21, 28, 35, 1000] {
                    assert_eq!(rb.has_position(ord, doc, ti), ra.has_position(ord, doc, ti), "ord {ord} doc {doc} ti {ti}");
                    assert_eq!(rb.entry_at(ord, doc, ti).is_some(), ra.entry_at(ord, doc, ti).is_some());
                }
            }
            let mut walked = Vec::new();
            rb.for_each_position(ord, |d, ti| walked.push((d, ti)));
            assert_eq!(walked, expect[ord as usize]);
            // The span-carrying accessors hand back zero spans, not garbage.
            assert!(rb.entries(ord).iter().all(|e| e.byte_from == 0 && e.byte_to == 0));
            assert_eq!(rb.entries(ord).len(), expect[ord as usize].len());
        }
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
        let entries = reader.entries_filtered(0, Some(&filter as &dyn DocFilter));
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
            assert_eq!(&v3[0..4], MAGIC_V4, "the writer must emit SFP4");
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
                    r3.entries_filtered(1, Some(&filter as &dyn DocFilter)), r2.entries_filtered(1, Some(&filter as &dyn DocFilter)),
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

    /// A corrupt `num_docs` or `headers_len` must be refused, not trusted:
    /// the first `SFP3` reader multiplied `num_docs` before bounding it
    /// (overflow in debug, a wrapped size in release, a truncated one on
    /// wasm32).
    #[test]
    fn v3_refuses_corrupt_block_counts() {
        let mut w = SfxPostWriterV2::new(1);
        for &(d, ti, bf, bt) in &sample(20) {
            w.add_entry(0, d, ti, bf, bt);
        }
        let good = w.finish();
        let entry_data_start = 8 + 2 * 4;
        for bad in [u32::MAX as u64, u32::MAX as u64 - 1, 1u64 << 40, 100_000] {
            // Overwrite the block's leading varint(s) with `bad`.
            let mut data = good[..entry_data_start].to_vec();
            write_varint(&mut data, bad);
            write_varint(&mut data, bad);
            data.extend_from_slice(&good[entry_data_start + 2..]);
            if let Some(r) = SfxPostReaderV2::open(data) {
                assert!(r.entries(0).is_empty(), "bad count {bad} must read as an empty ordinal");
                assert!(!r.has_doc(0, 1));
                assert_eq!(r.doc_freq(0), 0);
            }
        }
        // Six continuation bytes in a payload: `decode_vint` must stop.
        let r = SfxPostReaderV2::open(good.clone()).unwrap();
        let payload = [0xFFu8; 12];
        let mut pos = 0;
        while pos < payload.len() {
            let (_, n) = decode_vint(&payload[pos..]);
            assert!(n > 0 && n <= 5);
            pos += n;
        }
        assert!(!r.entries(0).is_empty());
    }

    #[test]
    fn test_v2_not_v2_format() {
        // V1 data doesn't start with "SFP2"
        let v1_data = vec![0u8; 100];
        assert!(SfxPostReaderV2::open(v1_data).is_none());
    }
}
