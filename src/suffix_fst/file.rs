use std::io::{self, Write};

use lucivy_fst::{IntoStreamer, Map, OutputTable, Streamer};

use super::builder::{decode_output, decode_parent_entries, read_parent_list, ParentEntry, ParentRef};
use super::gapmap::GapMapReader;

// .sfx file format:
//
// HEADER (fixed 49 bytes):
//   magic: [u8; 4] = b"SFX1"
//   version: u8 = 1
//   num_docs: u32 LE
//   num_suffix_terms: u32 LE
//   fst_offset: u64 LE
//   fst_length: u64 LE
//   parent_list_offset: u64 LE
//   parent_list_length: u64 LE
//   gapmap_offset: u64 LE
//
// SECTION A: Suffix FST (at fst_offset, fst_length bytes)
// SECTION B: Parent lists (at parent_list_offset, parent_list_length bytes)
// SECTION C: GapMap (at gapmap_offset, to end of file)

const MAGIC: &[u8; 4] = b"SFX1";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 4 + 1 + 4 + 4 + 8 + 8 + 8 + 8 + 8; // 53 bytes

/// Assembles FST + parent lists + GapMap into a single .sfx file.
pub struct SfxFileWriter {
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
    gapmap_data: Vec<u8>,
    num_docs: u32,
    num_suffix_terms: u32,
}

impl SfxFileWriter {
    pub fn new(
        fst_data: Vec<u8>,
        parent_list_data: Vec<u8>,
        gapmap_data: Vec<u8>,
        num_docs: u32,
        num_suffix_terms: u32,
    ) -> Self {
        Self {
            fst_data,
            parent_list_data,
            gapmap_data,
            num_docs,
            num_suffix_terms,
        }
    }

    /// Write the complete .sfx file.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let fst_offset = HEADER_SIZE as u64;
        let fst_length = self.fst_data.len() as u64;
        let parent_list_offset = fst_offset + fst_length;
        let parent_list_length = self.parent_list_data.len() as u64;
        let gapmap_offset = parent_list_offset + parent_list_length;

        // Header
        writer.write_all(MAGIC)?;
        writer.write_all(&[VERSION])?;
        writer.write_all(&self.num_docs.to_le_bytes())?;
        writer.write_all(&self.num_suffix_terms.to_le_bytes())?;
        writer.write_all(&fst_offset.to_le_bytes())?;
        writer.write_all(&fst_length.to_le_bytes())?;
        writer.write_all(&parent_list_offset.to_le_bytes())?;
        writer.write_all(&parent_list_length.to_le_bytes())?;
        writer.write_all(&gapmap_offset.to_le_bytes())?;

        // Sections
        writer.write_all(&self.fst_data)?;
        writer.write_all(&self.parent_list_data)?;
        writer.write_all(&self.gapmap_data)?;

        Ok(())
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.write_to(&mut buf).unwrap();
        buf
    }
}

/// Reads a .sfx file from mmap'd or in-memory data.
pub struct SfxFileReader<'a> {
    fst: Map<Vec<u8>>,
    parent_list_data: &'a [u8],
    gapmap: GapMapReader<'a>,
    num_docs: u32,
    num_suffix_terms: u32,
}

impl<'a> SfxFileReader<'a> {
    /// Open a .sfx file from raw bytes (mmap'd or in-memory).
    pub fn open(data: &'a [u8]) -> Result<Self, SfxError> {
        // Validate magic
        if data.len() < HEADER_SIZE || &data[0..4] != MAGIC {
            return Err(SfxError::InvalidMagic);
        }
        let version = data[4];
        if version != VERSION {
            return Err(SfxError::UnsupportedVersion(version));
        }

        let num_docs = u32::from_le_bytes(data[5..9].try_into().unwrap());
        let num_suffix_terms = u32::from_le_bytes(data[9..13].try_into().unwrap());
        let fst_offset = u64::from_le_bytes(data[13..21].try_into().unwrap()) as usize;
        let fst_length = u64::from_le_bytes(data[21..29].try_into().unwrap()) as usize;
        let parent_list_offset = u64::from_le_bytes(data[29..37].try_into().unwrap()) as usize;
        let parent_list_length = u64::from_le_bytes(data[37..45].try_into().unwrap()) as usize;
        let gapmap_offset = u64::from_le_bytes(data[45..53].try_into().unwrap()) as usize;

        let fst_bytes = data[fst_offset..fst_offset + fst_length].to_vec();
        let fst = Map::new(fst_bytes).map_err(|e| SfxError::FstError(e.to_string()))?;

        let parent_list_data =
            &data[parent_list_offset..parent_list_offset + parent_list_length];
        let gapmap_data = &data[gapmap_offset..];
        let gapmap = GapMapReader::open(gapmap_data);

        Ok(Self {
            fst,
            parent_list_data,
            gapmap,
            num_docs,
            num_suffix_terms,
        })
    }

    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    pub fn num_suffix_terms(&self) -> u32 {
        self.num_suffix_terms
    }

    /// Resolve a suffix term to its parent entries.
    /// Returns empty vec if the suffix is not in the FST.
    pub fn resolve_suffix(&self, suffix: &str) -> Vec<ParentEntry> {
        match self.fst.get(suffix.as_bytes()) {
            Some(val) => self.decode_parents(val),
            None => Vec::new(),
        }
    }

    /// Prefix walk: find all suffix terms starting with `prefix`.
    /// Returns an iterator of (suffix_term, parent_entries).
    pub fn prefix_walk(&self, prefix: &str) -> Vec<(String, Vec<ParentEntry>)> {
        // Compute the exclusive upper bound for the prefix range.
        // e.g., "g3d" → "g3e" (increment last byte)
        let ge = prefix.as_bytes();
        let lt = increment_prefix(ge);

        let mut results = Vec::new();
        let mut stream = if let Some(ref lt_bound) = lt {
            self.fst.range().ge(ge).lt(lt_bound).into_stream()
        } else {
            // prefix is all 0xFF bytes, just scan to end
            self.fst.range().ge(ge).into_stream()
        };

        while let Some((key, val)) = stream.next() {
            let term = String::from_utf8_lossy(key).into_owned();
            let parents = self.decode_parents(val);
            results.push((term, parents));
        }

        results
    }

    /// Access the GapMap reader.
    pub fn gapmap(&self) -> &GapMapReader<'a> {
        &self.gapmap
    }

    fn decode_parents(&self, val: u64) -> Vec<ParentEntry> {
        match decode_output(val) {
            ParentRef::Single { raw_ordinal, si } => {
                vec![ParentEntry { raw_ordinal, si }]
            }
            ParentRef::Multi { offset } => {
                let table = OutputTable::new(self.parent_list_data);
                let record = table.get(offset);
                decode_parent_entries(record)
            }
        }
    }
}

/// Compute the exclusive upper bound for a prefix range scan.
/// Increments the last byte of the prefix. Returns None if overflow (all 0xFF).
fn increment_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    for i in (0..upper.len()).rev() {
        if upper[i] < 0xFF {
            upper[i] += 1;
            upper.truncate(i + 1);
            return Some(upper);
        }
    }
    None
}

#[derive(Debug)]
pub enum SfxError {
    InvalidMagic,
    UnsupportedVersion(u8),
    FstError(String),
}

impl std::fmt::Display for SfxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SfxError::InvalidMagic => write!(f, "invalid .sfx magic bytes"),
            SfxError::UnsupportedVersion(v) => write!(f, "unsupported .sfx version: {v}"),
            SfxError::FstError(e) => write!(f, "FST error: {e}"),
        }
    }
}

impl std::error::Error for SfxError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::builder::{ParentEntry, SuffixFstBuilder};
    use crate::suffix_fst::gapmap::GapMapWriter;

    fn build_test_sfx() -> Vec<u8> {
        // Build suffix FST
        let mut sfx_builder = SuffixFstBuilder::with_min_suffix_len(3);
        sfx_builder.add_token("import", 0);
        sfx_builder.add_token("rag3db", 1);
        sfx_builder.add_token("from", 2);
        sfx_builder.add_token("core", 3);

        let num_terms = sfx_builder.num_terms() as u32;
        let (fst_data, parent_list_data) = sfx_builder.build().unwrap();

        // Build GapMap
        let mut gapmap_writer = GapMapWriter::new();
        gapmap_writer.add_doc(&[b"", b" ", b" ", b" '", b"_", b"';"]);

        let gapmap_data = gapmap_writer.serialize();

        // Assemble .sfx file
        let file_writer = SfxFileWriter::new(
            fst_data,
            parent_list_data,
            gapmap_data,
            1, // num_docs
            num_terms,
        );

        file_writer.to_bytes()
    }

    #[test]
    fn test_sfx_roundtrip() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        assert_eq!(reader.num_docs(), 1);

        // Resolve "g3db" → parent "rag3db" (ordinal=1), SI=2
        let parents = reader.resolve_suffix("g3db");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 2 });

        // Resolve "rag3db" → parent "rag3db" (ordinal=1), SI=0
        let parents = reader.resolve_suffix("rag3db");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 0 });

        // Resolve nonexistent
        assert!(reader.resolve_suffix("xyz").is_empty());
    }

    #[test]
    fn test_sfx_prefix_walk() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // Prefix walk "g3d" → should find "g3db"
        let results = reader.prefix_walk("g3d");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "g3db");
        assert_eq!(results[0].1[0], ParentEntry { raw_ordinal: 1, si: 2 });

        // Prefix walk "rag" → should find "rag3db"
        let results = reader.prefix_walk("rag");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "rag3db");

        // Prefix walk "or" → should find "ore" (suffix of "core") and "ort" (suffix of "import")
        let results = reader.prefix_walk("or");
        assert_eq!(results.len(), 2);
        let terms: Vec<&str> = results.iter().map(|(t, _)| t.as_str()).collect();
        assert!(terms.contains(&"ore"));
        assert!(terms.contains(&"ort"));
    }

    #[test]
    fn test_sfx_gapmap_access() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        assert_eq!(reader.gapmap().read_gap(0, 0), b"");
        assert_eq!(reader.gapmap().read_gap(0, 1), b" ");
        assert_eq!(reader.gapmap().read_gap(0, 4), b"_");
        assert_eq!(reader.gapmap().read_gap(0, 5), b"';");
    }

    #[test]
    fn test_sfx_invalid_magic() {
        let bytes = vec![0u8; 100];
        assert!(SfxFileReader::open(&bytes).is_err());
    }

    #[test]
    fn test_sfx_multi_parent() {
        let mut sfx_builder = SuffixFstBuilder::with_min_suffix_len(3);
        sfx_builder.add_token("core", 0);
        sfx_builder.add_token("hardcore", 1);

        let num_terms = sfx_builder.num_terms() as u32;
        let (fst_data, parent_list_data) = sfx_builder.build().unwrap();

        let mut gapmap_writer = GapMapWriter::new();
        gapmap_writer.add_empty_doc();
        let gapmap_data = gapmap_writer.serialize();

        let file_writer = SfxFileWriter::new(
            fst_data,
            parent_list_data,
            gapmap_data,
            1,
            num_terms,
        );
        let bytes = file_writer.to_bytes();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "core" should have 2 parents
        let parents = reader.resolve_suffix("core");
        assert_eq!(parents.len(), 2);
        assert!(parents.contains(&ParentEntry { raw_ordinal: 0, si: 0 }));
        assert!(parents.contains(&ParentEntry { raw_ordinal: 1, si: 4 }));

        // Prefix walk "cor" → finds "core" with 2 parents
        let results = reader.prefix_walk("cor");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "core");
        assert_eq!(results[0].1.len(), 2);
    }
}
