//! Word-level sfxpost for partition 0x02 (word-stripped entries).
//!
//! Separate from the chunk-level SfxPostV2 because word postings have
//! different semantics: position = last chunk of the word (for cross-word
//! chain adjacency), byte_from = first chunk start (for highlights).
//!
//! Format:
//! ```text
//! [4 bytes] magic "WSP1"
//! [4 bytes] num_ordinals (u32 LE)
//! [num_ordinals * 4 bytes] offset table (u32 LE per ordinal → byte offset into entries)
//! [4 bytes] sentinel offset (= end of entries)
//! [entries...] packed WordPostingEntry (20 bytes each)
//! ```

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
    /// End byte offset in the original text (last chunk end).
    pub byte_to: u32,
}

const ENTRY_SIZE: usize = 20; // 5 × u32
const MAGIC: &[u8; 4] = b"WSP1";

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

        // Compute offsets
        let header_size = 4 + 4 + (num_ords as usize + 1) * 4; // magic + num_ords + offset table + sentinel
        let mut offsets: Vec<u32> = Vec::with_capacity(num_ords as usize + 1);
        let mut current = header_size as u32;
        for entries in &self.entries {
            offsets.push(current);
            current += (entries.len() * ENTRY_SIZE) as u32;
        }
        offsets.push(current); // sentinel

        let mut buf = Vec::with_capacity(current as usize);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&num_ords.to_le_bytes());
        for &off in &offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        for entries in &self.entries {
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
}

// ─── Reader ──────────────────────────────────────────────────────────────

pub struct WordSfxPostReader<'a> {
    data: &'a [u8],
    num_ordinals: u32,
}

impl<'a> WordSfxPostReader<'a> {
    pub fn open(data: &'a [u8]) -> Option<Self> {
        if data.len() < 8 { return None; }
        if &data[0..4] != MAGIC { return None; }
        let num_ordinals = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let min_size = 8 + (num_ordinals as usize + 1) * 4;
        if data.len() < min_size { return None; }
        Some(Self { data, num_ordinals })
    }

    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }

    pub fn entries(&self, ordinal: u32) -> Vec<WordPostingEntry> {
        if ordinal >= self.num_ordinals {
            return Vec::new();
        }
        let off_base = 8 + ordinal as usize * 4;
        let start = u32::from_le_bytes(self.data[off_base..off_base + 4].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(self.data[off_base + 4..off_base + 8].try_into().unwrap()) as usize;
        if start >= end || end > self.data.len() {
            return Vec::new();
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
