//! Word-level sfxpost for partition 0x02 (word-stripped entries).
//!
//! Separate from the chunk-level SfxPostV2 because word postings have
//! different semantics: position = last chunk of the word (for cross-word
//! chain adjacency), byte_from = first chunk start (for highlights),
//! byte_to = end of the word's content bytes (separators excluded).
//!
//! `WSP2` changed the meaning of byte_to from "end of the last chunk" to
//! "end of the content". A 0x02 key does not fix its word's length ("0ui" is
//! "0"+"ui" or "0u"+"i" under one ordinal), so the posting has to carry it;
//! readers must not derive it from the FST entry's metadata.
//!
//! Format `WSP2` (read-only now):
//! ```text
//! [4 bytes] magic "WSP2"
//! [4 bytes] num_ordinals (u32 LE)
//! [num_ordinals * 4 bytes] offset table (u32 LE per ordinal → byte offset into entries)
//! [4 bytes] sentinel offset (= end of entries)
//! [entries...] packed WordPostingEntry (20 bytes each)
//! ```
//!
//! Format `WSP3` (written since 25 August 2026): the same layout, with each
//! ordinal's entries delta-encoded and varint-packed. Five fixed `u32`
//! (20 bytes) become 6.9 bytes on a real index — measured over 8.87 M entries
//! of a kernel index, `lucivy_core/tests/test_wsp_density.rs`. This file is
//! 17 % of an index and 78-82 % of it is faulted in by a common query
//! (`test_touched_bytes`), so it dominates the working set; reading 2.9x less
//! of it is the point. There is no decompression pass: entries were already
//! decoded field by field, and a varint read replaces a `u32` read.
//!
//! ```text
//! [4 bytes] magic "WSP3"
//! [4 bytes] num_ordinals (u32 LE)
//! [(num_ordinals + 1) * 4 bytes] offset table (u32 LE → start of each block)
//! [blocks...]
//! ```
//!
//! One block, for a non-empty ordinal:
//! ```text
//! [varint] n                          number of entries
//! [c * 16 bytes] checkpoints          c = (n - 1) / 32, see below
//! [n * varints] entries
//! ```
//!
//! An entry is `d_doc`, `d_first`, `last - first`, `d_from`, `to - from`, where
//! `d_doc = doc - prev_doc` and, when the document is the same as the previous
//! entry's, `d_first` and `d_from` are deltas too (entries are sorted, so both
//! are small); on a new document they are absolute. Deltas are computed with
//! `wrapping_sub` and applied with `wrapping_add`, so a field that is not
//! monotone round-trips exactly — it only costs a wider varint.
//!
//! `entry_at` is a binary search on fixed-size records in WSP2, which varints
//! would break. Checkpoint `k` (every `CHECKPOINT_EVERY` entries) stores the
//! decoder state after entry `k * 32 - 1` — `(doc, first, from)` — and the
//! offset of entry `k * 32` from the start of the entries region. Searching
//! the checkpoints on `(doc, first)` narrows to one run of 32 entries, so a
//! lookup stays logarithmic and the checkpoints cost 1.5 MB on a 177 MB file.

use super::varint::{read_varint_u32, write_varint};

/// A single word-level posting entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WordPostingEntry {
    pub doc_id: u32,
    /// Token index of the first chunk of the word.
    pub first_position: u32,
    /// Token index of the last chunk of the word (used for adjacency check).
    pub last_position: u32,
    /// Start byte offset in the original text (first chunk start).
    pub byte_from: u32,
    /// End byte offset of the word's content in the original text. Content
    /// is contiguous from `byte_from`; separators come after.
    pub byte_to: u32,
}

const ENTRY_SIZE: usize = 20; // 5 × u32
const MAGIC: &[u8; 4] = b"WSP2";
const MAGIC_V3: &[u8; 4] = b"WSP3";
/// Entries between two checkpoints. 32 keeps a lookup at one binary search
/// over the checkpoints plus at most 32 decodes, for 16 bytes per 32 entries
/// (0.5 B/entry against the 13.3 B/entry the varints save).
const CHECKPOINT_EVERY: usize = 32;
const CHECKPOINT_SIZE: usize = 16; // doc, first, from, offset

/// Number of checkpoints an ordinal of `n` entries carries.
fn checkpoints_for(n: usize) -> usize {
    if n == 0 { 0 } else { (n - 1) / CHECKPOINT_EVERY }
}

/// Decoder state carried from one entry to the next.
#[derive(Clone, Copy, Default)]
struct DeltaState {
    doc: u32,
    first: u32,
    from: u32,
}

/// Decodes one entry at `*pos` and advances both the position and the state.
fn decode_entry(data: &[u8], pos: &mut usize, st: &mut DeltaState) -> Option<WordPostingEntry> {
    let d_doc = read_varint_u32(data, pos)?;
    let d_first = read_varint_u32(data, pos)?;
    let d_last = read_varint_u32(data, pos)?;
    let d_from = read_varint_u32(data, pos)?;
    let len = read_varint_u32(data, pos)?;
    let doc = st.doc.wrapping_add(d_doc);
    let same_doc = d_doc == 0;
    let first = if same_doc { st.first.wrapping_add(d_first) } else { d_first };
    let byte_from = if same_doc { st.from.wrapping_add(d_from) } else { d_from };
    st.doc = doc;
    st.first = first;
    st.from = byte_from;
    Some(WordPostingEntry {
        doc_id: doc,
        first_position: first,
        last_position: first.wrapping_add(d_last),
        byte_from,
        byte_to: byte_from.wrapping_add(len),
    })
}

// ─── Writer ──────────────────────────────────────────────────────────────

pub struct WordSfxPostWriter {
    entries: Vec<Vec<WordPostingEntry>>,
}

impl WordSfxPostWriter {
    pub fn new(num_ordinals: usize) -> Self {
        Self {
            entries: vec![Vec::new(); num_ordinals],
        }
    }

    pub fn add(&mut self, ordinal: u32, entry: WordPostingEntry) {
        if (ordinal as usize) < self.entries.len() {
            self.entries[ordinal as usize].push(entry);
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        let num_ords = self.entries.len() as u32;
        // Sort and dedup each ordinal's entries
        for entries in &mut self.entries {
            entries.sort();
            entries.dedup();
        }

        // One block per ordinal, assembled first so the offset table is exact.
        let header_size = 4 + 4 + (num_ords as usize + 1) * 4;
        let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(num_ords as usize);
        for entries in &self.entries {
            blocks.push(encode_block(entries));
        }

        let mut offsets: Vec<u32> = Vec::with_capacity(num_ords as usize + 1);
        let mut current = header_size as u32;
        for b in &blocks {
            offsets.push(current);
            current += b.len() as u32;
        }
        offsets.push(current); // sentinel

        let mut buf = Vec::with_capacity(current as usize);
        buf.extend_from_slice(MAGIC_V3);
        buf.extend_from_slice(&num_ords.to_le_bytes());
        for &off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        for b in blocks {
            buf.extend_from_slice(&b);
        }
        buf
    }
}

/// One ordinal's block: `n`, its checkpoints, then the delta-varint entries.
fn encode_block(entries: &[WordPostingEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    let n = entries.len();

    // Pass 1: the entries, recording the state and offset at each checkpoint.
    let mut body: Vec<u8> = Vec::with_capacity(n * 8);
    let mut checkpoints: Vec<(DeltaState, u32)> = Vec::with_capacity(checkpoints_for(n));
    let mut st = DeltaState::default();
    for (i, e) in entries.iter().enumerate() {
        if i > 0 && i % CHECKPOINT_EVERY == 0 {
            checkpoints.push((st, body.len() as u32));
        }
        // A zero document delta is what tells the decoder to read `first` and
        // `byte_from` as deltas. On the first entry of a block the state is
        // (0, 0, 0), so "delta from zero" and "absolute" are the same bytes —
        // no special case is needed, and none may be introduced: the decoder
        // has only `d_doc` to go on.
        let same_doc = e.doc_id == st.doc;
        write_varint(&mut body, (e.doc_id.wrapping_sub(st.doc)) as u64);
        write_varint(&mut body, (if same_doc { e.first_position.wrapping_sub(st.first) } else { e.first_position }) as u64);
        write_varint(&mut body, (e.last_position.wrapping_sub(e.first_position)) as u64);
        write_varint(&mut body, (if same_doc { e.byte_from.wrapping_sub(st.from) } else { e.byte_from }) as u64);
        write_varint(&mut body, (e.byte_to.wrapping_sub(e.byte_from)) as u64);
        st = DeltaState { doc: e.doc_id, first: e.first_position, from: e.byte_from };
    }

    // Pass 2: header + checkpoints + entries.
    let mut out = Vec::with_capacity(body.len() + checkpoints.len() * CHECKPOINT_SIZE + 5);
    write_varint(&mut out, n as u64);
    for (st, off) in &checkpoints {
        out.extend_from_slice(&st.doc.to_le_bytes());
        out.extend_from_slice(&st.first.to_le_bytes());
        out.extend_from_slice(&st.from.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
    }
    out.extend_from_slice(&body);
    out
}

// ─── Reader ──────────────────────────────────────────────────────────────

pub struct WordSfxPostReader<'a> {
    data: &'a [u8],
    num_ordinals: u32,
    /// `WSP3`: delta-varint blocks. `WSP2` segments written before 25 August
    /// 2026 keep their fixed 20-byte records and their own read paths.
    v3: bool,
}

impl<'a> WordSfxPostReader<'a> {
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 { return None; }
        let v3 = match &data[0..4] {
            m if m == MAGIC_V3 => true,
            m if m == MAGIC => false,
            _ => return None,
        };
        let num_ordinals = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let min_size = 8 + (num_ordinals as usize + 1) * 4;
        if data.len() < min_size { return None; }
        Some(Self { data, num_ordinals, v3 })
    }

    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }

    /// Byte range of an ordinal's records (WSP2) or block (WSP3).
    fn block_range(&self, ordinal: u32) -> Option<(usize, usize)> {
        if ordinal >= self.num_ordinals {
            return None;
        }
        let off_base = 8 + ordinal as usize * 4;
        let start = u32::from_le_bytes(self.data.get(off_base..off_base + 4)?.try_into().ok()?) as usize;
        let end = u32::from_le_bytes(self.data.get(off_base + 4..off_base + 8)?.try_into().ok()?) as usize;
        if start >= end || end > self.data.len() {
            return None;
        }
        Some((start, end))
    }

    /// `(number of entries, offset of the first checkpoint, offset of the
    /// first entry)` for a WSP3 block.
    fn block_header(&self, start: usize) -> Option<(usize, usize, usize)> {
        let mut pos = start;
        let n = read_varint_u32(self.data, &mut pos)? as usize;
        let checkpoints = pos;
        let entries = checkpoints + checkpoints_for(n) * CHECKPOINT_SIZE;
        if entries > self.data.len() {
            return None;
        }
        Some((n, checkpoints, entries))
    }

    /// Checkpoint `k` (1-based): the decoder state after entry `k * 32 - 1`
    /// and the offset of entry `k * 32` from the entries region.
    fn checkpoint(&self, checkpoints: usize, k: usize) -> Option<(DeltaState, usize)> {
        let o = checkpoints + (k - 1) * CHECKPOINT_SIZE;
        let g = |j: usize| -> Option<u32> {
            Some(u32::from_le_bytes(self.data.get(o + j * 4..o + j * 4 + 4)?.try_into().ok()?))
        };
        Some((DeltaState { doc: g(0)?, first: g(1)?, from: g(2)? }, g(3)? as usize))
    }

    /// Walk a WSP3 block, stopping when `f` returns `false`.
    fn walk_v3(&self, start: usize, end: usize, mut f: impl FnMut(WordPostingEntry) -> bool) {
        let Some((n, _, entries)) = self.block_header(start) else { return };
        let mut pos = entries;
        let mut st = DeltaState::default();
        for _ in 0..n {
            if pos >= end {
                break;
            }
            let Some(e) = decode_entry(self.data, &mut pos, &mut st) else { break };
            if !f(e) {
                break;
            }
        }
    }

    /// The entry of `ordinal` whose word starts at (`doc_id`, `first_position`).
    ///
    /// Entries are written sorted by (doc_id, first_position, ...), so this is a
    /// binary search over the fixed-size records — no list materialised. Used by
    /// the word_pos_map-driven resolver, once per emitted match.
    pub fn entry_at(&self, ordinal: u32, doc_id: u32, first_position: u32) -> Option<WordPostingEntry> {
        let (start, end) = self.block_range(ordinal)?;
        if self.v3 {
            return self.entry_at_v3(start, end, doc_id, first_position);
        }
        let num = (end - start) / ENTRY_SIZE;
        let key = |i: usize| -> (u32, u32) {
            let b = &self.data[start + i * ENTRY_SIZE..];
            (u32::from_le_bytes(b[0..4].try_into().unwrap()),
             u32::from_le_bytes(b[4..8].try_into().unwrap()))
        };
        let (mut lo, mut hi) = (0usize, num);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if key(mid) < (doc_id, first_position) { lo = mid + 1; } else { hi = mid; }
        }
        if lo >= num || key(lo) != (doc_id, first_position) {
            return None;
        }
        let b = &self.data[start + lo * ENTRY_SIZE..start + (lo + 1) * ENTRY_SIZE];
        Some(WordPostingEntry {
            doc_id,
            first_position,
            last_position: u32::from_le_bytes(b[8..12].try_into().ok()?),
            byte_from: u32::from_le_bytes(b[12..16].try_into().ok()?),
            byte_to: u32::from_le_bytes(b[16..20].try_into().ok()?),
        })
    }

    /// The entry keyed `(doc_id, first_position)` in a WSP3 block: a binary
    /// search over the checkpoints, then at most `CHECKPOINT_EVERY` decodes.
    fn entry_at_v3(&self, start: usize, end: usize, doc_id: u32, first_position: u32) -> Option<WordPostingEntry> {
        let (n, checkpoints, entries) = self.block_header(start)?;
        let key = (doc_id, first_position);
        let c = checkpoints_for(n);

        // Largest k whose state key is strictly below the target: the entry we
        // want, if it exists, is then among the 32 entries starting at k * 32.
        let (mut lo, mut hi) = (0usize, c);
        while lo < hi {
            let mid = lo + (hi - lo) / 2 + 1;
            let Some((st, _)) = self.checkpoint(checkpoints, mid) else { break };
            if (st.doc, st.first) < key {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }

        let (mut st, mut pos) = if lo == 0 {
            (DeltaState::default(), entries)
        } else {
            let (st, off) = self.checkpoint(checkpoints, lo)?;
            (st, entries + off)
        };
        let remaining = (n - lo * CHECKPOINT_EVERY).min(CHECKPOINT_EVERY);
        for _ in 0..remaining {
            if pos >= end {
                break;
            }
            let e = decode_entry(self.data, &mut pos, &mut st)?;
            match (e.doc_id, e.first_position).cmp(&key) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => return Some(e),
                std::cmp::Ordering::Greater => return None,
            }
        }
        None
    }

    /// Visit every entry of an ordinal without allocating (the merge path).
    pub fn for_each_entry(&self, ordinal: u32, mut f: impl FnMut(WordPostingEntry)) {
        let Some((start, end)) = self.block_range(ordinal) else { return };
        if self.v3 {
            self.walk_v3(start, end, |e| { f(e); true });
            return;
        }
        for base in (start..end).step_by(ENTRY_SIZE) {
            if base + ENTRY_SIZE > end { break; }
            let b = &self.data[base..base + ENTRY_SIZE];
            f(WordPostingEntry {
                doc_id: u32::from_le_bytes(b[0..4].try_into().unwrap()),
                first_position: u32::from_le_bytes(b[4..8].try_into().unwrap()),
                last_position: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                byte_from: u32::from_le_bytes(b[12..16].try_into().unwrap()),
                byte_to: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            });
        }
    }

    pub fn entries(&self, ordinal: u32) -> Vec<WordPostingEntry> {
        let Some((start, end)) = self.block_range(ordinal) else { return Vec::new() };
        if self.v3 {
            let n = self.block_header(start).map(|(n, _, _)| n).unwrap_or(0);
            let mut out = Vec::with_capacity(n);
            self.walk_v3(start, end, |e| { out.push(e); true });
            return out;
        }
        let num = (end - start) / ENTRY_SIZE;
        let mut result = Vec::with_capacity(num);
        for i in 0..num {
            let base = start + i * ENTRY_SIZE;
            let b = &self.data[base..base + ENTRY_SIZE];
            result.push(WordPostingEntry {
                doc_id: u32::from_le_bytes(b[0..4].try_into().unwrap()),
                first_position: u32::from_le_bytes(b[4..8].try_into().unwrap()),
                last_position: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                byte_from: u32::from_le_bytes(b[12..16].try_into().unwrap()),
                byte_to: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            });
        }
        result
    }
}

// ─── Index registry entry ────────────────────────────────────────────────

/// Prebuilt index entry for the word-level sfxpost.
/// Data is written by the DAG (not EventDriven). This entry only exists
/// so that the segment reader discovers and loads the file.
pub struct WordSfxPostIndex;

impl crate::suffix_fst::index_registry::SfxIndexFile for WordSfxPostIndex {
    fn id(&self) -> &'static str { "word_sfxpost" }
    fn extension(&self) -> &'static str { "word_sfxpost" }
    /// v3 only: word-level partitioning does not exist in v2.
    fn written_for(&self, sfx_version: u8) -> bool { sfx_version >= 3 }
    fn merge_strategy(&self) -> crate::suffix_fst::index_registry::MergeStrategy {
        crate::suffix_fst::index_registry::MergeStrategy::ExternalDagNode
    }
    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc: u32, _ti: u32, _bf: u32, _bt: u32) {}
    fn serialize(&self) -> Vec<u8> { Vec::new() }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let mut writer = WordSfxPostWriter::new(3);
        writer.add(0, WordPostingEntry {
            doc_id: 0, first_position: 0, last_position: 2, byte_from: 0, byte_to: 21,
        });
        writer.add(1, WordPostingEntry {
            doc_id: 0, first_position: 4, last_position: 5, byte_from: 29, byte_to: 43,
        });
        // ordinal 2: no entries

        let data = writer.finish();
        let reader = WordSfxPostReader::open(&data).unwrap();
        assert_eq!(reader.num_ordinals(), 3);

        let e0 = reader.entries(0);
        assert_eq!(e0.len(), 1);
        assert_eq!(e0[0].first_position, 0);
        assert_eq!(e0[0].last_position, 2);
        assert_eq!(e0[0].byte_from, 0);
        assert_eq!(e0[0].byte_to, 21);

        let e1 = reader.entries(1);
        assert_eq!(e1.len(), 1);
        assert_eq!(e1[0].last_position, 5);

        let e2 = reader.entries(2);
        assert!(e2.is_empty());

        // Out of bounds
        assert!(reader.entries(3).is_empty());
    }

    #[test]
    fn test_empty() {
        let writer = WordSfxPostWriter::new(0);
        let data = writer.finish();
        let reader = WordSfxPostReader::open(&data).unwrap();
        assert_eq!(reader.num_ordinals(), 0);
    }

    /// The WSP2 writer, kept in the tests only: WSP3 has to keep reading what
    /// segments written before 25 August 2026 hold.
    fn write_v2(entries_per_ordinal: &[Vec<WordPostingEntry>]) -> Vec<u8> {
        let num_ords = entries_per_ordinal.len() as u32;
        let header_size = 4 + 4 + (num_ords as usize + 1) * 4;
        let mut offsets = Vec::new();
        let mut current = header_size as u32;
        for e in entries_per_ordinal {
            offsets.push(current);
            current += (e.len() * ENTRY_SIZE) as u32;
        }
        offsets.push(current);
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&num_ords.to_le_bytes());
        for off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        for entries in entries_per_ordinal {
            for e in entries {
                buf.extend_from_slice(&e.doc_id.to_le_bytes());
                buf.extend_from_slice(&e.first_position.to_le_bytes());
                buf.extend_from_slice(&e.last_position.to_le_bytes());
                buf.extend_from_slice(&e.byte_from.to_le_bytes());
                buf.extend_from_slice(&e.byte_to.to_le_bytes());
            }
        }
        buf
    }

    /// Entries spread over several documents and well past one checkpoint run,
    /// with a few non-monotone `byte_from` so the wrapping deltas are exercised.
    fn sample(n: usize) -> Vec<WordPostingEntry> {
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let doc = (i / 7) as u32;
            let pos = (i % 7) as u32 * 3;
            let from = if i % 23 == 22 { 5 } else { pos * 11 + 4 };
            v.push(WordPostingEntry {
                doc_id: doc,
                first_position: pos,
                last_position: pos + (i % 3) as u32,
                byte_from: from,
                byte_to: from + 4 + (i % 5) as u32,
            });
        }
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn v3_roundtrip_matches_v2_exactly() {
        for n in [1usize, 2, 31, 32, 33, 64, 65, 200, 1000] {
            let entries = sample(n);
            let mut w = WordSfxPostWriter::new(2);
            for e in &entries {
                w.add(1, e.clone());
            }
            let v3 = w.finish();
            assert_eq!(&v3[0..4], MAGIC_V3, "the writer must emit WSP3");
            let v2 = write_v2(&[Vec::new(), entries.clone()]);

            let r3 = WordSfxPostReader::open(&v3).unwrap();
            let r2 = WordSfxPostReader::open(&v2).unwrap();
            assert_eq!(r3.entries(1), entries, "n={n}: entries() differs from the source");
            assert_eq!(r3.entries(1), r2.entries(1), "n={n}: WSP3 differs from WSP2");
            assert!(r3.entries(0).is_empty(), "n={n}: empty ordinal");

            let mut walked = Vec::new();
            r3.for_each_entry(1, |e| walked.push(e));
            assert_eq!(walked, entries, "n={n}: for_each_entry");

            // Every key found, and only the keys that exist.
            for e in &entries {
                assert_eq!(
                    r3.entry_at(1, e.doc_id, e.first_position),
                    r2.entry_at(1, e.doc_id, e.first_position),
                    "n={n}: entry_at({}, {}) differs from WSP2", e.doc_id, e.first_position
                );
            }
            for (doc, pos) in [(0u32, 1u32), (9999, 0), (0, 9999)] {
                assert_eq!(r3.entry_at(1, doc, pos), r2.entry_at(1, doc, pos),
                    "n={n}: missing key ({doc}, {pos}) must answer like WSP2");
            }
            if n > CHECKPOINT_EVERY {
                assert!(v3.len() < v2.len(), "n={n}: WSP3 {} B is not smaller than WSP2 {} B", v3.len(), v2.len());
            }
        }
    }

    #[test]
    fn v3_reader_still_reads_v2() {
        let entries = sample(100);
        let v2 = write_v2(&[entries.clone()]);
        let r = WordSfxPostReader::open(&v2).unwrap();
        assert_eq!(r.entries(0), entries);
        assert_eq!(r.entry_at(0, entries[50].doc_id, entries[50].first_position), Some(entries[50].clone()));
    }

    #[test]
    fn v3_survives_a_truncated_file() {
        let entries = sample(200);
        let mut w = WordSfxPostWriter::new(1);
        for e in &entries {
            w.add(0, e.clone());
        }
        let data = w.finish();
        for cut in [data.len() / 2, data.len() - 1, data.len() - 7] {
            let r = WordSfxPostReader::open(&data[..cut]);
            if let Some(r) = r {
                // Whatever it returns, it must not panic and must not loop.
                let _ = r.entries(0);
                let _ = r.entry_at(0, 3, 6);
                r.for_each_entry(0, |_| {});
            }
        }
    }

    #[test]
    fn test_multi_doc() {
        let mut writer = WordSfxPostWriter::new(1);
        writer.add(0, WordPostingEntry {
            doc_id: 0, first_position: 0, last_position: 2, byte_from: 0, byte_to: 20,
        });
        writer.add(0, WordPostingEntry {
            doc_id: 1, first_position: 0, last_position: 3, byte_from: 0, byte_to: 25,
        });
        let data = writer.finish();
        let reader = WordSfxPostReader::open(&data).unwrap();
        let entries = reader.entries(0);
        assert_eq!(entries.len(), 2);
    }
}
