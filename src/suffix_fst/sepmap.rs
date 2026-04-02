//! Separator byte bitmap per ordinal: 256 bits (32 bytes) indicating which
//! byte values appear in separators AFTER this token (across all documents).
//!
//! Bit 0x00 is reserved as "contiguous flag" — set when the token has been
//! observed immediately followed by another token with no separator (gap=0).
//!
//! Used to quickly check if a regex gap pattern like `[a-z]+` can cross a
//! token boundary without reading the per-doc GapMap.
//!
//! Format: identical to ByteMap.
//! ```text
//! [4 bytes] magic "SMAP"
//! [4 bytes] num_ordinals: u32 LE
//! [32 bytes × num_ordinals] bitmaps (256 bits each)
//! ```

/// Byte value used as "contiguous flag" in the bitmap.
/// Set when a token has been observed with gap=0 (no separator) after it.
pub const CONTIGUOUS_FLAG: u8 = 0x00;

/// Builds separator byte bitmaps during indexation.
pub struct SepMapWriter {
    bitmaps: Vec<[u8; 32]>,
}

impl SepMapWriter {
    pub fn new() -> Self {
        Self { bitmaps: Vec::new() }
    }

    pub fn ensure_capacity(&mut self, num_ordinals: u32) {
        let n = num_ordinals as usize;
        if self.bitmaps.len() < n {
            self.bitmaps.resize(n, [0u8; 32]);
        }
    }

    /// Record that a separator byte was observed after `ordinal`.
    pub fn record_byte(&mut self, ordinal: u32, byte: u8) {
        let o = ordinal as usize;
        if o >= self.bitmaps.len() {
            self.bitmaps.resize(o + 1, [0u8; 32]);
        }
        self.bitmaps[o][byte as usize / 8] |= 1 << (byte % 8);
    }

    /// Record a contiguous transition (gap=0) after `ordinal`.
    pub fn record_contiguous(&mut self, ordinal: u32) {
        self.record_byte(ordinal, CONTIGUOUS_FLAG);
    }

    /// Record all separator bytes between `ordinal` and the next token.
    /// If gap_bytes is empty, records contiguous flag.
    pub fn record_gap(&mut self, ordinal: u32, gap_bytes: &[u8]) {
        if gap_bytes.is_empty() {
            self.record_contiguous(ordinal);
        } else {
            for &byte in gap_bytes {
                self.record_byte(ordinal, byte);
            }
        }
    }

    /// OR-merge an existing bitmap into this writer (for merge path).
    pub fn or_bitmap(&mut self, ordinal: u32, bitmap: &[u8; 32]) {
        let o = ordinal as usize;
        if o >= self.bitmaps.len() {
            self.bitmaps.resize(o + 1, [0u8; 32]);
        }
        for i in 0..32 {
            self.bitmaps[o][i] |= bitmap[i];
        }
    }

    /// Access the raw bitmaps (for remapping ordinals).
    pub fn bitmaps_ref(&self) -> &[[u8; 32]] {
        &self.bitmaps
    }

    pub fn serialize(&self) -> Vec<u8> {
        let num = self.bitmaps.len() as u32;
        let mut buf = Vec::with_capacity(8 + self.bitmaps.len() * 32);
        buf.extend_from_slice(b"SMAP");
        buf.extend_from_slice(&num.to_le_bytes());
        for bm in &self.bitmaps {
            buf.extend_from_slice(bm);
        }
        buf
    }
}

/// Reads separator byte bitmaps. O(1) lookup per ordinal.
pub struct SepMapReader<'a> {
    num_ordinals: u32,
    data: &'a [u8],
}

impl<'a> SepMapReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 || &bytes[0..4] != b"SMAP" {
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

    pub fn bitmap(&self, ordinal: u32) -> Option<&[u8; 32]> {
        if ordinal >= self.num_ordinals {
            return None;
        }
        let off = ordinal as usize * 32;
        Some(self.data[off..off + 32].try_into().unwrap())
    }

    /// Check if ordinal has ever been observed with a contiguous successor (gap=0).
    pub fn has_contiguous(&self, ordinal: u32) -> bool {
        match self.bitmap(ordinal) {
            Some(bm) => bm[0] & 1 != 0, // bit 0x00
            None => false,
        }
    }

    /// Check if ALL separator bytes observed after `ordinal` fall within the ranges.
    /// Returns true if:
    /// - Only contiguous (gap=0) observed, OR
    /// - All non-contiguous separator bytes are in the ranges.
    /// Returns false if any separator byte is outside the ranges.
    pub fn sep_bytes_in_ranges(&self, ordinal: u32, ranges: &[(u8, u8)]) -> bool {
        let Some(bm) = self.bitmap(ordinal) else { return true; };
        for chunk_idx in 0..32 {
            let chunk = bm[chunk_idx];
            if chunk == 0 { continue; }
            let mut bits = chunk;
            while bits != 0 {
                let bit_pos = bits.trailing_zeros() as u8;
                let byte_val = (chunk_idx as u8) * 8 + bit_pos;
                // Skip contiguous flag
                if byte_val == CONTIGUOUS_FLAG {
                    bits &= bits - 1;
                    continue;
                }
                let in_range = ranges.iter().any(|&(lo, hi)| byte_val >= lo && byte_val <= hi);
                if !in_range { return false; }
                bits &= bits - 1;
            }
        }
        true
    }

    /// Check if ordinal has ONLY contiguous transitions (no separator bytes at all).
    pub fn only_contiguous(&self, ordinal: u32) -> bool {
        let Some(bm) = self.bitmap(ordinal) else { return false; };
        // Only bit 0 should be set
        if bm[0] & 1 == 0 { return false; } // no contiguous flag
        // Check all other bits are 0
        for i in 1..32 {
            if bm[i] != 0 { return false; }
        }
        // Check bits 1-7 of byte 0
        bm[0] & 0xFE == 0
    }

    pub fn num_ordinals(&self) -> u32 {
        self.num_ordinals
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

pub struct SepMapIndex {
    data: Vec<u8>,
}

impl SepMapIndex {
    pub fn new() -> Self { Self { data: Vec::new() } }
}

impl super::index_registry::SfxIndexFile for SepMapIndex {
    fn id(&self) -> &'static str { "sepmap" }
    fn extension(&self) -> &'static str { "sepmap" }
    fn merge_strategy(&self) -> super::index_registry::MergeStrategy {
        super::index_registry::MergeStrategy::OrMergeWithRemap
    }
    fn prebuilt_by_collector(&self) -> bool { true }

    fn merge_from_sources(
        &mut self,
        sources: &[Option<&[u8]>],
        source_termtexts: &[Option<&[u8]>],
        token_to_new_ord: &dyn Fn(&str) -> Option<u32>,
    ) {
        use super::TermTextsReader;

        let readers: Vec<Option<SepMapReader>> = sources.iter()
            .map(|opt| opt.and_then(SepMapReader::open))
            .collect();

        if !readers.iter().any(|r| r.is_some()) {
            return;
        }

        let mut writer = SepMapWriter::new();

        for (seg_idx, reader_opt) in readers.iter().enumerate() {
            let reader = match reader_opt { Some(r) => r, None => continue };
            let tt = match source_termtexts[seg_idx].and_then(|b| TermTextsReader::open(b)) {
                Some(t) => t, None => continue,
            };

            for old_ord in 0..tt.num_terms() {
                let text = match tt.text(old_ord) { Some(t) => t, None => continue };
                let new_ord = match token_to_new_ord(text) { Some(o) => o, None => continue };

                if let Some(bitmap) = reader.bitmap(old_ord) {
                    writer.ensure_capacity(new_ord + 1);
                    writer.or_bitmap(new_ord, bitmap);
                }
            }
        }

        self.data = writer.serialize();
    }

    fn serialize(&self) -> Vec<u8> { self.data.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sepmap_roundtrip() {
        let mut writer = SepMapWriter::new();
        writer.ensure_capacity(3);
        writer.record_gap(0, b" ");         // space after token 0
        writer.record_gap(1, b"");          // contiguous after token 1
        writer.record_gap(2, b"\n  ");      // newline + spaces after token 2

        let data = writer.serialize();
        let reader = SepMapReader::open(&data).unwrap();

        // Token 0: space separator
        assert!(!reader.has_contiguous(0));
        assert!(reader.sep_bytes_in_ranges(0, &[(b' ', b' ')])); // space in range
        assert!(!reader.sep_bytes_in_ranges(0, &[(b'a', b'z')])); // space not in [a-z]

        // Token 1: contiguous only
        assert!(reader.has_contiguous(1));
        assert!(reader.only_contiguous(1));
        assert!(reader.sep_bytes_in_ranges(1, &[(b'a', b'z')])); // contiguous → OK for any range

        // Token 2: newline + spaces
        assert!(!reader.has_contiguous(2));
        assert!(!reader.sep_bytes_in_ranges(2, &[(b'a', b'z')])); // newline/space not in [a-z]
    }

    #[test]
    fn test_sepmap_or_merge() {
        let mut w1 = SepMapWriter::new();
        w1.record_gap(0, b" ");
        let mut w2 = SepMapWriter::new();
        w2.record_gap(0, b"\n");

        let d1 = w1.serialize();
        let d2 = w2.serialize();
        let r1 = SepMapReader::open(&d1).unwrap();
        let r2 = SepMapReader::open(&d2).unwrap();

        // Merge via OR
        let mut merged = SepMapWriter::new();
        merged.ensure_capacity(1);
        merged.or_bitmap(0, r1.bitmap(0).unwrap());
        merged.or_bitmap(0, r2.bitmap(0).unwrap());
        let data = merged.serialize();
        let reader = SepMapReader::open(&data).unwrap();

        // Should have both space and newline
        assert!(reader.bitmap(0).unwrap()[b' ' as usize / 8] & (1 << (b' ' % 8)) != 0);
        assert!(reader.bitmap(0).unwrap()[b'\n' as usize / 8] & (1 << (b'\n' % 8)) != 0);
    }
}
