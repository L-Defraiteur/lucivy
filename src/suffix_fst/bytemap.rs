//! Byte presence bitmap per ordinal: 256 bits (32 bytes) indicating which
//! byte values appear in the token text.
//!
//! Used as a pre-filter for regex validation: if the regex requires bytes
//! that the token doesn't contain, skip without running the DFA.
//!
//! Format:
//! ```text
//! [4 bytes] magic "BMAP"
//! [4 bytes] num_ordinals: u32 LE
//! [32 bytes × num_ordinals] bitmaps (256 bits each)
//! ```

/// Builds byte presence bitmaps during indexation.
pub struct ByteBitmapWriter {
    bitmaps: Vec<[u8; 32]>,
}

impl ByteBitmapWriter {
    pub fn new() -> Self {
        Self { bitmaps: Vec::new() }
    }

    /// Ensure capacity for at least `num_ordinals` entries.
    pub fn ensure_capacity(&mut self, num_ordinals: u32) {
        let n = num_ordinals as usize;
        if self.bitmaps.len() < n {
            self.bitmaps.resize(n, [0u8; 32]);
        }
    }

    /// Record that `ordinal`'s token contains these bytes.
    pub fn record_token(&mut self, ordinal: u32, text: &[u8]) {
        let o = ordinal as usize;
        if o >= self.bitmaps.len() {
            self.bitmaps.resize(o + 1, [0u8; 32]);
        }
        let bm = &mut self.bitmaps[o];
        for &byte in text {
            bm[byte as usize / 8] |= 1 << (byte % 8);
        }
    }

    /// Copy an existing bitmap for the given ordinal (from a source ByteBitmapReader).
    pub fn copy_bitmap(&mut self, ordinal: u32, bitmap: &[u8; 32]) {
        let o = ordinal as usize;
        if o >= self.bitmaps.len() {
            self.bitmaps.resize(o + 1, [0u8; 32]);
        }
        self.bitmaps[o] = *bitmap;
    }

    /// Serialize to binary format.
    pub fn serialize(&self) -> Vec<u8> {
        let num = self.bitmaps.len() as u32;
        let mut buf = Vec::with_capacity(8 + self.bitmaps.len() * 32);
        buf.extend_from_slice(b"BMAP");
        buf.extend_from_slice(&num.to_le_bytes());
        for bm in &self.bitmaps {
            buf.extend_from_slice(bm);
        }
        buf
    }
}

/// Reads byte presence bitmaps. O(1) lookup per ordinal.
pub struct ByteBitmapReader<'a> {
    num_ordinals: u32,
    data: &'a [u8], // 32 bytes per ordinal
}

impl<'a> ByteBitmapReader<'a> {
    /// Open from raw bytes. Returns None if invalid.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 || &bytes[0..4] != b"BMAP" {
            return None;
        }
        let num_ordinals = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let expected_data = num_ordinals as usize * 32;
        if bytes.len() < 8 + expected_data {
            return None;
        }
        Some(Self {
            num_ordinals,
            data: &bytes[8..8 + expected_data],
        })
    }

    /// Get the 256-bit bitmap for an ordinal.
    pub fn bitmap(&self, ordinal: u32) -> Option<&[u8; 32]> {
        if ordinal >= self.num_ordinals {
            return None;
        }
        let off = ordinal as usize * 32;
        Some(self.data[off..off + 32].try_into().unwrap())
    }

    /// Check if the token at `ordinal` contains byte `b`.
    pub fn contains_byte(&self, ordinal: u32, b: u8) -> bool {
        match self.bitmap(ordinal) {
            Some(bm) => bm[b as usize / 8] & (1 << (b % 8)) != 0,
            None => false,
        }
    }

    /// Check if ALL bytes in the token at `ordinal` fall within [lo, hi] (inclusive).
    /// Useful for `[a-z]+` checks: all_bytes_in_range(ord, b'a', b'z').
    pub fn all_bytes_in_range(&self, ordinal: u32, lo: u8, hi: u8) -> bool {
        let bm = match self.bitmap(ordinal) {
            Some(bm) => bm,
            None => return false,
        };
        for i in 0..32 {
            let mut mask = bm[i];
            while mask != 0 {
                let bit = mask.trailing_zeros() as u8;
                let byte_val = (i as u8) * 8 + bit;
                if byte_val < lo || byte_val > hi {
                    return false;
                }
                mask &= mask - 1;
            }
        }
        true
    }

    /// Check if the token at `ordinal` contains ALL the specified bytes.
    /// Useful for literal substring checks.
    pub fn contains_all_bytes(&self, ordinal: u32, required: &[u8]) -> bool {
        let bm = match self.bitmap(ordinal) {
            Some(bm) => bm,
            None => return false,
        };
        for &b in required {
            if bm[b as usize / 8] & (1 << (b % 8)) == 0 {
                return false;
            }
        }
        true
    }

    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytemap_roundtrip() {
        let mut writer = ByteBitmapWriter::new();
        writer.record_token(0, b"hello");
        writer.record_token(1, b"rag3db");
        writer.record_token(2, b"UPPER");

        let data = writer.serialize();
        let reader = ByteBitmapReader::open(&data).unwrap();

        assert!(reader.contains_byte(0, b'h'));
        assert!(reader.contains_byte(0, b'e'));
        assert!(reader.contains_byte(0, b'l'));
        assert!(reader.contains_byte(0, b'o'));
        assert!(!reader.contains_byte(0, b'x'));

        // "rag3db" contains 'r','a','g','3','d','b'
        assert!(reader.contains_byte(1, b'3'));
        assert!(!reader.all_bytes_in_range(1, b'a', b'z')); // has '3'

        // "hello" is all lowercase
        assert!(reader.all_bytes_in_range(0, b'a', b'z'));

        // contains_all_bytes
        assert!(reader.contains_all_bytes(1, b"rag"));
        assert!(!reader.contains_all_bytes(1, b"xyz"));
    }

    #[test]
    fn test_bytemap_empty() {
        let writer = ByteBitmapWriter::new();
        let data = writer.serialize();
        let reader = ByteBitmapReader::open(&data).unwrap();
        assert_eq!(reader.num_ordinals(), 0);
        assert!(reader.bitmap(0).is_none());
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

pub struct ByteMapIndex {
    writer: ByteBitmapWriter,
}

impl ByteMapIndex {
    pub fn new() -> Self { Self { writer: ByteBitmapWriter::new() } }
}

impl super::index_registry::SfxIndexFile for ByteMapIndex {
    fn id(&self) -> &'static str { "bytemap" }
    fn extension(&self) -> &'static str { "bytemap" }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy { super::index_registry::MergeStrategy::EventDriven }

    fn on_token(&mut self, ord: u32, text: &str) {
        self.writer.ensure_capacity(ord + 1);
        self.writer.record_token(ord, text.as_bytes());
    }

    fn serialize(&self) -> Vec<u8> { self.writer.serialize() }
}
