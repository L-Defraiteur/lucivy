//! A compact table that maps u64 offsets to variable-length byte records.
//!
//! Designed to be used alongside an FST: the FST maps keys to u64 offsets,
//! and this table resolves those offsets to richer data structures.
//!
//! The caller is responsible for encoding/decoding the record bytes into
//! domain-specific types. This table is a generic byte-level container.
//!
//! # Format
//!
//! Records are stored sequentially:
//! ```text
//! [varint length][record bytes][varint length][record bytes]...
//! ```
//!
//! The FST output value is the byte offset of the record's length prefix.

/// Builder for an [`OutputTable`].
///
/// Records are appended sequentially. The returned offset from [`add`] should
/// be stored as the FST output value for the corresponding key.
pub struct OutputTableBuilder {
    data: Vec<u8>,
}

impl OutputTableBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        OutputTableBuilder { data: Vec::new() }
    }

    /// Add a record to the table. Returns the byte offset to store in the FST.
    pub fn add(&mut self, record: &[u8]) -> u64 {
        let offset = self.data.len() as u64;
        encode_varint(&mut self.data, record.len() as u64);
        self.data.extend_from_slice(record);
        offset
    }

    /// Current size in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Finish building and return the raw bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.data
    }
}

impl Default for OutputTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Reader for an [`OutputTable`].
///
/// Zero-copy: borrows the underlying byte slice.
pub struct OutputTable<'a> {
    data: &'a [u8],
}

impl<'a> OutputTable<'a> {
    /// Create a reader over the given bytes.
    pub fn new(data: &'a [u8]) -> Self {
        OutputTable { data }
    }

    /// Read the record at the given byte offset.
    ///
    /// # Panics
    ///
    /// Panics if the offset is out of bounds or the record is malformed.
    pub fn get(&self, offset: u64) -> &'a [u8] {
        let pos = offset as usize;
        let (len, varint_size) = decode_varint(&self.data[pos..]);
        let start = pos + varint_size;
        let end = start + len as usize;
        &self.data[start..end]
    }

    /// Read the record at the given byte offset, returning None if invalid.
    pub fn try_get(&self, offset: u64) -> Option<&'a [u8]> {
        let pos = offset as usize;
        if pos >= self.data.len() {
            return None;
        }
        let (len, varint_size) = decode_varint(&self.data[pos..]);
        let start = pos + varint_size;
        let end = start + len as usize;
        if end > self.data.len() {
            return None;
        }
        Some(&self.data[start..end])
    }

    /// Total size of the table in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Encode a u64 as a varint (LEB128).
fn encode_varint(buf: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        buf.push((n as u8) | 0x80);
        n >>= 7;
    }
    buf.push(n as u8);
}

/// Decode a varint (LEB128) from the start of the slice.
/// Returns (value, number of bytes consumed).
fn decode_varint(data: &[u8]) -> (u64, usize) {
    let mut n: u64 = 0;
    let mut shift = 0;
    for (i, &b) in data.iter().enumerate() {
        n |= ((b & 0x7F) as u64) << shift;
        if b < 0x80 {
            return (n, i + 1);
        }
        shift += 7;
    }
    panic!("BUG: unterminated varint");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table() {
        let builder = OutputTableBuilder::new();
        assert!(builder.is_empty());
        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert!(table.is_empty());
    }

    #[test]
    fn single_record() {
        let mut builder = OutputTableBuilder::new();
        let offset = builder.add(b"hello");
        assert_eq!(offset, 0);
        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert_eq!(table.get(offset), b"hello");
    }

    #[test]
    fn multiple_records() {
        let mut builder = OutputTableBuilder::new();
        let o1 = builder.add(b"abc");
        let o2 = builder.add(b"defgh");
        let o3 = builder.add(b"");
        let o4 = builder.add(b"x");
        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert_eq!(table.get(o1), b"abc");
        assert_eq!(table.get(o2), b"defgh");
        assert_eq!(table.get(o3), b"");
        assert_eq!(table.get(o4), b"x");
    }

    #[test]
    fn large_record() {
        let mut builder = OutputTableBuilder::new();
        let big = vec![0xAB; 1000];
        let offset = builder.add(&big);
        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert_eq!(table.get(offset), &big[..]);
        // 1000 needs 2 varint bytes (1000 >= 128)
        assert_eq!(data.len(), 2 + 1000);
    }

    #[test]
    fn varint_encoding() {
        // Test boundary values
        let mut builder = OutputTableBuilder::new();
        // 127 bytes = 1-byte varint
        let small = vec![0x01; 127];
        let o1 = builder.add(&small);
        // 128 bytes = 2-byte varint
        let medium = vec![0x02; 128];
        let o2 = builder.add(&medium);
        // 16384 bytes = 3-byte varint
        let large = vec![0x03; 16384];
        let o3 = builder.add(&large);

        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert_eq!(table.get(o1).len(), 127);
        assert_eq!(table.get(o2).len(), 128);
        assert_eq!(table.get(o3).len(), 16384);
    }

    #[test]
    fn try_get_invalid_offset() {
        let mut builder = OutputTableBuilder::new();
        builder.add(b"test");
        let data = builder.into_inner();
        let table = OutputTable::new(&data);
        assert!(table.try_get(0).is_some());
        assert!(table.try_get(999).is_none());
    }

    #[test]
    fn offsets_are_sequential() {
        let mut builder = OutputTableBuilder::new();
        let o1 = builder.add(b"aa");   // varint(2)=1 byte + 2 = 3 bytes total
        let o2 = builder.add(b"bbb");  // starts at offset 3
        let o3 = builder.add(b"cc");   // starts at offset 3+1+3 = 7
        assert_eq!(o1, 0);
        assert_eq!(o2, 3);  // 1 (varint) + 2 (data)
        assert_eq!(o3, 7);  // 3 + 1 (varint) + 3 (data)
    }
}
