//! ResolvedPostings — adapts Vec<PostingEntry> into the Postings + DocSet traits.
//!
//! This allows any PostingResolver output to be used as a drop-in replacement
//! for SegmentPostings in all scorers (TermScorer, AutomatonScorer, PhraseScorer, etc.).

use crate::docset::{DocSet, TERMINATED};
use crate::postings::Postings;
use crate::query::posting_resolver::PostingEntry;
use crate::DocId;

/// A group of posting entries for a single document.
struct DocGroup {
    doc_id: DocId,
    /// Entries sorted by position. Each entry = one occurrence of the term.
    entries: Vec<PostingEntry>,
}

/// Implements Postings + DocSet from pre-resolved posting entries.
///
/// Entries are grouped by doc_id at construction time. Iteration is O(1) per advance.
/// No disk I/O — all data is in memory from the PostingResolver.
pub struct ResolvedPostings {
    groups: Vec<DocGroup>,
    cursor: usize,
}

impl ResolvedPostings {
    /// Build from a list of PostingEntry, assumed sorted by (doc_id, position).
    /// Groups entries by doc_id for efficient iteration.
    pub fn from_entries(entries: Vec<PostingEntry>) -> Self {
        if entries.is_empty() {
            return Self { groups: Vec::new(), cursor: 0 };
        }

        let mut groups: Vec<DocGroup> = Vec::new();
        let mut current_doc = entries[0].doc_id;
        let mut current_entries: Vec<PostingEntry> = Vec::new();

        for entry in entries {
            if entry.doc_id != current_doc {
                groups.push(DocGroup { doc_id: current_doc, entries: current_entries });
                current_doc = entry.doc_id;
                current_entries = Vec::new();
            }
            current_entries.push(entry);
        }
        groups.push(DocGroup { doc_id: current_doc, entries: current_entries });

        Self { groups, cursor: 0 }
    }

    /// Number of unique documents.
    pub fn num_docs(&self) -> u32 {
        self.groups.len() as u32
    }
}

impl DocSet for ResolvedPostings {
    fn advance(&mut self) -> DocId {
        self.cursor += 1;
        self.doc()
    }

    fn doc(&self) -> DocId {
        if self.cursor < self.groups.len() {
            self.groups[self.cursor].doc_id
        } else {
            TERMINATED
        }
    }

    fn size_hint(&self) -> u32 {
        self.groups.len() as u32
    }

    fn seek(&mut self, target: DocId) -> DocId {
        // Binary search for efficiency on large posting lists.
        if self.cursor >= self.groups.len() {
            return TERMINATED;
        }
        let remaining = &self.groups[self.cursor..];
        match remaining.binary_search_by_key(&target, |g| g.doc_id) {
            Ok(pos) => {
                self.cursor += pos;
                target
            }
            Err(pos) => {
                self.cursor += pos;
                self.doc()
            }
        }
    }
}

impl Postings for ResolvedPostings {
    fn term_freq(&self) -> u32 {
        if self.cursor < self.groups.len() {
            self.groups[self.cursor].entries.len() as u32
        } else {
            0
        }
    }

    fn append_positions_with_offset(&mut self, offset: u32, output: &mut Vec<u32>) {
        if self.cursor < self.groups.len() {
            for entry in &self.groups[self.cursor].entries {
                output.push(entry.position + offset);
            }
        }
    }

    fn append_offsets(&mut self, output: &mut Vec<(u32, u32)>) {
        if self.cursor < self.groups.len() {
            for entry in &self.groups[self.cursor].entries {
                output.push((entry.byte_from, entry.byte_to));
            }
        }
    }

    fn append_positions_and_offsets(&mut self, offset: u32, output: &mut Vec<(u32, u32, u32)>) {
        if self.cursor < self.groups.len() {
            for entry in &self.groups[self.cursor].entries {
                output.push((entry.position + offset, entry.byte_from, entry.byte_to));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entries() -> Vec<PostingEntry> {
        vec![
            PostingEntry { doc_id: 0, position: 0, byte_from: 0, byte_to: 5 },
            PostingEntry { doc_id: 0, position: 3, byte_from: 20, byte_to: 25 },
            PostingEntry { doc_id: 2, position: 1, byte_from: 6, byte_to: 11 },
            PostingEntry { doc_id: 5, position: 0, byte_from: 0, byte_to: 4 },
            PostingEntry { doc_id: 5, position: 2, byte_from: 10, byte_to: 14 },
            PostingEntry { doc_id: 5, position: 7, byte_from: 40, byte_to: 44 },
        ]
    }

    #[test]
    fn test_grouping() {
        let rp = ResolvedPostings::from_entries(make_entries());
        assert_eq!(rp.num_docs(), 3);
    }

    #[test]
    fn test_iteration() {
        let mut rp = ResolvedPostings::from_entries(make_entries());
        assert_eq!(rp.doc(), 0);
        assert_eq!(rp.term_freq(), 2);
        assert_eq!(rp.advance(), 2);
        assert_eq!(rp.term_freq(), 1);
        assert_eq!(rp.advance(), 5);
        assert_eq!(rp.term_freq(), 3);
        assert_eq!(rp.advance(), TERMINATED);
    }

    #[test]
    fn test_seek() {
        let mut rp = ResolvedPostings::from_entries(make_entries());
        assert_eq!(rp.seek(2), 2);
        assert_eq!(rp.doc(), 2);
        assert_eq!(rp.seek(4), 5); // no doc 4, jumps to 5
        assert_eq!(rp.doc(), 5);
        assert_eq!(rp.seek(100), TERMINATED);
    }

    #[test]
    fn test_positions() {
        let mut rp = ResolvedPostings::from_entries(make_entries());
        let mut positions = Vec::new();
        rp.append_positions_with_offset(0, &mut positions);
        assert_eq!(positions, vec![0, 3]); // doc 0 has positions 0 and 3

        rp.advance(); // doc 2
        positions.clear();
        rp.append_positions_with_offset(10, &mut positions);
        assert_eq!(positions, vec![11]); // position 1 + offset 10
    }

    #[test]
    fn test_offsets() {
        let mut rp = ResolvedPostings::from_entries(make_entries());
        rp.seek(5); // doc 5
        let mut offsets = Vec::new();
        rp.append_offsets(&mut offsets);
        assert_eq!(offsets, vec![(0, 4), (10, 14), (40, 44)]);
    }

    #[test]
    fn test_positions_and_offsets() {
        let mut rp = ResolvedPostings::from_entries(make_entries());
        let mut combined = Vec::new();
        rp.append_positions_and_offsets(0, &mut combined);
        assert_eq!(combined, vec![(0, 0, 5), (3, 20, 25)]); // doc 0
    }

    #[test]
    fn test_empty() {
        let mut rp = ResolvedPostings::from_entries(Vec::new());
        assert_eq!(rp.doc(), TERMINATED);
        assert_eq!(rp.term_freq(), 0);
        assert_eq!(rp.advance(), TERMINATED);
    }
}
