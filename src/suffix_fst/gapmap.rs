use std::io::{self, Write};

// GapMap binary format:
//
// HEADER:
//   num_docs: u32 LE
//   offsets: [u64 LE × (num_docs + 1)]   (offset into data section for each doc)
//
// DATA (per doc):
//   num_tokens: u16 LE
//   gaps: [gap × (num_tokens + 1)]
//     gap = [len: u8][bytes: [u8; len]]
//     len = 255 → extended: [ext_len: u16 LE][bytes: [u8; ext_len]]
//
// gap[0]       = prefix (before token 0)
// gap[i]       = separator between token i-1 and token i
// gap[N]       = suffix (after last token)

const HEADER_SIZE_BASE: usize = 4; // num_docs: u32

/// Writes gap data for multiple documents into a binary format.
pub struct GapMapWriter {
    data_buffer: Vec<u8>,
    doc_offsets: Vec<u64>,
}

impl GapMapWriter {
    pub fn new() -> Self {
        Self {
            data_buffer: Vec::new(),
            doc_offsets: Vec::new(),
        }
    }

    /// Add gap data for one document.
    /// `gaps` contains num_tokens + 1 entries:
    ///   gaps[0] = prefix before first token
    ///   gaps[i] = separator between token i-1 and token i
    ///   gaps[num_tokens] = suffix after last token
    pub fn add_doc(&mut self, gaps: &[&[u8]]) {
        self.doc_offsets.push(self.data_buffer.len() as u64);

        let num_tokens = if gaps.is_empty() { 0u16 } else { (gaps.len() - 1) as u16 };
        self.data_buffer.extend_from_slice(&num_tokens.to_le_bytes());

        for gap in gaps {
            encode_gap(&mut self.data_buffer, gap);
        }
    }

    /// Add an empty document (no tokens, no gaps).
    pub fn add_empty_doc(&mut self) {
        self.doc_offsets.push(self.data_buffer.len() as u64);
        self.data_buffer.extend_from_slice(&0u16.to_le_bytes());
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
        // Sentinel: end of data
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
    if gap.len() < 255 {
        buf.push(gap.len() as u8);
        buf.extend_from_slice(gap);
    } else {
        buf.push(255);
        buf.extend_from_slice(&(gap.len() as u16).to_le_bytes());
        buf.extend_from_slice(gap);
    }
}

/// Reads gap data from a mmap'd or in-memory GapMap.
pub struct GapMapReader<'a> {
    data: &'a [u8],
    num_docs: u32,
}

impl<'a> GapMapReader<'a> {
    pub fn open(data: &'a [u8]) -> Self {
        let num_docs = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        Self { data, num_docs }
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Get the raw bytes for a document's gap data.
    fn doc_data(&self, doc_id: u32) -> &'a [u8] {
        let offset_pos = HEADER_SIZE_BASE + (doc_id as usize) * 8;
        let start = u64::from_le_bytes(
            self.data[offset_pos..offset_pos + 8].try_into().unwrap(),
        ) as usize;
        let end = u64::from_le_bytes(
            self.data[offset_pos + 8..offset_pos + 16].try_into().unwrap(),
        ) as usize;
        &self.data[start..end]
    }

    /// Number of tokens in a document.
    pub fn num_tokens(&self, doc_id: u32) -> u16 {
        let dd = self.doc_data(doc_id);
        u16::from_le_bytes([dd[0], dd[1]])
    }

    /// Read the gap at `gap_index` for document `doc_id`.
    ///
    /// gap_index = 0             → prefix before token 0
    /// gap_index = Ti + 1        → separator after token Ti
    /// gap_index = num_tokens    → suffix after last token
    pub fn read_gap(&self, doc_id: u32, gap_index: u32) -> &'a [u8] {
        let dd = self.doc_data(doc_id);
        let mut cursor = 2; // skip num_tokens: u16

        // Skip to the target gap
        for _ in 0..gap_index {
            let (_, next) = decode_gap_at(dd, cursor);
            cursor = next;
        }

        let (gap, _) = decode_gap_at(dd, cursor);
        gap
    }

    /// Read all gaps for a document. Returns num_tokens + 1 gaps.
    pub fn read_all_gaps(&self, doc_id: u32) -> Vec<&'a [u8]> {
        let dd = self.doc_data(doc_id);
        let num_tokens = u16::from_le_bytes([dd[0], dd[1]]) as usize;
        let num_gaps = num_tokens + 1;
        let mut cursor = 2;
        let mut gaps = Vec::with_capacity(num_gaps);

        for _ in 0..num_gaps {
            let (gap, next) = decode_gap_at(dd, cursor);
            gaps.push(gap);
            cursor = next;
        }

        gaps
    }
}

/// Decode a gap at position `cursor` in the data. Returns (gap_bytes, next_cursor).
fn decode_gap_at(data: &[u8], cursor: usize) -> (&[u8], usize) {
    let len = data[cursor] as usize;
    if len == 255 {
        let ext_len =
            u16::from_le_bytes([data[cursor + 1], data[cursor + 2]]) as usize;
        let start = cursor + 3;
        (&data[start..start + ext_len], start + ext_len)
    } else {
        let start = cursor + 1;
        (&data[start..start + len], start + len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_simple() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b" ", b" '", b"_", b"';"]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 1);
        assert_eq!(reader.num_tokens(0), 5);
        assert_eq!(reader.read_gap(0, 0), b"");      // prefix
        assert_eq!(reader.read_gap(0, 1), b" ");      // sep after token 0
        assert_eq!(reader.read_gap(0, 2), b" ");      // sep after token 1
        assert_eq!(reader.read_gap(0, 3), b" '");     // sep after token 2
        assert_eq!(reader.read_gap(0, 4), b"_");      // sep after token 3
        assert_eq!(reader.read_gap(0, 5), b"';");     // suffix
    }

    #[test]
    fn test_roundtrip_empty_doc() {
        let mut writer = GapMapWriter::new();
        writer.add_empty_doc();

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 1);
        assert_eq!(reader.num_tokens(0), 0);
    }

    #[test]
    fn test_roundtrip_multi_docs() {
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b" ", b""]);       // doc 0: 2 tokens
        writer.add_doc(&[b"  ", b"\n", b";"]);    // doc 1: 2 tokens
        writer.add_empty_doc();                   // doc 2: empty

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.num_docs(), 3);

        // Doc 0
        assert_eq!(reader.num_tokens(0), 2);
        assert_eq!(reader.read_gap(0, 0), b"");
        assert_eq!(reader.read_gap(0, 1), b" ");
        assert_eq!(reader.read_gap(0, 2), b"");

        // Doc 1
        assert_eq!(reader.num_tokens(1), 2);
        assert_eq!(reader.read_gap(1, 0), b"  ");
        assert_eq!(reader.read_gap(1, 1), b"\n");
        assert_eq!(reader.read_gap(1, 2), b";");

        // Doc 2
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
        // Tokens directly adjacent (no separator)
        let mut writer = GapMapWriter::new();
        writer.add_doc(&[b"", b"", b""]);

        let bytes = writer.serialize();
        let reader = GapMapReader::open(&bytes);

        assert_eq!(reader.read_gap(0, 0), b"");
        assert_eq!(reader.read_gap(0, 1), b"");
        assert_eq!(reader.read_gap(0, 2), b"");
    }
}
