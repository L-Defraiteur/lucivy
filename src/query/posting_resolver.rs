//! PostingResolver — posting resolution from .sfxpost files.
//!
//! All query scorers use this trait to resolve ordinals to posting entries.
//! Supports both V1 (pre-loaded) and V2 (lazy, binary-searchable doc_ids).

use std::collections::HashSet;

use crate::suffix_fst::file::SfxPostingsReader;
use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
use crate::{DocId, SegmentReader};

/// A resolved posting entry: one occurrence of a term in a document.
#[derive(Debug, Clone)]
pub struct PostingEntry {
    /// Document containing this occurrence.
    pub doc_id: DocId,
    /// Token position within the document.
    pub position: u32,
    /// Start byte offset of the term in the original text.
    pub byte_from: u32,
    /// End byte offset (exclusive) of the term in the original text.
    pub byte_to: u32,
}

/// Resolves ordinals to posting entries.
pub trait PostingResolver: Send + Sync {
    /// Resolve a term ordinal to all its posting entries.
    fn resolve(&self, ordinal: u64) -> Vec<PostingEntry>;

    /// Resolve filtered by doc_ids. Only returns entries whose doc_id is in the set.
    /// Default: resolve all then filter. V2 overrides with O(log n) binary search.
    fn resolve_filtered(&self, ordinal: u64, doc_ids: &HashSet<u32>) -> Vec<PostingEntry> {
        self.resolve(ordinal).into_iter()
            .filter(|e| doc_ids.contains(&e.doc_id))
            .collect()
    }

    /// Check if a doc_id has entries for this ordinal. Default: resolve and check.
    /// V2 overrides with O(log n) binary search, zero payload decode.
    fn has_doc(&self, ordinal: u64, doc_id: u32) -> bool {
        self.resolve(ordinal).iter().any(|e| e.doc_id == doc_id)
    }

    /// doc_freq = number of unique docs for this ordinal.
    fn doc_freq(&self, ordinal: u64) -> u32 {
        let entries = self.resolve(ordinal);
        let mut count = 0u32;
        let mut prev = u32::MAX;
        for e in &entries {
            if e.doc_id != prev {
                count += 1;
                prev = e.doc_id;
            }
        }
        count
    }
}

/// Pre-loaded resolver from .sfxpost — all entries in memory, O(1) ordinal lookup.
pub struct SfxPostResolver {
    entries: Vec<Vec<PostingEntry>>,
}

impl SfxPostResolver {
    /// Load all posting entries from a .sfxpost file into memory.
    pub fn from_bytes(data: &[u8]) -> Result<Self, crate::LucivyError> {
        let reader = SfxPostingsReader::open(data)
            .map_err(|e| crate::LucivyError::SystemError(format!("open .sfxpost: {e}")))?;
        let num = reader.num_terms();
        let mut entries = Vec::with_capacity(num as usize);
        for ord in 0..num {
            entries.push(
                reader.entries(ord).into_iter().map(|e| PostingEntry {
                    doc_id: e.doc_id,
                    position: e.token_index,
                    byte_from: e.byte_from,
                    byte_to: e.byte_to,
                }).collect()
            );
        }
        Ok(Self { entries })
    }
}

impl PostingResolver for SfxPostResolver {
    fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
        self.entries.get(ordinal as usize).cloned().unwrap_or_default()
    }

    fn doc_freq(&self, ordinal: u64) -> u32 {
        match self.entries.get(ordinal as usize) {
            Some(entries) => {
                let mut count = 0u32;
                let mut prev = u32::MAX;
                for e in entries {
                    if e.doc_id != prev {
                        count += 1;
                        prev = e.doc_id;
                    }
                }
                count
            }
            None => 0,
        }
    }
}

/// Lazy V2 resolver — reads directly from owned bytes, no pre-loading.
/// O(log n) filtered access via binary-searchable doc_ids.
pub struct SfxPostResolverV2 {
    reader: SfxPostReaderV2,
}

impl SfxPostResolverV2 {
    pub fn new(reader: SfxPostReaderV2) -> Self {
        Self { reader }
    }
}

impl PostingResolver for SfxPostResolverV2 {
    fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
        self.reader.entries(ordinal as u32).into_iter().map(|e| PostingEntry {
            doc_id: e.doc_id,
            position: e.token_index,
            byte_from: e.byte_from,
            byte_to: e.byte_to,
        }).collect()
    }

    fn resolve_filtered(&self, ordinal: u64, doc_ids: &HashSet<u32>) -> Vec<PostingEntry> {
        self.reader.entries_filtered(ordinal as u32, Some(doc_ids)).into_iter().map(|e| PostingEntry {
            doc_id: e.doc_id,
            position: e.token_index,
            byte_from: e.byte_from,
            byte_to: e.byte_to,
        }).collect()
    }

    fn has_doc(&self, ordinal: u64, doc_id: u32) -> bool {
        self.reader.has_doc(ordinal as u32, doc_id)
    }

    fn doc_freq(&self, ordinal: u64) -> u32 {
        self.reader.doc_freq(ordinal as u32)
    }
}

/// Build a PostingResolver from the .sfxpost file for a field in a segment.
/// Auto-detects V2 ("SFP2" magic) vs V1 (legacy) format.
pub fn build_resolver(reader: &SegmentReader, field: crate::schema::Field) -> Result<Box<dyn PostingResolver>, crate::LucivyError> {
    let sfxpost_data = reader.sfxpost_file(field).ok_or_else(|| {
        crate::LucivyError::InvalidArgument(format!(
            "no .sfxpost file for field {:?}. PostingResolver requires suffix postings.",
            field
        ))
    })?;
    let bytes = sfxpost_data.read_bytes().map_err(|e| {
        crate::LucivyError::SystemError(format!("read .sfxpost: {e}"))
    })?;

    // Try V2 first (lazy, binary-searchable doc_ids)
    if let Some(v2_reader) = SfxPostReaderV2::open(bytes.to_vec()) {
        return Ok(Box::new(SfxPostResolverV2::new(v2_reader)));
    }

    // Fallback to V1 (pre-loaded)
    Ok(Box::new(SfxPostResolver::from_bytes(&bytes)?))
}
