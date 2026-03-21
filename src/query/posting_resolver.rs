//! PostingResolver — posting resolution from .sfxpost files.
//!
//! All query scorers use this trait to resolve ordinals to posting entries.

use crate::suffix_fst::file::SfxPostingsReader;
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

/// Build a PostingResolver from the .sfxpost file for a field in a segment.
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
    Ok(Box::new(SfxPostResolver::from_bytes(&bytes)?))
}
