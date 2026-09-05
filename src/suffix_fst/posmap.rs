//! Position-to-ordinal map: for each (doc_id, position) → ordinal.
//!
//! The reverse of the posting index. Enables O(1) lookup of which token
//! ordinal sits at a given position in a given document. Used by regex
//! cross-token search to validate the path between two known literal
//! positions without sibling walk.
//!
//! Format:
//! ```text
//! [4 bytes] magic "PMAP" (4-byte slots) or "PMP3" (3-byte slots)
//! [4 bytes] num_docs: u32 LE
//! [8 bytes × (num_docs + 1)] offset table (byte offset into data section)
//! Data section (per doc):
//!   [slot × num_tokens] ordinals, little-endian, one per position;
//!   all-ones (u32::MAX or 0xFFFFFF) = no token at this position
//! ```
//!
//! `PMP3` is written since 4 September 2026 whenever every ordinal fits in
//! 24 bits — always the case for a v3 segment, whose FST refuses larger
//! ordinals (`builder_v3::ORDINAL_MASK`). One slot per position on the whole
//! corpus: the 4th byte was 25 % of the file for nothing. A writer that sees
//! a wider ordinal (a v2 segment) falls back to `PMAP`; the reader takes both.
//!
//! `PMP4` (written since 5 September 2026 when the writer knows the
//! `own_len` of every ordinal) adds **byte checkpoints**: the source byte
//! offset of every `CHECKPOINT_EVERY`-th position, so that the byte offset
//! of any position derives from the checkpoint before it plus the
//! `own_len` (termtexts META) of the positions in between — chunk `p + 1`
//! starts at `byte_from(p) + own_len(p)` (`collector_v3`: `offset +=
//! chunk_len`). That is what lets the postings drop their byte spans
//! (`SFP5` / `WSP5`): 0.25 B per position here against ~2.9 B per posting
//! entry there. Offsets restart at 0 on every field value; the collector
//! leaves one empty position between two values, and the walk resets on
//! an empty slot.
//!
//! ```text
//! [4 bytes] magic "PMP4"
//! [4 bytes] num_docs: u32 LE
//! [8 bytes × (num_docs + 1)] offset table (byte offset into data section)
//! Data section (per doc):
//!   [u32 num_tokens]
//!   [u32 × ceil(num_tokens / 16)] byte offset of positions 0, 16, 32, …
//!   [3 bytes × num_tokens] ordinals, 0xFFFFFF = no token at this position
//! ```

/// Slot value meaning "no token at this position", in either width.
const EMPTY: u32 = u32::MAX;
const MAX_ORDINAL_3: u32 = 0xFF_FFFF;
/// Positions between two byte checkpoints in `PMP4`. A derivation walks at
/// most this many META records; 16 costs 0.25 B per position.
pub const CHECKPOINT_EVERY: u32 = 16;

/// Byte offset of every `CHECKPOINT_EVERY`-th position of one document,
/// from the ordinals of its positions and their `own_len`. An empty slot is
/// a value boundary: the next position starts a new value at offset 0.
fn byte_checkpoints(slots: &[u32], own_len: &dyn Fn(u32) -> Option<u16>) -> Vec<u32> {
    let mut out = Vec::with_capacity(slots.len().div_ceil(CHECKPOINT_EVERY as usize));
    let mut acc = 0u32;
    for (p, &ord) in slots.iter().enumerate() {
        if p % CHECKPOINT_EVERY as usize == 0 {
            out.push(acc);
        }
        if ord == EMPTY {
            acc = 0;
        } else {
            acc = acc.wrapping_add(own_len(ord).unwrap_or(0) as u32);
        }
    }
    out
}

/// Builds a position-to-ordinal map during indexation.
pub struct PosMapWriter {
    /// Per doc: ordinals in position order. Index = doc_id.
    docs: Vec<Vec<u32>>,
}

impl Default for PosMapWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl PosMapWriter {
    /// Creates a new position-to-ordinal map writer.
    pub fn new() -> Self {
        Self { docs: Vec::new() }
    }

    /// Record that `ordinal` appears at `position` in `doc_id`.
    pub fn add(&mut self, doc_id: u32, position: u32, ordinal: u32) {
        let d = doc_id as usize;
        if d >= self.docs.len() {
            self.docs.resize(d + 1, Vec::new());
        }
        let doc = &mut self.docs[d];
        let p = position as usize;
        if p >= doc.len() {
            doc.resize(p + 1, u32::MAX);
        }
        // One ordinal per (doc, position) is what makes posmap an exact inverse
        // of sfxpost — and what the chain resolver relies on to replace posting
        // materialisation with a lookup. A second writer here would be silently
        // overwritten, so count it instead of assuming it never happens.
        if doc[p] != u32::MAX && doc[p] != ordinal {
            crate::suffix_fst::briques::profile::bump(|c| &c.n_posmap_collisions, 1);
        }
        doc[p] = ordinal;
    }

    /// Add an empty doc (no tokens).
    pub fn add_empty_doc(&mut self) {
        self.docs.push(Vec::new());
    }

    /// Serialize to binary format (`PMP3`, or `PMAP` for wide ordinals):
    /// no byte checkpoints. `serialize_with_lens` is what a v3 writer calls.
    pub fn serialize(&self) -> Vec<u8> {
        self.serialize_with_lens(None)
    }

    /// Serialize as `PMP4` when `own_len` is given and every ordinal is
    /// narrow — the layout with byte checkpoints, from which the byte offset
    /// of any position derives (`PosMapReader::byte_at`). Without lengths,
    /// or with a wide ordinal, the layouts without checkpoints.
    pub fn serialize_with_lens(&self, own_len: Option<&dyn Fn(u32) -> Option<u16>>) -> Vec<u8> {
        let num_docs = self.docs.len() as u32;
        // 3-byte slots unless an ordinal needs the 4th byte (EMPTY is not an ordinal).
        let narrow = self.docs.iter().flatten()
            .all(|&o| o == EMPTY || o < MAX_ORDINAL_3);
        let width = if narrow { 3 } else { 4 };
        let checkpoints = narrow && own_len.is_some();
        let header_size = 4 + 4 + (num_docs as usize + 1) * 8; // magic + num_docs + offsets
        let doc_size = |n: usize| -> usize {
            n * width + if checkpoints { 4 + 4 * n.div_ceil(CHECKPOINT_EVERY as usize) } else { 0 }
        };
        let data_size: usize = self.docs.iter().map(|d| doc_size(d.len())).sum();
        let mut buf = Vec::with_capacity(header_size + data_size);

        // Magic
        buf.extend_from_slice(if checkpoints { b"PMP4" } else if narrow { b"PMP3" } else { b"PMAP" });
        buf.extend_from_slice(&num_docs.to_le_bytes());

        // Offset table
        let mut offset: u64 = 0;
        for doc in &self.docs {
            buf.extend_from_slice(&offset.to_le_bytes());
            offset += doc_size(doc.len()) as u64;
        }
        buf.extend_from_slice(&offset.to_le_bytes()); // sentinel

        // Data
        for doc in &self.docs {
            if let Some(own_len) = own_len.filter(|_| checkpoints) {
                buf.extend_from_slice(&(doc.len() as u32).to_le_bytes());
                for ck in byte_checkpoints(doc, own_len) {
                    buf.extend_from_slice(&ck.to_le_bytes());
                }
            }
            for &ord in doc {
                buf.extend_from_slice(&ord.to_le_bytes()[..width]);
            }
        }

        buf
    }
}

/// Reads a position-to-ordinal map. O(1) lookup.
pub struct PosMapReader<'a> {
    num_docs: u32,
    offsets: &'a [u8],
    data: &'a [u8],
    /// Bytes per slot: 3 (`PMP3`, `PMP4`) or 4 (`PMAP`).
    width: usize,
    /// The empty marker at this width: all ones.
    empty: u32,
    /// `PMP4`: each document's data starts with its token count and its
    /// byte checkpoints, before the slots.
    checkpoints: bool,
    /// Shard dictionary mode: the slots hold local ordinals, callers speak
    /// global ids (`gmap.rs`).
    gmap: Option<super::gmap::GmapReader<'a>>,
}

impl<'a> PosMapReader<'a> {
    /// Open from raw bytes. Returns None if data is too small or invalid magic.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let (width, empty, checkpoints) = match &bytes[0..4] {
            b"PMAP" => (4, EMPTY, false),
            b"PMP3" => (3, MAX_ORDINAL_3, false),
            b"PMP4" => (3, MAX_ORDINAL_3, true),
            _ => return None,
        };
        let num_docs = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let offsets_size = (num_docs as usize + 1) * 8;
        if bytes.len() < 8 + offsets_size {
            return None;
        }
        let offsets = &bytes[8..8 + offsets_size];
        let data = &bytes[8 + offsets_size..];
        Some(Self { num_docs, offsets, data, width, empty, checkpoints, gmap: None })
    }

    /// Whether the file carries byte checkpoints (`PMP4`), i.e. whether
    /// `byte_at` can answer. Without them the postings carry the spans.
    pub fn has_byte_checkpoints(&self) -> bool {
        self.checkpoints
    }

    /// Translate every ordinal read to its global id (dictionary segment).
    pub fn with_gmap(mut self, gmap: super::gmap::GmapReader<'a>) -> Self {
        self.gmap = Some(gmap);
        self
    }

    #[inline]
    fn out(&self, local: u32) -> u32 {
        match &self.gmap { Some(g) => g.global(local), None => local }
    }

    /// Bytes per slot, 3 or 4.
    pub fn slot_width(&self) -> usize {
        self.width
    }

    /// The document's data: its slots, `width` bytes each — after the
    /// token count and the checkpoints in `PMP4`.
    #[inline]
    fn doc_data(&self, doc_id: u32) -> &'a [u8] {
        let raw = self.doc_raw(doc_id);
        if !self.checkpoints {
            return raw;
        }
        let skip = self.checkpoint_bytes(raw);
        &raw[skip.min(raw.len())..]
    }

    /// The document's whole region of the data section.
    #[inline]
    fn doc_raw(&self, doc_id: u32) -> &'a [u8] {
        let start = (self.read_offset(doc_id) as usize).min(self.data.len());
        let end = (self.read_offset(doc_id + 1) as usize).min(self.data.len());
        &self.data[start..end.max(start)]
    }

    /// `PMP4`: bytes of the token count and the checkpoints at the head of
    /// a document's region.
    #[inline]
    fn checkpoint_bytes(&self, raw: &[u8]) -> usize {
        if raw.len() < 4 { return raw.len(); }
        let n = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        4 + 4 * n.div_ceil(CHECKPOINT_EVERY as usize)
    }

    /// Source byte offset of `position` in `doc_id`, derived from the byte
    /// checkpoint before it and the `own_len` of the positions in between —
    /// `own_len` answers for the ids `ordinal_at` returns (global ids on a
    /// dictionary segment). `None` without checkpoints (`PMP3`, `PMAP`: the
    /// postings carry the spans), past the document, on an empty slot, or
    /// when a length is unknown.
    pub fn byte_at(&self, doc_id: u32, position: u32, own_len: impl Fn(u32) -> Option<u16>) -> Option<u32> {
        if !self.checkpoints || doc_id >= self.num_docs {
            return None;
        }
        let raw = self.doc_raw(doc_id);
        if raw.len() < 4 { return None; }
        let n = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if position >= n { return None; }
        let ck = (position / CHECKPOINT_EVERY) as usize;
        let at = 4 + 4 * ck;
        let mut acc = u32::from_le_bytes(raw[at..at + 4].try_into().ok()?);
        let slots = &raw[self.checkpoint_bytes(raw)..];
        for p in (ck as u32 * CHECKPOINT_EVERY)..position {
            match self.slot(slots, p as usize * self.width) {
                None => acc = 0,
                Some(local) => acc = acc.wrapping_add(own_len(self.out(local))? as u32),
            }
        }
        self.slot(slots, position as usize * self.width)?;
        Some(acc)
    }

    /// The slot at byte offset `off`, or None when it is the empty marker.
    #[inline]
    fn slot(&self, doc_data: &[u8], off: usize) -> Option<u32> {
        let ord = if self.width == 4 {
            u32::from_le_bytes([doc_data[off], doc_data[off + 1], doc_data[off + 2], doc_data[off + 3]])
        } else {
            u32::from_le_bytes([doc_data[off], doc_data[off + 1], doc_data[off + 2], 0])
        };
        if ord == self.empty { None } else { Some(ord) }
    }

    /// Get the ordinal at (doc_id, position). Returns None if out of bounds.
    pub fn ordinal_at(&self, doc_id: u32, position: u32) -> Option<u32> {
        if doc_id >= self.num_docs {
            return None;
        }
        let doc_data = self.doc_data(doc_id);
        let num_tokens = doc_data.len() / self.width;
        let p = position as usize;
        if p >= num_tokens {
            return None;
        }
        self.slot(doc_data, p * self.width).map(|o| self.out(o))
    }

    /// Get ordinals for a range of positions [pos_from, pos_to) in a doc.
    /// Returns Vec of (position, ordinal) pairs for valid positions.
    pub fn ordinals_range(&self, doc_id: u32, pos_from: u32, pos_to: u32) -> Vec<(u32, u32)> {
        if doc_id >= self.num_docs {
            return Vec::new();
        }
        let doc_data = self.doc_data(doc_id);
        let num_tokens = doc_data.len() / self.width;
        let mut result = Vec::new();
        for pos in pos_from..pos_to.min(num_tokens as u32) {
            if let Some(ord) = self.slot(doc_data, pos as usize * self.width) {
                result.push((pos, self.out(ord)));
            }
        }
        result
    }

    /// Number of documents in the map.
    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Number of tokens in a document.
    pub fn num_tokens(&self, doc_id: u32) -> u32 {
        if doc_id >= self.num_docs {
            return 0;
        }
        (self.doc_data(doc_id).len() / self.width) as u32
    }

    fn read_offset(&self, idx: u32) -> u64 {
        let pos = idx as usize * 8;
        u64::from_le_bytes(self.offsets[pos..pos + 8].try_into().unwrap())
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation (Derived)
// ─────────────────────────────────────────────────────────────────────

/// Index file wrapper for position maps (SfxIndexFile trait).
pub struct PosMapIndex {
    writer: PosMapWriter,
    /// `own_len` by ordinal, when the build says it (`on_own_len`): the
    /// map is then written as `PMP4`, with byte checkpoints.
    own_lens: Vec<u16>,
}

impl Default for PosMapIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl PosMapIndex {
    /// Creates a new position map index file instance.
    pub fn new() -> Self { Self { writer: PosMapWriter::new(), own_lens: Vec::new() } }
}

impl super::index_registry::SfxIndexFile for PosMapIndex {
    fn id(&self) -> &'static str { "posmap" }
    fn extension(&self) -> &'static str { "posmap" }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy { super::index_registry::MergeStrategy::EventDriven }

    fn on_own_len(&mut self, ord: u32, own_len: u16) {
        let o = ord as usize;
        if o >= self.own_lens.len() {
            self.own_lens.resize(o + 1, 0);
        }
        self.own_lens[o] = own_len;
    }

    fn on_posting(&mut self, ord: u32, doc_id: u32, position: u32, _bf: u32, _bt: u32) {
        self.writer.add(doc_id, position, ord);
    }

    fn serialize(&self) -> Vec<u8> {
        if self.own_lens.is_empty() {
            return self.writer.serialize();
        }
        let lens = &self.own_lens;
        self.writer.serialize_with_lens(Some(&|ord: u32| lens.get(ord as usize).copied()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posmap_roundtrip() {
        let mut writer = PosMapWriter::new();
        // Doc 0: 3 tokens
        writer.add(0, 0, 10); // pos 0 = ordinal 10
        writer.add(0, 1, 20); // pos 1 = ordinal 20
        writer.add(0, 2, 30); // pos 2 = ordinal 30
        // Doc 1: 2 tokens
        writer.add(1, 0, 5);
        writer.add(1, 1, 15);

        let data = writer.serialize();
        let reader = PosMapReader::open(&data).unwrap();

        assert_eq!(reader.ordinal_at(0, 0), Some(10));
        assert_eq!(reader.ordinal_at(0, 1), Some(20));
        assert_eq!(reader.ordinal_at(0, 2), Some(30));
        assert_eq!(reader.ordinal_at(0, 3), None); // out of bounds
        assert_eq!(reader.ordinal_at(1, 0), Some(5));
        assert_eq!(reader.ordinal_at(1, 1), Some(15));
        assert_eq!(reader.ordinal_at(2, 0), None); // no doc 2

        let range = reader.ordinals_range(0, 1, 3);
        assert_eq!(range, vec![(1, 20), (2, 30)]);
    }

    /// Ordinals under 2^24 get 3-byte slots; one wider ordinal keeps the
    /// 4-byte layout; both read back the same, empty slots included.
    #[test]
    fn narrow_and_wide_layouts_read_alike() {
        let fill = |w: &mut PosMapWriter| {
            w.add(0, 0, 7);
            w.add(0, 2, 0xFF_FFFE); // position 1 stays empty
            w.add_empty_doc();
            w.add(2, 0, 0);
        };
        let mut narrow = PosMapWriter::new(); fill(&mut narrow);
        let n = narrow.serialize();
        assert_eq!(&n[0..4], b"PMP3");
        let mut wide = PosMapWriter::new(); fill(&mut wide); wide.add(2, 1, 0x100_0000);
        let w = wide.serialize();
        assert_eq!(&w[0..4], b"PMAP");
        let (rn, rw) = (PosMapReader::open(&n).unwrap(), PosMapReader::open(&w).unwrap());
        assert_eq!((rn.slot_width(), rw.slot_width()), (3, 4));
        for (doc, pos) in [(0, 0), (0, 1), (0, 2), (0, 3), (1, 0), (2, 0), (3, 0)] {
            assert_eq!(rn.ordinal_at(doc, pos), rw.ordinal_at(doc, pos), "doc {doc} pos {pos}");
        }
        assert_eq!(rn.ordinal_at(0, 1), None);
        assert_eq!(rn.ordinal_at(0, 2), Some(0xFF_FFFE));
        assert_eq!(rw.ordinal_at(2, 1), Some(0x100_0000));
        assert_eq!(rn.ordinals_range(0, 0, 3), vec![(0, 7), (2, 0xFF_FFFE)]);
        assert_eq!(rn.num_tokens(0), 3);
        assert_eq!(rn.num_tokens(1), 0);
    }

    /// `PMP4`: the byte offset of every position derives from the
    /// checkpoints and the lengths, across checkpoints and across a value
    /// boundary (an empty slot restarts at 0); `PMP3` answers `None`.
    #[test]
    fn byte_offsets_derive_from_checkpoints_and_lengths() {
        // Ordinal o has own_len o + 1.
        let own_len = |o: u32| Some((o + 1) as u16);
        let mut w = PosMapWriter::new();
        // Doc 0: 40 positions, ordinal = position % 5; position 23 is a
        // value boundary (empty).
        let mut expect: Vec<Option<u32>> = Vec::new();
        let mut acc = 0u32;
        for p in 0..40u32 {
            if p == 23 { expect.push(None); acc = 0; continue; }
            let o = p % 5;
            w.add(0, p, o);
            expect.push(Some(acc));
            acc += o + 1;
        }
        w.add_empty_doc();
        w.add(2, 0, 4);
        w.add(2, 1, 4);
        let bytes = w.serialize_with_lens(Some(&own_len));
        assert_eq!(&bytes[0..4], b"PMP4");
        let r = PosMapReader::open(&bytes).unwrap();
        assert!(r.has_byte_checkpoints());
        assert_eq!(r.num_tokens(0), 40);
        assert_eq!(r.num_tokens(1), 0);
        assert_eq!(r.num_tokens(2), 2);
        for p in 0..40u32 {
            assert_eq!(r.ordinal_at(0, p), if p == 23 { None } else { Some(p % 5) }, "ordinal at {p}");
            assert_eq!(r.byte_at(0, p, own_len), expect[p as usize], "byte at {p}");
        }
        assert_eq!(r.byte_at(0, 40, own_len), None);
        assert_eq!(r.byte_at(1, 0, own_len), None);
        assert_eq!(r.byte_at(2, 0, own_len), Some(0));
        assert_eq!(r.byte_at(2, 1, own_len), Some(5));
        assert_eq!(r.byte_at(3, 0, own_len), None);
        // An unknown length makes the derivation refuse rather than guess.
        assert_eq!(r.byte_at(0, 3, |o| if o == 1 { None } else { own_len(o) }), None);
        assert_eq!(r.byte_at(0, 1, |o| if o == 1 { None } else { own_len(o) }), Some(1));
        assert_eq!(r.ordinals_range(0, 22, 25), vec![(22, 2), (24, 4)]);

        let plain_bytes = w.serialize();
        let plain = PosMapReader::open(&plain_bytes).unwrap();
        assert!(!plain.has_byte_checkpoints());
        assert_eq!(plain.byte_at(0, 5, own_len), None);
        assert_eq!(plain.ordinal_at(0, 5), Some(0));
    }

    /// The registry adapter writes `PMP4` once it has been told the lengths,
    /// `PMP3` otherwise.
    #[test]
    fn registry_adapter_writes_checkpoints_when_lengths_are_known() {
        use super::super::index_registry::SfxIndexFile;
        let mut idx = PosMapIndex::new();
        idx.on_posting(7, 0, 0, 0, 0);
        idx.on_posting(3, 0, 1, 0, 0);
        let plain = idx.serialize();
        assert_eq!(&plain[0..4], b"PMP3");
        idx.on_own_len(7, 9);
        idx.on_own_len(3, 2);
        let bytes = idx.serialize();
        assert_eq!(&bytes[0..4], b"PMP4");
        let r = PosMapReader::open(&bytes).unwrap();
        let lens = |o: u32| Some(if o == 7 { 9 } else { 2 });
        assert_eq!(r.byte_at(0, 1, lens), Some(9));
    }

    #[test]
    fn test_posmap_empty_doc() {
        let mut writer = PosMapWriter::new();
        writer.add(0, 0, 42);
        writer.add_empty_doc(); // doc 1 is empty
        writer.add(2, 0, 99);

        let data = writer.serialize();
        let reader = PosMapReader::open(&data).unwrap();

        assert_eq!(reader.ordinal_at(0, 0), Some(42));
        assert_eq!(reader.num_tokens(1), 0);
        assert_eq!(reader.ordinal_at(2, 0), Some(99));
    }
}
