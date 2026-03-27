use std::io::{self, Write};

use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use lucivy_fst::{Automaton, IntoStreamer, Map, OutputTable, Streamer};
use lucivy_fst::Levenshtein as LevAutomaton;

use super::builder::{decode_output, decode_parent_entries, ParentEntry, ParentRef};
use super::gapmap::GapMapReader;
use super::sibling_table::SiblingTableReader;

/// DFA wrapper implementing lucivy_fst::Automaton for Levenshtein search on the suffix FST.
pub(crate) struct SfxDfaWrapper(pub DFA);

impl Automaton for SfxDfaWrapper {
    type State = u32;

    fn start(&self) -> Self::State {
        self.0.initial_state()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.transition(*state, byte)
    }
}

// .sfx file format v2:
//
// HEADER (fixed 69 bytes):
//   magic: [u8; 4] = b"SFX2"
//   version: u8 = 2
//   num_docs: u32 LE
//   num_suffix_terms: u32 LE
//   fst_offset: u64 LE
//   fst_length: u64 LE
//   parent_list_offset: u64 LE
//   parent_list_length: u64 LE
//   gapmap_offset: u64 LE
//   gapmap_length: u64 LE          (NEW in v2)
//   postings_offset: u64 LE        (NEW in v2 — 0 if absent)
//
// SECTION A: Suffix FST (at fst_offset, fst_length bytes)
// SECTION B: Parent lists (at parent_list_offset, parent_list_length bytes)
// SECTION C: GapMap (at gapmap_offset, gapmap_length bytes)
// SECTION D: Mini postings (at postings_offset, to end of file)
//            Format: [num_terms: u32] [offsets: u32 × (num_terms+1)] [delta-VInt doc_ids]

const MAGIC_V1: &[u8; 4] = b"SFX1";
const HEADER_SIZE_V1: usize = 4 + 1 + 4 + 4 + 8 + 8 + 8 + 8 + 8; // 53 bytes

/// Assembles FST + parent lists + sibling table + GapMap into a single .sfx file.
///
/// Layout: [header 53B] [FST] [parent list] [sibling table] [GapMap]
/// The GapMap is always last (its data extends to EOF for backward compat).
/// The sibling table is inserted between parent list and GapMap.
/// If sibling_data is empty, gapmap_offset == parent_list_end (no sibling section).
pub struct SfxFileWriter {
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
    sibling_data: Vec<u8>,
    gapmap_data: Vec<u8>,
    num_docs: u32,
    /// Number of unique suffix terms in the FST.
    num_suffix_terms: u32,
}

impl SfxFileWriter {
    /// Create a new writer from pre-built FST, parent list, and GapMap data.
    /// Sibling table is optional (empty = no cross-token links).
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
            sibling_data: Vec::new(),
            gapmap_data,
            num_docs,
            num_suffix_terms,
        }
    }

    /// Set the sibling table data (from SiblingTableWriter::serialize()).
    pub fn with_sibling_data(mut self, data: Vec<u8>) -> Self {
        self.sibling_data = data;
        self
    }

    /// Write the complete .sfx file.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let fst_offset = HEADER_SIZE_V1 as u64;
        let fst_length = self.fst_data.len() as u64;
        let parent_list_offset = fst_offset + fst_length;
        let parent_list_length = self.parent_list_data.len() as u64;
        // Sibling table sits between parent list and GapMap.
        let sibling_offset = parent_list_offset + parent_list_length;
        let sibling_length = self.sibling_data.len() as u64;
        let gapmap_offset = sibling_offset + sibling_length;

        // Header (unchanged layout — gapmap_offset now accounts for sibling section)
        writer.write_all(MAGIC_V1)?;
        writer.write_all(&[1u8])?;
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
        writer.write_all(&self.sibling_data)?;
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

/// A split candidate found during a falling walk.
/// Represents a point where a prefix of the query reaches the end of an indexed token.
#[derive(Debug, Clone)]
pub struct SplitCandidate {
    /// How many bytes of the query are consumed by the left part.
    pub prefix_len: usize,
    /// The parent entry that reaches its token boundary.
    pub parent: ParentEntry,
}

/// Reads a .sfx file from mmap'd or in-memory data.
pub struct SfxFileReader<'a> {
    fst: Map<Vec<u8>>,
    parent_list_data: &'a [u8],
    sibling_table: Option<SiblingTableReader<'a>>,
    gapmap: GapMapReader<'a>,
    num_docs: u32,
    num_suffix_terms: u32,
}

impl<'a> SfxFileReader<'a> {
    /// Open a .sfx file from raw bytes (mmap'd or in-memory).
    pub fn open(data: &'a [u8]) -> Result<Self, SfxError> {
        if data.len() < HEADER_SIZE_V1 || &data[0..4] != MAGIC_V1 {
            return Err(SfxError::InvalidMagic);
        }

        let num_docs = u32::from_le_bytes(data[5..9].try_into().unwrap());
        let num_suffix_terms = u32::from_le_bytes(data[9..13].try_into().unwrap());
        let fst_offset = u64::from_le_bytes(data[13..21].try_into().unwrap()) as usize;
        let fst_length = u64::from_le_bytes(data[21..29].try_into().unwrap()) as usize;
        let parent_list_offset = u64::from_le_bytes(data[29..37].try_into().unwrap()) as usize;
        let parent_list_length = u64::from_le_bytes(data[37..45].try_into().unwrap()) as usize;
        let gapmap_offset = u64::from_le_bytes(data[45..53].try_into().unwrap()) as usize;

        // Handle empty FST (deferred merge: FST not yet rebuilt).
        // Build an empty Map so the reader is valid but has no entries.
        let fst = if fst_length == 0 {
            Map::new(lucivy_fst::MapBuilder::memory().into_inner().unwrap_or_default())
                .map_err(|e| SfxError::FstError(e.to_string()))?
        } else {
            let fst_bytes = data[fst_offset..fst_offset + fst_length].to_vec();
            Map::new(fst_bytes).map_err(|e| SfxError::FstError(e.to_string()))?
        };

        let parent_list_data = &data[parent_list_offset..parent_list_offset + parent_list_length];

        // Sibling table sits between parent_list_end and gapmap_offset.
        // If they're equal, no sibling table (backward compat with old files).
        let parent_list_end = parent_list_offset + parent_list_length;
        let sibling_table = if gapmap_offset > parent_list_end {
            let sibling_data = &data[parent_list_end..gapmap_offset];
            SiblingTableReader::open(sibling_data)
        } else {
            None
        };

        let gapmap_data = &data[gapmap_offset..];
        let gapmap = GapMapReader::open(gapmap_data);

        Ok(Self {
            fst,
            parent_list_data,
            sibling_table,
            gapmap,
            num_docs,
            num_suffix_terms,
        })
    }

    /// Number of documents indexed in this `.sfx` file.
    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Number of unique suffix terms in the FST.
    pub fn num_suffix_terms(&self) -> u32 {
        self.num_suffix_terms
    }

    /// Resolve a suffix term to its parent entries.
    /// Returns empty vec if the suffix is not in the FST.
    /// Searches both SI=0 and SI>0 entries (contains mode).
    pub fn resolve_suffix(&self, suffix: &str) -> Vec<ParentEntry> {
        let mut all = Vec::new();
        // Check SI=0 entry
        let key0 = format!("\x00{suffix}");
        if let Some(val) = self.fst.get(key0.as_bytes()) {
            all.extend(self.decode_parents(val));
        }
        // Check SI>0 entry
        let key1 = format!("\x01{suffix}");
        if let Some(val) = self.fst.get(key1.as_bytes()) {
            all.extend(self.decode_parents(val));
        }
        all
    }

    /// Resolve a suffix term to only SI=0 parent entries.
    pub fn resolve_suffix_si0(&self, suffix: &str) -> Vec<ParentEntry> {
        let key0 = format!("\x00{suffix}");
        match self.fst.get(key0.as_bytes()) {
            Some(val) => self.decode_parents(val),
            None => Vec::new(),
        }
    }

    /// Prefix walk: find all suffix terms starting with `prefix`.
    /// Searches both SI=0 and SI>0 entries (contains mode).
    /// Merges parents from both partitions by key.
    pub fn prefix_walk(&self, prefix: &str) -> Vec<(String, Vec<ParentEntry>)> {
        use std::collections::HashMap;
        let mut merged: HashMap<String, Vec<ParentEntry>> = HashMap::new();
        for (key, parents) in self.prefix_walk_with_byte(super::builder::SI0_PREFIX, prefix) {
            merged.entry(key).or_default().extend(parents);
        }
        for (key, parents) in self.prefix_walk_with_byte(super::builder::SI_REST_PREFIX, prefix) {
            merged.entry(key).or_default().extend(parents);
        }
        let mut results: Vec<_> = merged.into_iter().collect();
        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }

    /// Prefix walk for startsWith: only SI=0 entries (full token start).
    /// Much faster than prefix_walk() — skips all substring entries.
    pub fn prefix_walk_si0(&self, prefix: &str) -> Vec<(String, Vec<ParentEntry>)> {
        self.prefix_walk_with_byte(super::builder::SI0_PREFIX, prefix)
    }

    /// Internal: prefix walk within a single partition (SI=0 or SI>0).
    fn prefix_walk_with_byte(&self, prefix_byte: u8, prefix: &str) -> Vec<(String, Vec<ParentEntry>)> {
        let mut ge_key = vec![prefix_byte];
        ge_key.extend_from_slice(prefix.as_bytes());

        let lt_key = increment_prefix(&ge_key);

        let mut results = Vec::new();
        let mut stream = if let Some(ref lt_bound) = lt_key {
            self.fst.range().ge(&ge_key).lt(lt_bound).into_stream()
        } else {
            self.fst.range().ge(&ge_key).into_stream()
        };

        while let Some((key, val)) = stream.next() {
            // Strip the prefix byte from the returned term
            let term = String::from_utf8_lossy(&key[1..]).into_owned();
            let parents = self.decode_parents(val);
            results.push((term, parents));
        }

        crate::diag_emit!(crate::diag::DiagEvent::SfxWalk {
            query: prefix.to_string(),
            segment_id: String::new(),
            si0_entries: if prefix_byte == super::builder::SI0_PREFIX { results.len() } else { 0 },
            si_rest_entries: if prefix_byte != super::builder::SI0_PREFIX { results.len() } else { 0 },
            total_parents: results.iter().map(|(_, p)| p.len()).sum(),
        });

        results
    }

    /// Fuzzy walk: find all suffix terms within Levenshtein distance `d` of `query`.
    /// Searches both SI=0 and SI>0 entries (contains mode).
    /// Merges parents from both partitions by key.
    pub fn fuzzy_walk(&self, query: &str, distance: u8) -> Vec<(String, Vec<ParentEntry>)> {
        use std::collections::HashMap;
        let mut merged: HashMap<String, Vec<ParentEntry>> = HashMap::new();
        for (key, parents) in self.fuzzy_walk_with_byte(super::builder::SI0_PREFIX, query, distance) {
            merged.entry(key).or_default().extend(parents);
        }
        for (key, parents) in self.fuzzy_walk_with_byte(super::builder::SI_REST_PREFIX, query, distance) {
            merged.entry(key).or_default().extend(parents);
        }
        let mut results: Vec<_> = merged.into_iter().collect();
        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }

    /// Fuzzy walk for startsWith: only SI=0 entries.
    pub fn fuzzy_walk_si0(&self, query: &str, distance: u8) -> Vec<(String, Vec<ParentEntry>)> {
        self.fuzzy_walk_with_byte(super::builder::SI0_PREFIX, query, distance)
    }

    /// Internal: fuzzy walk within a single partition.
    fn fuzzy_walk_with_byte(&self, prefix_byte: u8, query: &str, distance: u8) -> Vec<(String, Vec<ParentEntry>)> {
        // Build a prefixed query: the DFA must match prefix_byte + query
        let mut prefixed_query = String::with_capacity(1 + query.len());
        prefixed_query.push(prefix_byte as char);
        prefixed_query.push_str(query);

        let builder = LevenshteinAutomatonBuilder::new(distance, true);
        let dfa = builder.build_prefix_dfa(&prefixed_query);
        let automaton = SfxDfaWrapper(dfa);

        let mut results = Vec::new();
        let mut stream = self.fst.search(automaton).into_stream();

        while let Some((key, val)) = stream.next() {
            // Strip the prefix byte
            let term = String::from_utf8_lossy(&key[1..]).into_owned();
            let parents = self.decode_parents(val);
            results.push((term, parents));
        }

        results
    }

    /// Access the GapMap reader.
    pub fn gapmap(&self) -> &GapMapReader<'a> {
        &self.gapmap
    }

    /// Access the sibling table (if present). None for old .sfx files without sibling links.
    pub fn sibling_table(&self) -> Option<&SiblingTableReader<'a>> {
        self.sibling_table.as_ref()
    }

    /// Access the underlying FST map (for automaton searches).
    pub fn fst(&self) -> &Map<Vec<u8>> {
        &self.fst
    }

    /// Access the parent list data (for OutputTable lookups).
    pub fn parent_list_data(&self) -> &'a [u8] {
        self.parent_list_data
    }

    fn decode_parents(&self, val: u64) -> Vec<ParentEntry> {
        match decode_output(val) {
            ParentRef::Single { raw_ordinal, si, token_len } => {
                vec![ParentEntry { raw_ordinal, si, token_len }]
            }
            ParentRef::Multi { offset } => {
                let table = OutputTable::new(self.parent_list_data);
                let record = table.get(offset);
                decode_parent_entries(record)
            }
        }
    }

    /// Walk the FST byte-by-byte with the query, collecting all split candidates
    /// where a prefix of the query reaches the end of a parent token
    /// (si + prefix_len == token_len).
    ///
    /// Walks both SI=0 and SI>0 partitions. Returns candidates sorted by
    /// prefix_len descending (longest first).
    ///
    /// Cost: O(2L) node lookups where L = query.len(). No posting resolution.
    pub fn falling_walk(&self, query: &str) -> Vec<SplitCandidate> {
        let fst = self.fst.as_fst();
        let query_bytes = query.as_bytes();
        let mut candidates = Vec::new();

        for &partition in &[super::builder::SI0_PREFIX, super::builder::SI_REST_PREFIX] {
            let root = fst.root();
            // Follow partition prefix byte
            let Some(idx) = root.find_input(partition) else { continue };
            let trans = root.transition(idx);
            let mut output = lucivy_fst::raw::Output::zero().cat(trans.out);
            let mut node = fst.node(trans.addr);

            // Walk query bytes
            for (i, &byte) in query_bytes.iter().enumerate() {
                let Some(idx) = node.find_input(byte) else { break };
                let trans = node.transition(idx);
                output = output.cat(trans.out);
                node = fst.node(trans.addr);

                // If this node is a final state, we have a complete SFX key
                if node.is_final() {
                    let val = output.cat(node.final_output()).value();
                    let prefix_len = i + 1;
                    let parents = self.decode_parents(val);

                    for parent in parents {
                        // Does this prefix reach the END of the parent token?
                        if parent.si as usize + prefix_len == parent.token_len as usize {
                            candidates.push(SplitCandidate {
                                prefix_len,
                                parent,
                            });
                        }
                    }
                }
            }
        }

        // Sort by prefix_len descending (longest split first = most selective)
        candidates.sort_by(|a, b| b.prefix_len.cmp(&a.prefix_len));
        candidates
    }

    /// Fuzzy falling walk: like falling_walk but uses a Levenshtein DFA to
    /// tolerate edit distance in the query prefix.
    ///
    /// Uses DFS through the FST guided by the Levenshtein automaton.
    /// At each final FST node where the DFA has matched a prefix of the query
    /// (max_prefix_len > 0), checks if si + prefix_len == token_len.
    ///
    /// Falls back to exact falling_walk when distance == 0.
    pub fn fuzzy_falling_walk(&self, query: &str, distance: u8) -> Vec<SplitCandidate> {
        if distance == 0 {
            return self.falling_walk(query);
        }

        let Ok(lev) = LevAutomaton::new(query, distance as u32) else {
            return Vec::new(); // query too long for DFA construction
        };

        let fst = self.fst.as_fst();
        let mut candidates = Vec::new();

        for &partition in &[super::builder::SI0_PREFIX, super::builder::SI_REST_PREFIX] {
            let root = fst.root();
            let Some(idx) = root.find_input(partition) else { continue };
            let trans = root.transition(idx);

            // DFS stack: (fst_node, fst_output, lev_dfa_state)
            let initial_output = lucivy_fst::raw::Output::zero().cat(trans.out);
            let initial_lev_state = {
                // The partition byte is not part of the query — skip it in the DFA.
                // We start the DFA fresh after consuming the partition prefix.

                lev.start()
            };

            let mut stack: Vec<(lucivy_fst::raw::CompiledAddr, lucivy_fst::raw::Output, Option<usize>)> = Vec::new();
            stack.push((trans.addr, initial_output, initial_lev_state));

            while let Some((addr, output, lev_state)) = stack.pop() {
                let node = fst.node(addr);

                // Check: final FST node + DFA has matched a prefix?
                if node.is_final() {
                    let prefix_len = lev.max_prefix_len(&lev_state);
                    if prefix_len > 0 {
                        let val = output.cat(node.final_output()).value();
                        let parents = self.decode_parents(val);
                        for parent in parents {
                            if parent.si as usize + prefix_len == parent.token_len as usize {
                                candidates.push(SplitCandidate {
                                    prefix_len,
                                    parent,
                                });
                            }
                        }
                    }
                }

                // Pruning: can the DFA still match?

                if !lev.can_match(&lev_state) { continue; }

                // Explore all FST transitions
                for t in node.transitions() {
                    let next_lev = lev.accept(&lev_state, t.inp);
                    let next_output = output.cat(t.out);
                    stack.push((t.addr, next_output, next_lev));
                }
            }
        }

        candidates.sort_by(|a, b| b.prefix_len.cmp(&a.prefix_len));
        candidates.dedup_by(|a, b| a.prefix_len == b.prefix_len && a.parent.raw_ordinal == b.parent.raw_ordinal);
        candidates
    }
}

// ─── SFX Postings Reader ───────────────────────────────────────────────────

/// A posting entry from the .sfxpost file.
#[derive(Debug, Clone, PartialEq)]
pub struct SfxPostingEntry {
    /// Document ID containing this posting.
    pub doc_id: u32,
    /// Token position within the document.
    pub token_index: u32,
    /// Start byte offset of the token in the original text.
    pub byte_from: u32,
    /// End byte offset (exclusive) of the token in the original text.
    pub byte_to: u32,
}

/// Reads a .sfxpost file: per-ordinal posting entries.
/// Format: [num_terms: u32] [offsets: u32 × (num_terms+1)] [entries: VInt packed]
/// Each entry: doc_id(VInt) + token_index(VInt) + byte_from(VInt) + byte_to(VInt)
pub struct SfxPostingsReader<'a> {
    num_terms: u32,
    offsets: &'a [u8],
    entry_data: &'a [u8],
}

impl<'a> SfxPostingsReader<'a> {
    /// Open a `.sfxpost` file from raw bytes.
    pub fn open(data: &'a [u8]) -> Result<Self, SfxError> {
        if data.len() < 4 {
            return Err(SfxError::InvalidMagic);
        }
        let num_terms = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let offsets_size = (num_terms as usize + 1) * 4;
        if data.len() < 4 + offsets_size {
            return Err(SfxError::InvalidMagic);
        }
        let offsets = &data[4..4 + offsets_size];
        let entry_data = &data[4 + offsets_size..];
        Ok(Self { num_terms, offsets, entry_data })
    }

    /// Number of unique terms.
    pub fn num_terms(&self) -> u32 {
        self.num_terms
    }

    /// Get all posting entries for a given ordinal, sorted by (doc_id, token_index).
    pub fn entries(&self, ordinal: u32) -> Vec<SfxPostingEntry> {
        if ordinal >= self.num_terms {
            return Vec::new();
        }
        let off_start = self.read_offset(ordinal) as usize;
        let off_end = self.read_offset(ordinal + 1) as usize;
        if off_start >= off_end || off_start >= self.entry_data.len() {
            return Vec::new();
        }
        let slice = &self.entry_data[off_start..off_end.min(self.entry_data.len())];
        decode_posting_entries(slice)
    }

    /// Get the doc_freq (number of unique docs) for a given ordinal.
    pub fn doc_freq(&self, ordinal: u32) -> u32 {
        let entries = self.entries(ordinal);
        let mut count = 0u32;
        let mut prev_doc = u32::MAX;
        for e in &entries {
            if e.doc_id != prev_doc {
                count += 1;
                prev_doc = e.doc_id;
            }
        }
        count
    }

    fn read_offset(&self, idx: u32) -> u32 {
        let pos = idx as usize * 4;
        u32::from_le_bytes(self.offsets[pos..pos + 4].try_into().unwrap())
    }
}

fn decode_posting_entries(data: &[u8]) -> Vec<SfxPostingEntry> {
    let mut result = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let (doc_id, n) = decode_vint(&data[pos..]);
        pos += n;
        if pos >= data.len() { break; }
        let (token_index, n) = decode_vint(&data[pos..]);
        pos += n;
        if pos >= data.len() { break; }
        let (byte_from, n) = decode_vint(&data[pos..]);
        pos += n;
        if pos >= data.len() { break; }
        let (byte_to, n) = decode_vint(&data[pos..]);
        pos += n;
        result.push(SfxPostingEntry { doc_id, token_index, byte_from, byte_to });
    }
    result
}

/// Decode a single VInt. Returns (value, bytes_consumed).
fn decode_vint(data: &[u8]) -> (u32, usize) {
    let mut val = 0u32;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        val |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (val, i + 1);
        }
        shift += 7;
    }
    (val, data.len())
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

/// Errors that can occur when reading a `.sfx` or `.sfxpost` file.
#[derive(Debug)]
pub enum SfxError {
    /// The file header does not contain valid magic bytes.
    InvalidMagic,
    /// The file version is not supported by this reader.
    UnsupportedVersion(u8),
    /// The FST data is corrupted or invalid.
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

        let num_tokens = 4u32; // import, rag3db, from, core
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
            num_tokens,
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
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 2, token_len: 6 });

        // Resolve "rag3db" → parent "rag3db" (ordinal=1), SI=0
        let parents = reader.resolve_suffix("rag3db");
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0], ParentEntry { raw_ordinal: 1, si: 0, token_len: 6 });

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
        assert_eq!(results[0].1[0], ParentEntry { raw_ordinal: 1, si: 2, token_len: 6 });

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

        let num_tokens = 2u32; // core, hardcore
        let (fst_data, parent_list_data) = sfx_builder.build().unwrap();

        let mut gapmap_writer = GapMapWriter::new();
        gapmap_writer.add_empty_doc();
        let gapmap_data = gapmap_writer.serialize();

        let file_writer = SfxFileWriter::new(
            fst_data,
            parent_list_data,
            gapmap_data,
            1,
            num_tokens,
        );
        let bytes = file_writer.to_bytes();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "core" should have 2 parents
        let parents = reader.resolve_suffix("core");
        assert_eq!(parents.len(), 2);
        assert!(parents.contains(&ParentEntry { raw_ordinal: 0, si: 0, token_len: 4 }));
        assert!(parents.contains(&ParentEntry { raw_ordinal: 1, si: 4, token_len: 8 }));

        // Prefix walk "cor" → finds "core" with 2 parents
        let results = reader.prefix_walk("cor");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "core");
        assert_eq!(results[0].1.len(), 2);
    }

    #[test]
    fn test_falling_walk_cross_token() {
        // Tokens: "import" (ord 0), "rag3db" (ord 1), "from" (ord 2), "core" (ord 3)
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "rag3db" is 6 chars. Query "rag3dbfr" crosses "rag3db"|"from" boundary.
        // Falling walk should find: "rag3db" at SI=0, token_len=6, prefix_len=6
        // (si=0 + 6 == token_len=6 → reaches end of token)
        let candidates = reader.falling_walk("rag3dbfr");
        assert!(!candidates.is_empty(), "should find cross-token split for 'rag3dbfr'");

        let best = &candidates[0];
        assert_eq!(best.prefix_len, 6, "should split after 'rag3db' (6 bytes)");
        assert_eq!(best.parent.raw_ordinal, 1, "should be ordinal 1 (rag3db)");
        assert_eq!(best.parent.si, 0);
        assert_eq!(best.parent.token_len, 6);
    }

    #[test]
    fn test_falling_walk_suffix_cross_token() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "3dbfr" → "3db" is a suffix of "rag3db" at SI=3.
        // si(3) + prefix_len(3) = 6 = token_len(6) → reaches end.
        let candidates = reader.falling_walk("3dbfr");
        assert!(!candidates.is_empty(), "should find cross-token split for '3dbfr'");

        let found = candidates.iter().find(|c| c.prefix_len == 3);
        assert!(found.is_some(), "should have split at prefix_len=3 ('3db')");
        let c = found.unwrap();
        assert_eq!(c.parent.si, 3);
        assert_eq!(c.parent.token_len, 6);
    }

    #[test]
    fn test_falling_walk_no_cross_token() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "import" is fully within one token → no split candidate
        // (si=0, prefix_len=6, token_len=6 → si+6==6 → this IS a boundary,
        //  but we'd use single-token search first, not falling_walk)
        // Actually "importx" → "import" at SI=0, si+6==6 → valid split!
        // The falling walk just collects candidates, it doesn't know about single-token.
        let candidates = reader.falling_walk("importx");
        assert!(!candidates.is_empty(), "should find split for 'importx'");
        assert_eq!(candidates[0].prefix_len, 6);
    }

    #[test]
    fn test_falling_walk_nonexistent() {
        let bytes = build_test_sfx();
        let reader = SfxFileReader::open(&bytes).unwrap();

        // "zzzzz" doesn't match any suffix
        let candidates = reader.falling_walk("zzzzz");
        assert!(candidates.is_empty());
    }
}
