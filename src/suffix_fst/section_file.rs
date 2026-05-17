//! Section-based binary file format — extensible container for named sections.
//!
//! Used by `.sfx` and other composite index files. Each version can define
//! its own set of sections without changing the header structure.
//!
//! ## Format
//!
//! ```text
//! [4 bytes]  magic (caller-defined, e.g. "SFX3")
//! [1 byte]   version
//! [2 bytes]  num_sections: u16 LE
//! [section table: num_sections × 12 bytes]
//!   [2 bytes]  section_id: u16 LE
//!   [2 bytes]  reserved (padding, must be 0)
//!   [4 bytes]  offset: u32 LE (from start of data area)
//!   [4 bytes]  length: u32 LE
//! [data area: concatenated section payloads]
//! ```
//!
//! The header size = 7 + num_sections × 12.
//! Section offsets are relative to the start of the data area (after the section table).

/// Size of each entry in the section table.
const ENTRY_SIZE: usize = 12;
/// Fixed header before the section table: magic(4) + version(1) + num_sections(2) = 7.
const FIXED_HEADER: usize = 7;

// ─── Writer ────────────────────────────────────────────────────────────────

/// Builds a section-based file by accumulating named sections.
pub struct SectionFileWriter {
    magic: [u8; 4],
    version: u8,
    sections: Vec<SectionEntry>,
    data: Vec<u8>,
}

struct SectionEntry {
    id: u16,
    offset: u32,
    length: u32,
}

impl SectionFileWriter {
    /// Create a new writer with the given magic bytes and version.
    pub fn new(magic: [u8; 4], version: u8) -> Self {
        Self {
            magic,
            version,
            sections: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Add a section. Returns the section's offset in the data area.
    /// Sections must be added in order of their section_id for deterministic output,
    /// but this is not enforced.
    pub fn add_section(&mut self, section_id: u16, payload: &[u8]) -> u32 {
        let offset = self.data.len() as u32;
        let length = payload.len() as u32;
        self.sections.push(SectionEntry {
            id: section_id,
            offset,
            length,
        });
        self.data.extend_from_slice(payload);
        offset
    }

    /// Serialize the complete file to bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let num_sections = self.sections.len();
        let header_size = FIXED_HEADER + num_sections * ENTRY_SIZE;
        let total = header_size + self.data.len();
        let mut buf = Vec::with_capacity(total);

        // Fixed header
        buf.extend_from_slice(&self.magic);
        buf.push(self.version);
        buf.extend_from_slice(&(num_sections as u16).to_le_bytes());

        // Section table
        for entry in &self.sections {
            buf.extend_from_slice(&entry.id.to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
            buf.extend_from_slice(&entry.offset.to_le_bytes());
            buf.extend_from_slice(&entry.length.to_le_bytes());
        }

        // Data area
        buf.extend_from_slice(&self.data);

        buf
    }

    /// Number of sections added.
    pub fn num_sections(&self) -> usize {
        self.sections.len()
    }
}

// ─── Reader ────────────────────────────────────────────────────────────────

/// Reads a section-based file. Zero-copy: holds a reference to the source bytes.
pub struct SectionFileReader<'a> {
    magic: &'a [u8; 4],
    version: u8,
    num_sections: u16,
    section_table: &'a [u8],
    data: &'a [u8],
}

/// A section descriptor from the section table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionDesc {
    pub id: u16,
    pub offset: u32,
    pub length: u32,
}

impl<'a> SectionFileReader<'a> {
    /// Open and validate a section file from raw bytes.
    /// Checks that the magic matches the expected value.
    pub fn open(bytes: &'a [u8], expected_magic: &[u8; 4]) -> Option<Self> {
        if bytes.len() < FIXED_HEADER {
            return None;
        }
        let magic: &[u8; 4] = bytes[0..4].try_into().ok()?;
        if magic != expected_magic {
            return None;
        }
        let version = bytes[4];
        let num_sections = u16::from_le_bytes(bytes[5..7].try_into().ok()?);
        let table_size = num_sections as usize * ENTRY_SIZE;
        if bytes.len() < FIXED_HEADER + table_size {
            return None;
        }
        let section_table = &bytes[FIXED_HEADER..FIXED_HEADER + table_size];
        let data = &bytes[FIXED_HEADER + table_size..];
        Some(Self {
            magic,
            version,
            num_sections,
            section_table,
            data,
        })
    }

    /// File version.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Magic bytes.
    pub fn magic(&self) -> &[u8; 4] {
        self.magic
    }

    /// Number of sections in the file.
    pub fn num_sections(&self) -> u16 {
        self.num_sections
    }

    /// Get a section descriptor by index (0-based).
    pub fn section_at(&self, index: usize) -> Option<SectionDesc> {
        if index >= self.num_sections as usize {
            return None;
        }
        let pos = index * ENTRY_SIZE;
        let id = u16::from_le_bytes(self.section_table[pos..pos + 2].try_into().ok()?);
        let offset = u32::from_le_bytes(self.section_table[pos + 4..pos + 8].try_into().ok()?);
        let length = u32::from_le_bytes(self.section_table[pos + 8..pos + 12].try_into().ok()?);
        Some(SectionDesc { id, offset, length })
    }

    /// Find a section by ID. Returns the first match.
    pub fn find_section(&self, section_id: u16) -> Option<SectionDesc> {
        for i in 0..self.num_sections as usize {
            if let Some(desc) = self.section_at(i) {
                if desc.id == section_id {
                    return Some(desc);
                }
            }
        }
        None
    }

    /// Get the raw bytes of a section by its descriptor.
    pub fn section_data(&self, desc: &SectionDesc) -> Option<&'a [u8]> {
        let start = desc.offset as usize;
        let end = start + desc.length as usize;
        if end > self.data.len() {
            return None;
        }
        Some(&self.data[start..end])
    }

    /// Convenience: find a section by ID and return its data.
    pub fn get_section(&self, section_id: u16) -> Option<&'a [u8]> {
        let desc = self.find_section(section_id)?;
        self.section_data(&desc)
    }

    /// Check if a section exists.
    pub fn has_section(&self, section_id: u16) -> bool {
        self.find_section(section_id).is_some()
    }

    /// Iterate over all section descriptors.
    pub fn sections(&self) -> impl Iterator<Item = SectionDesc> + '_ {
        (0..self.num_sections as usize).filter_map(move |i| self.section_at(i))
    }
}

// ─── Version detection ─────────────────────────────────────────────────────

/// Detect the SFX file version from raw bytes by reading the magic.
/// Returns None if the bytes are too short or unrecognized.
pub fn detect_sfx_version(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 4 {
        return None;
    }
    match &bytes[0..4] {
        b"SFX1" => Some(1),
        b"SFX2" => Some(2),
        b"SFX3" => Some(3),
        _ => None,
    }
}

/// Detect the termtexts file version from raw bytes.
pub fn detect_termtexts_version(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 4 {
        return None;
    }
    match &bytes[0..4] {
        b"TTXT" => Some(1),
        b"TTX3" => Some(3),
        _ => None,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAGIC: [u8; 4] = *b"TEST";

    #[test]
    fn test_empty_file() {
        let writer = SectionFileWriter::new(TEST_MAGIC, 1);
        let bytes = writer.serialize();

        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();
        assert_eq!(reader.version(), 1);
        assert_eq!(reader.num_sections(), 0);
        assert_eq!(reader.find_section(0), None);
    }

    #[test]
    fn test_single_section() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 3);
        writer.add_section(42, b"hello world");

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        assert_eq!(reader.version(), 3);
        assert_eq!(reader.num_sections(), 1);

        let desc = reader.find_section(42).unwrap();
        assert_eq!(desc.id, 42);
        assert_eq!(desc.length, 11);

        let data = reader.section_data(&desc).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn test_multiple_sections() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        writer.add_section(1, b"FST data here");
        writer.add_section(2, b"parent list");
        writer.add_section(10, b"word map");

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        assert_eq!(reader.num_sections(), 3);

        assert_eq!(reader.get_section(1).unwrap(), b"FST data here");
        assert_eq!(reader.get_section(2).unwrap(), b"parent list");
        assert_eq!(reader.get_section(10).unwrap(), b"word map");
        assert_eq!(reader.get_section(99), None);
    }

    #[test]
    fn test_has_section() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        writer.add_section(5, b"data");

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        assert!(reader.has_section(5));
        assert!(!reader.has_section(6));
    }

    #[test]
    fn test_wrong_magic() {
        let writer = SectionFileWriter::new(TEST_MAGIC, 1);
        let bytes = writer.serialize();

        assert!(SectionFileReader::open(&bytes, b"NOPE").is_none());
    }

    #[test]
    fn test_sections_iterator() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        writer.add_section(1, b"aaa");
        writer.add_section(2, b"bb");
        writer.add_section(3, b"c");

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        let descs: Vec<SectionDesc> = reader.sections().collect();
        assert_eq!(descs.len(), 3);
        assert_eq!(descs[0].id, 1);
        assert_eq!(descs[0].length, 3);
        assert_eq!(descs[1].id, 2);
        assert_eq!(descs[1].length, 2);
        assert_eq!(descs[2].id, 3);
        assert_eq!(descs[2].length, 1);
    }

    #[test]
    fn test_large_section() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        let big = vec![0xAB_u8; 100_000];
        writer.add_section(1, &big);

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        let data = reader.get_section(1).unwrap();
        assert_eq!(data.len(), 100_000);
        assert!(data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_empty_section() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        writer.add_section(1, b"content");
        writer.add_section(2, b""); // empty section
        writer.add_section(3, b"more");

        let bytes = writer.serialize();
        let reader = SectionFileReader::open(&bytes, &TEST_MAGIC).unwrap();

        assert_eq!(reader.get_section(2).unwrap(), b"");
        assert_eq!(reader.get_section(3).unwrap(), b"more");
    }

    #[test]
    fn test_detect_sfx_version() {
        assert_eq!(detect_sfx_version(b"SFX1xxxxx"), Some(1));
        assert_eq!(detect_sfx_version(b"SFX2xxxxx"), Some(2));
        assert_eq!(detect_sfx_version(b"SFX3xxxxx"), Some(3));
        assert_eq!(detect_sfx_version(b"NOPE"), None);
        assert_eq!(detect_sfx_version(b"SF"), None);
    }

    #[test]
    fn test_detect_termtexts_version() {
        assert_eq!(detect_termtexts_version(b"TTXTxxxxx"), Some(1));
        assert_eq!(detect_termtexts_version(b"TTX3xxxxx"), Some(3));
        assert_eq!(detect_termtexts_version(b"NOPE"), None);
    }

    #[test]
    fn test_truncated_file() {
        let mut writer = SectionFileWriter::new(TEST_MAGIC, 1);
        writer.add_section(1, b"data");

        let bytes = writer.serialize();
        // Truncate before section table is complete
        assert!(SectionFileReader::open(&bytes[..10], &TEST_MAGIC).is_none());
        // Truncate header
        assert!(SectionFileReader::open(&bytes[..3], &TEST_MAGIC).is_none());
    }
}
