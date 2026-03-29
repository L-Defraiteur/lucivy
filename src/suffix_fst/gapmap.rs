use std::io::{self, Write};

// GapMap binary format v2 (multi-value aware):
//
// HEADER:
//   num_docs: u32 LE
//   offsets: [u64 LE × (num_docs + 1)]
//
// DATA (per doc):
//   num_tokens: u16 LE       // total tokens across all values
//   num_values: u8            // 1 = single-value (fast path), >1 = multi
//
//   if num_values > 1:
//     value_offsets: [(seq_start: u16, ti_start: u32) × num_values]
//
//   gaps: encoded sequentially
//     For single-value: num_tokens + 1 gaps (prefix, seps, suffix)
//     For multi-value: per value (prefix, seps, suffix), separated by
//       VALUE_BOUNDARY markers between values
//
// Gap encoding:
//   len = 0..253   : normal gap of len bytes
//   len = 254      : VALUE_BOUNDARY marker (no bytes follow)
//   len = 255      : extended length → [ext_len: u16 LE][bytes...]

const HEADER_SIZE_BASE: usize = 4; // num_docs: u32
const VALUE_BOUNDARY_MARKER: u8 = 254;

/// Sentinel value returned by read_gap when a VALUE_BOUNDARY is encountered.
pub const VALUE_BOUNDARY: &[u8] = &[VALUE_BOUNDARY_MARKER];

/// Gap data for a single value within a document.
pub struct ValueGaps<'a> {
    /// Gaps for this value: prefix + separators + suffix = num_tokens_in_value + 1
    pub gaps: Vec<&'a [u8]>,
}

/// Writes gap data for multiple documents into a binary format.
/// Supports multi-value fields with VALUE_BOUNDARY markers.
pub struct GapMapWriter {
    data_buffer: Vec<u8>,
    /// Per-document byte offsets into `data_buffer`.
    doc_offsets: Vec<u64>,
}

impl GapMapWriter {
    /// Create a new empty GapMap writer.
    pub fn new() -> Self {
        Self {
            data_buffer: Vec::new(),
            doc_offsets: Vec::new(),
        }
    }

    /// Add gap data for a single-value document (fast path).
    /// `gaps` contains num_tokens + 1 entries.
    pub fn add_doc(&mut self, gaps: &[&[u8]]) {
        self.doc_offsets.push(self.data_buffer.len() as u64);

        let num_tokens = if gaps.is_empty() { 0u16 } else { (gaps.len() - 1) as u16 };
        self.data_buffer.extend_from_slice(&num_tokens.to_le_bytes());
        self.data_buffer.push(1u8); // num_values = 1

        for gap in gaps {
            encode_gap(&mut self.data_buffer, gap);
        }
    }

    /// Add gap data for a multi-value document.
    /// `values_gaps` is a vec of gap arrays, one per value.
    /// `ti_starts` is the posting Ti of the first token of each value.
    ///
    /// Each value's gaps = [prefix, sep_0, ..., suffix] = num_tokens_in_value + 1 entries.
    pub fn add_doc_multi(&mut self, values_gaps: &[Vec<&[u8]>], ti_starts: &[u32]) {
        self.doc_offsets.push(self.data_buffer.len() as u64);

        let num_values = values_gaps.len() as u8;
        let num_tokens: u16 = values_gaps
            .iter()
            .map(|vg| if vg.is_empty() { 0u16 } else { (vg.len() - 1) as u16 })
            .sum();

        // Header
        self.data_buffer.extend_from_slice(&num_tokens.to_le_bytes());
        self.data_buffer.push(num_values);

        // Value offsets table (only for multi-value)
        if num_values > 1 {
            let mut seq_start: u16 = 0;
            for (i, vg) in values_gaps.iter().enumerate() {
                self.data_buffer.extend_from_slice(&seq_start.to_le_bytes());
                self.data_buffer.extend_from_slice(&ti_starts[i].to_le_bytes());
                let tokens_in_value = if vg.is_empty() { 0u16 } else { (vg.len() - 1) as u16 };
                seq_start += tokens_in_value;
            }
        }

        // Gaps with VALUE_BOUNDARY between values
        for (i, vg) in values_gaps.iter().enumerate() {
            if i > 0 {
                self.data_buffer.push(VALUE_BOUNDARY_MARKER);
            }
            for gap in vg {
                encode_gap(&mut self.data_buffer, gap);
            }
        }
    }

    /// Add an empty document (no tokens, no gaps).
    pub fn add_empty_doc(&mut self) {
        self.doc_offsets.push(self.data_buffer.len() as u64);
        self.data_buffer.extend_from_slice(&0u16.to_le_bytes()); // num_tokens = 0
        self.data_buffer.push(1u8); // num_values = 1
    }

    /// Add raw doc bytes directly (for merge — copies the exact doc_data from a source GapMap).
    pub fn add_doc_raw(&mut self, raw_doc_data: &[u8]) {
        self.doc_offsets.push(self.data_buffer.len() as u64);
        self.data_buffer.extend_from_slice(raw_doc_data);
    }

    /// Number of documents added so far.
    pub fn num_docs(&self) -> u32 {
        self.doc_offsets.len() as u32
    }

    /// Serialize the complete GapMap into bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let num_docs = self.doc_offsets.len() as u32;
        let offset_table_size = (num_docs as usize + 1) * 8;
        let data_start = (HEADER_SIZE_BASE + offset_table_size) as u64;

        let total_size = HEADER_SIZE_BASE + offset_table_size + self.data_buffer.len();
        let mut out = Vec::with_capacity(total_size);

        // Header: num_docs
        out.extend_from_slice(&num_docs.to_le_bytes());

        // Offset table (absolute offsets)
        for &offset in &self.doc_offsets {
            out.extend_from_slice(&(data_start + offset).to_le_bytes());
        }
        // Sentinel
        out.extend_from_slice(&(data_start + self.data_buffer.len() as u64).to_le_bytes());

        // Data
        out.extend_from_slice(&self.data_buffer);

        out
    }

    /// Write the serialized GapMap to a writer.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let bytes = self.serialize();
        writer.write_all(&bytes)
    }
}

fn encode_gap(buf: &mut Vec<u8>, gap: &[u8]) {
    if gap.len() < 254 {
        buf.push(gap.len() as u8);
        buf.extend_from_slice(gap);
    } else if gap.len() < 255 {
        // len=254 is reserved for VALUE_BOUNDARY, use extended for 254-byte gaps
        buf.push(255);
        buf.extend_from_slice(&(gap.len() as u16).to_le_bytes());
        buf.extend_from_slice(gap);
    } else {
        buf.push(255);
        buf.extend_from_slice(&(gap.len() as u16).to_le_bytes());
        buf.extend_from_slice(gap);
    }
}

/// Error found during gapmap validation.
#[derive(Debug)]
pub struct GapMapError {
    /// ID of the document that failed validation.
    pub doc_id: u32,
    /// Description of the validation failure.
    pub message: String,
}

impl std::fmt::Display for GapMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gapmap doc_{}: {}", self.doc_id, self.message)
    }
}

/// Reads gap data from a mmap'd or in-memory GapMap.
pub struct GapMapReader<'a> {
    data: &'a [u8],
    num_docs: u32,
}

impl<'a> GapMapReader<'a> {
    /// Open a GapMap from raw bytes (mmap'd or in-memory).
    pub fn open(data: &'a [u8]) -> Self {
        let num_docs = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        Self { data, num_docs }
    }

    /// Raw bytes of the entire gapmap section (for rebuilding .sfx files).
    pub fn raw_data(&self) -> &'a [u8] {
        self.data
    }

    /// Number of documents in this GapMap.
    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Get the raw bytes for a document's gap data.
    pub fn doc_data(&self, doc_id: u32) -> &'a [u8] {
        let offset_pos = HEADER_SIZE_BASE + (doc_id as usize) * 8;
        let start = u64::from_le_bytes(
            self.data[offset_pos..offset_pos + 8].try_into().unwrap(),
        ) as usize;
        let end = u64::from_le_bytes(
            self.data[offset_pos + 8..offset_pos + 16].try_into().unwrap(),
        ) as usize;
        &self.data[start..end]
    }

    /// Number of tokens in a document (total across all values).
    pub fn num_tokens(&self, doc_id: u32) -> u16 {
        let dd = self.doc_data(doc_id);
        u16::from_le_bytes([dd[0], dd[1]])
    }

    /// Number of values for this document's field.
    pub fn num_values(&self, doc_id: u32) -> u8 {
        let dd = self.doc_data(doc_id);
        dd[2]
    }

    /// Read the gap at sequential `gap_index` for document `doc_id`.
    /// For single-value: gap_index = Ti + 1 for separator after token Ti.
    /// Returns VALUE_BOUNDARY sentinel if this gap is a value boundary.
    pub fn read_gap(&self, doc_id: u32, gap_index: u32) -> &'a [u8] {
        let dd = self.doc_data(doc_id);
        let num_values = dd[2];
        let mut cursor = 3; // skip num_tokens(2) + num_values(1)

        // Skip value_offsets table for multi-value
        if num_values > 1 {
            cursor += num_values as usize * 6; // (u16 + u32) per value
        }

        // Skip to the target gap (counting VALUE_BOUNDARY markers too)
        let mut gap_count = 0;
        while gap_count < gap_index {
            let (_, next) = decode_gap_at(dd, cursor);
            cursor = next;
            gap_count += 1;
        }

        let (gap, _) = decode_gap_at(dd, cursor);
        gap
    }

    /// Read the gap between two consecutive posting positions Ti_a and Ti_b.
    /// Returns None if:
    /// - Ti_b != Ti_a + 1 (not consecutive)
    /// - The gap is a VALUE_BOUNDARY (cross-value match)
    /// Returns Some(gap_bytes) for a valid separator.
    pub fn read_separator(&self, doc_id: u32, ti_a: u32, ti_b: u32) -> Option<&'a [u8]> {
        if ti_b != ti_a + 1 {
            return None;
        }

        let dd = self.doc_data(doc_id);
        let num_values = dd[2];

        if num_values == 1 {
            // Fast path: the separator between token ti_a and ti_b
            // is at gap_index = ti_a + 1 (gap after token ti_a)
            let gap = self.read_gap_from_data(dd, ti_a + 1);
            if is_value_boundary(gap) {
                return None;
            }
            Some(gap)
        } else {
            // Multi-value: gap after token ti_a
            let gap_index = self.ti_to_gap_index_after(dd, ti_a);
            let gap = self.read_gap_from_data(dd, gap_index);
            if is_value_boundary(gap) {
                return None;
            }
            Some(gap)
        }
    }

    /// Convert a posting Ti to the gap index in the serialized gap stream.
    ///
    /// For single-value: gap_index = Ti (direct).
    /// For multi-value: accounts for per-value prefix/suffix gaps and boundary markers.
    ///
    /// The serialized gap layout for multi-value is:
    ///   [prefix_v0] [seps_v0...] [suffix_v0] [BOUNDARY] [prefix_v1] [seps_v1...] [suffix_v1] ...
    ///
    /// For a token at position K within value V (K=0 is first token of that value),
    /// the separator AFTER it is at:
    ///   gap_index = sum_of_gaps_before_value_V + K + 1
    /// where sum_of_gaps_before_value_V = sum over values 0..V of (tokens_in_v + 1) + (V boundaries)
    fn ti_to_gap_index_after(&self, dd: &'a [u8], ti: u32) -> u32 {
        let num_values = dd[2] as usize;
        if num_values <= 1 {
            return ti;
        }

        let table_start = 3;

        // Find which value this Ti belongs to, and compute gap offset
        let mut best_value = 0;
        for v in 0..num_values {
            let offset = table_start + v * 6;
            let ti_start = u32::from_le_bytes([
                dd[offset + 2], dd[offset + 3], dd[offset + 4], dd[offset + 5],
            ]);
            if ti_start <= ti {
                best_value = v;
            } else {
                break;
            }
        }

        // Read this value's seq_start and ti_start
        let offset = table_start + best_value * 6;
        let seq_start = u16::from_le_bytes([dd[offset], dd[offset + 1]]) as u32;
        let ti_start = u32::from_le_bytes([
            dd[offset + 2], dd[offset + 3], dd[offset + 4], dd[offset + 5],
        ]);

        // Position of this token within its value (0-based)
        let pos_within_value = ti - ti_start;

        // Gap offset: each value V before us contributes (tokens_in_V + 1) gaps.
        // Plus V boundary markers for values 0..V-1.
        // The seq_start gives us the cumulative token count before this value.
        // Gaps before this value = seq_start (tokens) + best_value (one prefix per value) + best_value (boundaries)
        // Actually: for value 0: prefix + seps + suffix = tokens_v0 + 1 gaps
        //           boundary after value 0 = 1
        //           for value 1: prefix + seps + suffix = tokens_v1 + 1 gaps
        //           etc.
        // Total gaps before value V = sum(tokens_vi + 1 for i<V) + V boundaries
        //                           = seq_start + V + V = seq_start + 2*V
        // Wait: seq_start = cumulative tokens before value V.
        // Gaps before value V = sum of (tokens_vi + 1) for i < V = seq_start + V (one extra gap per value for prefix/suffix combined)
        // Plus (V) boundary markers.
        // Hmm, let me think again.
        //
        // Value 0 with 2 tokens: gaps = [prefix, sep, suffix] = 3 gaps = tokens+1
        // BOUNDARY marker = 1
        // Value 1 with 2 tokens: gaps = [prefix, sep, suffix] = 3 gaps
        //
        // Total before value 1: 3 gaps + 1 boundary = 4
        // Token 0 of value 1: gap_after = gap_index = 4 + 0 + 1 = 5
        //   (gap[4] = prefix of value 1, gap[5] = sep between token 0 and 1 of value 1)
        //
        // So: gaps_before_value_V = sum(tokens_vi + 1 for i<V) + V (boundaries)
        //                         = (seq_start + best_value) + best_value
        //                         = seq_start + 2 * best_value
        // NO wait. seq_start = sum(tokens_vi for i<V). The +1 per value is for the
        // (tokens_vi + 1) gap count. So sum(tokens_vi + 1 for i<V) = seq_start + V.
        // Plus V boundaries = seq_start + V + V = seq_start + 2*V? No, only V-0 = V
        // boundaries for V values before (boundaries are BETWEEN values, so V-1... no,
        // boundary after value 0, after value 1, etc. = best_value boundaries before value V).
        // Actually: boundary markers separate values. Between value 0 and 1 = 1 boundary.
        // Before value V = V boundaries.
        // Hmm no. Between value 0 and 1: 1 boundary. Between 1 and 2: 1 boundary.
        // Before value V: V boundaries (one before each of values 1..V).
        // Wait: boundaries are emitted BETWEEN values. So before value 0: 0 boundaries.
        // Before value 1: 1 boundary (after value 0). Before value 2: 2 boundaries.
        // So before value V: V boundaries. No — it's best_value boundaries.
        //
        // gaps_before_this_value = (seq_start + best_value) + best_value
        //                        = seq_start + 2 * best_value
        // Hmm that doesn't seem right. Let me trace:
        //
        // Value 0: 2 tokens → 3 gaps (indices 0,1,2)
        // Boundary: 1 (index 3)
        // Value 1: 2 tokens → 3 gaps (indices 4,5,6)
        //
        // seq_start for value 1 = 2 (2 tokens in value 0)
        // best_value = 1
        // gaps_before = seq_start + best_value + best_value = 2 + 1 + 1 = 4 ✓ (index 4 = prefix of value 1)
        //
        // For token 0 of value 1 (pos_within_value=0):
        //   gap_after = gaps_before + pos_within_value + 1 = 4 + 0 + 1 = 5 ✓ (sep between token 0 and 1)
        //
        // For token 1 of value 1 (pos_within_value=1):
        //   gap_after = 4 + 1 + 1 = 6 ✓ (suffix of value 1)
        //
        // Let me verify with 3 values:
        // Value 0: 2 tokens → gaps 0,1,2 (3 gaps)
        // Boundary at 3
        // Value 1: 3 tokens → gaps 4,5,6,7 (4 gaps)
        // Boundary at 8
        // Value 2: 1 token → gaps 9,10 (2 gaps)
        //
        // seq_start for value 2 = 2+3 = 5
        // best_value = 2
        // gaps_before = 5 + 2 + 2 = 9 ✓ (index 9 = prefix of value 2)
        // token 0 of value 2: gap_after = 9 + 0 + 1 = 10 ✓ (suffix of value 2)

        let gaps_before_this_value = seq_start + 2 * best_value as u32;

        // Gap index for the separator AFTER this token
        gaps_before_this_value + pos_within_value + 1
    }

    /// Internal: read gap at sequential index from doc data, accounting for header.
    fn read_gap_from_data(&self, dd: &'a [u8], gap_index: u32) -> &'a [u8] {
        let num_values = dd[2];
        let mut cursor = 3;

        if num_values > 1 {
            cursor += num_values as usize * 6;
        }

        let mut count = 0;
        while count < gap_index {
            let (_, next) = decode_gap_at(dd, cursor);
            cursor = next;
            count += 1;
        }

        let (gap, _) = decode_gap_at(dd, cursor);
        gap
    }

    /// Validate all documents in the gapmap. Returns errors for corrupt docs.
    ///
    /// Call after merge to detect corruption before search hits it.
    pub fn validate(&self) -> Vec<GapMapError> {
        let mut errors = Vec::new();
        for doc_id in 0..self.num_docs {
            if let Err(e) = self.validate_doc(doc_id) {
                errors.push(e);
            }
        }
        errors
    }

    /// Validate a single document's gap data.
    pub fn validate_doc(&self, doc_id: u32) -> Result<(), GapMapError> {
        let dd = self.doc_data(doc_id);
        if dd.len() < 3 {
            return Err(GapMapError {
                doc_id,
                message: format!("doc_data too short: {} bytes (min 3)", dd.len()),
            });
        }

        let num_tokens = u16::from_le_bytes([dd[0], dd[1]]) as usize;
        let num_values = dd[2] as usize;

        if num_tokens == 0 {
            return Ok(()); // empty doc is valid
        }

        if num_values == 0 {
            return Err(GapMapError {
                doc_id,
                message: format!("num_values=0 but num_tokens={}", num_tokens),
            });
        }

        let mut cursor = 3;
        if num_values > 1 {
            let table_size = num_values * 6;
            if cursor + table_size > dd.len() {
                return Err(GapMapError {
                    doc_id,
                    message: format!(
                        "value_offsets table overflows: cursor={}, table_size={}, data_len={}",
                        cursor, table_size, dd.len(),
                    ),
                });
            }
            cursor += table_size;
        }

        // Count gaps by reading until end of data
        let mut gap_count = 0;
        while cursor < dd.len() {
            if cursor >= dd.len() {
                return Err(GapMapError {
                    doc_id,
                    message: format!(
                        "gap decode overflows: cursor={}, data_len={}, gaps_so_far={}",
                        cursor, dd.len(), gap_count,
                    ),
                });
            }
            let len_byte = dd[cursor] as usize;
            if len_byte == VALUE_BOUNDARY_MARKER as usize {
                cursor += 1;
            } else if len_byte == 255 {
                if cursor + 3 > dd.len() {
                    return Err(GapMapError {
                        doc_id,
                        message: format!(
                            "extended gap header overflows at cursor={}, data_len={}",
                            cursor, dd.len(),
                        ),
                    });
                }
                let ext_len = u16::from_le_bytes([dd[cursor + 1], dd[cursor + 2]]) as usize;
                cursor += 3 + ext_len;
            } else {
                cursor += 1 + len_byte;
            }
            gap_count += 1;
        }

        // Expected: for N tokens and V values, total gaps = N + V - 1
        // (one gap between each consecutive token, plus VALUE_BOUNDARY between values)
        let _expected_gaps = if num_values == 1 {
            num_tokens + 1 // gaps around each token
        } else {
            num_tokens + num_values // tokens gaps + value boundaries
        };

        if gap_count < num_tokens {
            return Err(GapMapError {
                doc_id,
                message: format!(
                    "too few gaps: found {}, tokens={}, values={}, expected>={}",
                    gap_count, num_tokens, num_values, num_tokens,
                ),
            });
        }

        Ok(())
    }

    /// Read all gaps for a document, including VALUE_BOUNDARY markers.
    pub fn read_all_gaps(&self, doc_id: u32) -> Vec<&'a [u8]> {
        let dd = self.doc_data(doc_id);
        let num_tokens = u16::from_le_bytes([dd[0], dd[1]]) as usize;
        let num_values = dd[2] as usize;

        if num_tokens == 0 {
            return Vec::new();
        }

        let mut cursor = 3;
        if num_values > 1 {
            cursor += num_values * 6;
        }

        // Total gaps = sum of (tokens_per_value + 1) + (num_values - 1) boundaries
        // = num_tokens + num_values + (num_values - 1) = num_tokens + 2*num_values - 1
        // But simpler: just read until we've consumed all data
        let mut gaps = Vec::new();
        let dd_len = dd.len();
        while cursor < dd_len {
            let (gap, next) = decode_gap_at(dd, cursor);
            gaps.push(gap);
            cursor = next;
        }

        gaps
    }
}

/// Decode a gap at position `cursor` in the data. Returns (gap_bytes, next_cursor).
/// VALUE_BOUNDARY (len=254) returns the sentinel VALUE_BOUNDARY slice.
fn decode_gap_at<'a>(data: &'a [u8], cursor: usize) -> (&'a [u8], usize) {
    let len = data[cursor] as usize;
    if len == VALUE_BOUNDARY_MARKER as usize {
        // VALUE_BOUNDARY marker — no bytes follow
        (VALUE_BOUNDARY, cursor + 1)
    } else if len == 255 {
        let ext_len =
            u16::from_le_bytes([data[cursor + 1], data[cursor + 2]]) as usize;
        let start = cursor + 3;
        (&data[start..start + ext_len], start + ext_len)
    } else {
        let start = cursor + 1;
        (&data[start..start + len], start + len)
    }
}

/// Check if a gap is a VALUE_BOUNDARY.
pub fn is_value_boundary(gap: &[u8]) -> bool {
    gap.len() == 1 && gap[0] == VALUE_BOUNDARY_MARKER
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Single-value tests (backward compatible) ──

    #[test]
    fn test_roundtrip_simple() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b" ", b" '", b"_", b"';"]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 1);
        assert_eq!(reader.num_tokens(0), 5);
        assert_eq!(reader.num_values(0), 1);
        assert_eq!(reader.read_gap(0, 0), b"");
        assert_eq!(reader.read_gap(0, 1), b" ");
        assert_eq!(reader.read_gap(0, 2), b" ");
        assert_eq!(reader.read_gap(0, 3), b" '");
        assert_eq!(reader.read_gap(0, 4), b"_");
        assert_eq!(reader.read_gap(0, 5), b"';");
    }

    #[test]
    fn test_roundtrip_empty_doc() {
        let mut writer = GapMapWriter::new();
        writer.add_empty_doc();

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 1);
        assert_eq!(reader.num_tokens(0), 0);
        assert_eq!(reader.num_values(0), 1);
    }

    #[test]
    fn test_roundtrip_multi_docs() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b""]);
        writer.add_doc(&[b"  ", b"\n", b";"]);
        writer.add_empty_doc();

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 3);
        assert_eq!(reader.num_tokens(0), 2);
        assert_eq!(reader.read_gap(0, 1), b" ");
        assert_eq!(reader.num_tokens(1), 2);
        assert_eq!(reader.read_gap(1, 0), b"  ");
        assert_eq!(reader.read_gap(1, 1), b"\n");
        assert_eq!(reader.num_tokens(2), 0);
    }

    #[test]
    fn test_read_all_gaps() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b"_", b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        let gaps = reader.read_all_gaps(0);
        assert_eq!(gaps, vec![b"".as_slice(), b" ", b"_", b""]);
    }

    #[test]
    fn test_long_gap_extended_length() {
        let long_sep = vec![b'x'; 300];
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", long_sep.as_slice(), b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_tokens(0), 2);
        assert_eq!(reader.read_gap(0, 1), long_sep.as_slice());
    }

    #[test]
    fn test_empty_gaps() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b"", b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.read_gap(0, 0), b"");
        assert_eq!(reader.read_gap(0, 1), b"");
        assert_eq!(reader.read_gap(0, 2), b"");
    }

    #[test]
    fn test_separator_single_value() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b"_", b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        // Sep between Ti=0 and Ti=1
        assert_eq!(reader.read_separator(0, 0, 1), Some(b" ".as_slice()));
        // Sep between Ti=1 and Ti=2
        assert_eq!(reader.read_separator(0, 1, 2), Some(b"_".as_slice()));
        // Non-consecutive → None
        assert_eq!(reader.read_separator(0, 0, 2), None);
    }

    // ── Multi-value tests ──

    #[test]
    fn test_multi_value_basic() {
        let mut writer = GapMapWriter::new();
        // Value 0: "hello world" → tokens hello(Ti=0), world(Ti=1)
        // Value 1: "foo bar"     → tokens foo(Ti=3), bar(Ti=4)  (POSITION_GAP=1, so Ti=2+1=3)
        writer.add_doc_multi(
            &[
                vec![b"".as_slice(), b" ", b""],       // value 0: prefix, sep, suffix
                vec![b"".as_slice(), b" ", b""],       // value 1: prefix, sep, suffix
            ],
            &[0, 3], // ti_starts: value 0 starts at Ti=0, value 1 at Ti=3
        );

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 1);
        assert_eq!(reader.num_tokens(0), 4); // hello, world, foo, bar
        assert_eq!(reader.num_values(0), 2);

        // All gaps including VALUE_BOUNDARY
        let gaps = reader.read_all_gaps(0);
        // value 0: ["", " ", ""], boundary, value 1: ["", " ", ""]
        assert_eq!(gaps.len(), 7); // 3 + 1 boundary + 3
        assert_eq!(gaps[0], b"");
        assert_eq!(gaps[1], b" ");
        assert_eq!(gaps[2], b"");
        assert!(is_value_boundary(gaps[3])); // VALUE_BOUNDARY
        assert_eq!(gaps[4], b"");
        assert_eq!(gaps[5], b" ");
        assert_eq!(gaps[6], b"");
    }

    #[test]
    fn test_multi_value_separator() {
        let mut writer = GapMapWriter::new();
        writer.add_doc_multi(
            &[
                vec![b"".as_slice(), b"_", b""],
                vec![b"".as_slice(), b".", b""],
            ],
            &[0, 3],
        );

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        // Within value 0: Ti=0→Ti=1 → sep "_"
        assert_eq!(reader.read_separator(0, 0, 1), Some(b"_".as_slice()));
        // Cross-value: Ti=1→Ti=2 → not consecutive (2 ≠ 1+1... wait, 2 == 1+1)
        // BUT Ti=2 doesn't exist (POSITION_GAP skips it). Ti goes 0,1,3,4.
        // So Ti=1→Ti=3 : not consecutive (3 ≠ 2) → None
        assert_eq!(reader.read_separator(0, 1, 3), None);
        // Within value 1: Ti=3→Ti=4 → sep "."
        assert_eq!(reader.read_separator(0, 3, 4), Some(b".".as_slice()));
    }

    #[test]
    fn test_multi_value_ti_to_seq() {
        let mut writer = GapMapWriter::new();
        // 3 values: tokens at Ti=0,1 | Ti=3,4,5 | Ti=7
        writer.add_doc_multi(
            &[
                vec![b"".as_slice(), b" ", b""],          // 2 tokens
                vec![b"".as_slice(), b" ", b" ", b""],    // 3 tokens
                vec![b"".as_slice(), b""],                // 1 token
            ],
            &[0, 3, 7],
        );

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_tokens(0), 6);
        assert_eq!(reader.num_values(0), 3);

        // Within value 0: Ti=0→1 → sep " "
        assert_eq!(reader.read_separator(0, 0, 1), Some(b" ".as_slice()));
        // Within value 1: Ti=3→4 → sep " "
        assert_eq!(reader.read_separator(0, 3, 4), Some(b" ".as_slice()));
        // Within value 1: Ti=4→5 → sep " "
        assert_eq!(reader.read_separator(0, 4, 5), Some(b" ".as_slice()));
        // Cross value 0→1: Ti=1→3 → None (not consecutive)
        assert_eq!(reader.read_separator(0, 1, 3), None);
        // Cross value 1→2: Ti=5→7 → None (not consecutive)
        assert_eq!(reader.read_separator(0, 5, 7), None);
    }

    #[test]
    fn test_gap_254_bytes_not_confused_with_boundary() {
        // A gap of exactly 254 bytes should NOT be confused with VALUE_BOUNDARY
        let gap_254 = vec![b'y'; 254];
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", gap_254.as_slice(), b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        let gap = reader.read_gap(0, 1);
        assert_eq!(gap.len(), 254);
        assert!(!is_value_boundary(gap));
    }
}

// ─────────────────────────────────────────────────────────────────────
// SfxIndexFile implementation
// ─────────────────────────────────────────────────────────────────────

pub struct GapMapIndex;

impl super::index_registry::SfxIndexFile for GapMapIndex {
    fn id(&self) -> &'static str { "gapmap" }
    fn extension(&self) -> &'static str { "gapmap" }

    fn build(&self, ctx: &super::index_registry::SfxBuildContext) -> Vec<u8> {
        // GapMap is pre-built by the collector (needs raw text + token offsets).
        // Just pass through the pre-built data.
        ctx.gapmap_data.map(|d| d.to_vec()).unwrap_or_default()
    }

    fn merge(&self, _sources: &[Option<&[u8]>], ctx: &super::index_registry::SfxMergeContext) -> Vec<u8> {
        // Copy gap data doc-by-doc in merge order
        let mut writer = GapMapWriter::new();
        for &doc_addr in ctx.doc_mapping {
            let seg_ord = doc_addr.segment_ord as usize;
            let old_doc_id = doc_addr.doc_id;
            if let Some(Some(gapmap_bytes)) = ctx.source_gapmaps.get(seg_ord) {
                let reader = GapMapReader::open(gapmap_bytes);
                let doc_data = reader.doc_data(old_doc_id);
                writer.add_doc_raw(doc_data);
            } else {
                writer.add_empty_doc();
            }
        }
        writer.serialize()
    }
}
