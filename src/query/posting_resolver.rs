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

/// A set of documents a resolution may be restricted to.
///
/// Implemented by a `HashSet<u32>` of ids and by a segment's `AliveBitSet`,
/// so a filtered search hands the resolvers the same bitset the collector
/// applies — and the postings of documents outside it are never decoded.
/// `len` and `for_each` let a small set drive the loop (`filtered_indices`).
pub trait DocFilter: Sync {
    /// Whether `doc` is in the set.
    fn contains(&self, doc: u32) -> bool;
    /// Number of documents in the set.
    fn len(&self) -> usize;
    /// Whether the set is empty.
    fn is_empty(&self) -> bool { self.len() == 0 }
    /// Visit every document of the set.
    fn for_each(&self, f: &mut dyn FnMut(u32));
}

impl DocFilter for HashSet<u32> {
    fn contains(&self, doc: u32) -> bool { HashSet::contains(self, &doc) }
    fn len(&self) -> usize { HashSet::len(self) }
    fn for_each(&self, f: &mut dyn FnMut(u32)) { for &d in self { f(d); } }
}

impl DocFilter for crate::fastfield::AliveBitSet {
    fn contains(&self, doc: u32) -> bool { self.is_alive(doc) }
    fn len(&self) -> usize { self.num_alive_docs() }
    fn for_each(&self, f: &mut dyn FnMut(u32)) { for d in self.iter_alive() { f(d); } }
}

/// Resolves ordinals to posting entries.
pub trait PostingResolver: Send + Sync {
    /// Resolve a term ordinal to all its posting entries.
    fn resolve(&self, ordinal: u64) -> Vec<PostingEntry>;

    /// Resolve filtered by doc_ids. Only returns entries whose doc_id is in the set.
    /// Default: resolve all then filter. V2 overrides with O(log n) binary search.
    fn resolve_filtered(&self, ordinal: u64, doc_ids: &dyn DocFilter) -> Vec<PostingEntry> {
        self.resolve(ordinal).into_iter()
            .filter(|e| doc_ids.contains(e.doc_id))
            .collect()
    }

    /// Entries of one ordinal in one document.
    ///
    /// The chain resolver calls this once per surviving (doc, position) pair,
    /// after posmap has already said which ordinal sits there — so only the
    /// survivors' bytes are ever decoded. Default: filtered resolve on a
    /// singleton set. V2 overrides with a binary search and a single payload
    /// decode.
    fn resolve_doc(&self, ordinal: u64, doc_id: u32) -> Vec<PostingEntry> {
        let mut one = HashSet::with_capacity(1);
        one.insert(doc_id);
        self.resolve_filtered(ordinal, &one)
    }

    /// The posting of (`ordinal`, `doc_id`) at `position`, if any.
    /// Default: resolve the document and look. V2 overrides with a scan
    /// that stops at the position, without allocating.
    fn resolve_doc_at(&self, ordinal: u64, doc_id: u32, position: u32) -> Option<PostingEntry> {
        self.resolve_doc(ordinal, doc_id).into_iter().find(|p| p.position == position)
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
    /// Creates a new SFX posting resolver from a V2 reader.
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

    fn resolve_filtered(&self, ordinal: u64, doc_ids: &dyn DocFilter) -> Vec<PostingEntry> {
        self.reader.entries_filtered(ordinal as u32, Some(doc_ids)).into_iter().map(|e| PostingEntry {
            doc_id: e.doc_id,
            position: e.token_index,
            byte_from: e.byte_from,
            byte_to: e.byte_to,
        }).collect()
    }

    fn resolve_doc(&self, ordinal: u64, doc_id: u32) -> Vec<PostingEntry> {
        self.reader.entries_for_doc(ordinal as u32, doc_id).into_iter().map(|e| PostingEntry {
            doc_id: e.doc_id,
            position: e.token_index,
            byte_from: e.byte_from,
            byte_to: e.byte_to,
        }).collect()
    }

    fn resolve_doc_at(&self, ordinal: u64, doc_id: u32, position: u32) -> Option<PostingEntry> {
        self.reader.entry_at(ordinal as u32, doc_id, position).map(|(byte_from, byte_to)| PostingEntry {
            doc_id, position, byte_from, byte_to,
        })
    }

    fn has_doc(&self, ordinal: u64, doc_id: u32) -> bool {
        self.reader.has_doc(ordinal as u32, doc_id)
    }

    fn doc_freq(&self, ordinal: u64) -> u32 {
        self.reader.doc_freq(ordinal as u32)
    }
}

/// Build a PostingResolver from the .sfxpost V2 file for a field in a segment.
pub fn build_resolver(reader: &SegmentReader, field: crate::schema::Field) -> Result<Box<dyn PostingResolver>, crate::LucivyError> {
    let sfxpost_data = reader.sfxpost_file(field).ok_or_else(|| {
        crate::LucivyError::InvalidArgument(format!(
            "no .sfxpost file for field {field:?}. PostingResolver requires suffix postings."
        ))
    })?;
    let bytes = sfxpost_data.read_bytes().map_err(|e| {
        crate::LucivyError::SystemError(format!("read .sfxpost: {e}"))
    })?;

    let mut v2_reader = SfxPostReaderV2::open_owned(bytes).ok_or_else(|| {
        crate::LucivyError::SystemError("sfxpost: invalid V2 format (missing SFP2 magic)".into())
    })?;
    // A dictionary segment answers by local ordinal; callers ask by global id.
    if let Some(gmap) = reader.sfx_index_file("gmap", field).and_then(|f| f.read_bytes().ok()) {
        v2_reader = v2_reader.with_gmap(gmap);
    }
    Ok(Box::new(SfxPostResolverV2::new(v2_reader)))
}
