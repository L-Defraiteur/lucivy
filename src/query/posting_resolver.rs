//! PostingResolver — unified posting resolution from .sfxpost or ._raw inverted index.
//!
//! All query scorers use this trait to resolve ordinals to posting entries,
//! abstracting over the data source. When .sfxpost is available, it is preferred
//! (zero dependency on ._raw). Otherwise, falls back to the inverted index.

use std::sync::Arc;

use crate::docset::{DocSet, TERMINATED};
use crate::index::InvertedIndexReader;
use crate::postings::Postings;
use crate::schema::IndexRecordOption;
use crate::suffix_fst::file::SfxPostingsReader;
use crate::{DocId, SegmentReader};

/// A resolved posting entry: one occurrence of a term in a document.
#[derive(Debug, Clone)]
pub struct PostingEntry {
    pub doc_id: DocId,
    pub position: u32,
    pub byte_from: u32,
    pub byte_to: u32,
}

/// Resolves ordinals to posting entries.
pub trait PostingResolver: Send + Sync {
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

/// Fallback resolver from the ._raw inverted index (for old indexes without .sfxpost).
pub struct InvertedIndexResolver {
    inv_idx: Arc<InvertedIndexReader>,
}

impl InvertedIndexResolver {
    pub fn new(inv_idx: Arc<InvertedIndexReader>) -> Self {
        Self { inv_idx }
    }
}

impl PostingResolver for InvertedIndexResolver {
    fn resolve(&self, ordinal: u64) -> Vec<PostingEntry> {
        let term_dict = self.inv_idx.terms();
        let term_info = term_dict.term_info_from_ord(ordinal);
        let mut postings = match self.inv_idx.read_postings_from_terminfo(
            &term_info,
            IndexRecordOption::WithFreqsAndPositionsAndOffsets,
        ) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        let mut entries = Vec::new();
        loop {
            let doc = postings.doc();
            if doc == TERMINATED { break; }
            let mut pos_offsets = Vec::new();
            postings.append_positions_and_offsets(0, &mut pos_offsets);
            for (pos, off_from, off_to) in pos_offsets {
                entries.push(PostingEntry {
                    doc_id: doc, position: pos, byte_from: off_from, byte_to: off_to,
                });
            }
            postings.advance();
        }
        entries
    }
}

/// Build the best available PostingResolver for a field in a segment.
/// Prefers .sfxpost (self-contained), falls back to ._raw inverted index.
pub fn build_resolver(reader: &SegmentReader, field: crate::schema::Field) -> Result<Box<dyn PostingResolver>, crate::LucivyError> {
    if let Some(sfxpost_data) = reader.sfxpost_file(field) {
        let bytes = sfxpost_data.read_bytes().map_err(|e| {
            crate::LucivyError::SystemError(format!("read .sfxpost: {e}"))
        })?;
        return Ok(Box::new(SfxPostResolver::from_bytes(&bytes)?));
    }
    let inv_idx = reader.inverted_index(field)?;
    Ok(Box::new(InvertedIndexResolver::new(Arc::clone(&inv_idx))))
}
