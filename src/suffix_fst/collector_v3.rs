//! SFX Collector v3 — overlap-aware token collection for indexation.
//!
//! Differences from v2:
//! - Tokens are extended with 2-byte overlap from the next token
//! - No GapMap, SepMap, or sibling table (separators are in the tokens)
//! - Tracks word_id and is_word_start per token via ChunkMeta
//! - Interns extended token texts (e.g., "mutex_lo" not "mutex_")

use std::collections::HashMap;

use crate::tokenizer::equal_chunk::{is_content_char, segment_and_chunk, ChunkMeta, DEFAULT_MAX_TOKEN};

/// Default overlap size in bytes.
pub const DEFAULT_OVERLAP: usize = 2;

/// Extract the leading content-char prefix from a token text.
///
/// Scans from the start and stops at the first separator (non-content) char.
/// Works correctly for both regular chunks ("mutex_lo" → "mutex") and
/// word-stripped entries ("mutexlo" → "mutexlo") since word-stripped texts
/// have no sep bytes embedded.
pub fn extract_content_prefix(text: &str) -> String {
    text.chars()
        .take_while(|c| is_content_char(*c))
        .collect()
}

/// A captured token with its metadata and overlap info.
#[derive(Debug, Clone)]
struct TokenCaptureV3 {
    /// Interned ordinal of the EXTENDED token (content + sep + overlap).
    intern_id: u32,
    /// Byte offset in original text where this chunk starts.
    offset_from: usize,
    /// Byte offset in original text where this chunk ends (exclusive, before overlap).
    offset_to: usize,
    /// Chunk metadata from the tokenizer.
    meta: ChunkMeta,
    /// Number of overlap bytes appended from the next token.
    overlap_len: u8,
    /// own_len = content_len + sep_len (the chunk's own bytes, without overlap).
    own_len: u16,
}

/// Collected data for one value within a document.
struct ValueDataV3 {
    ti_start: u32,
    num_tokens: u32,
}

/// V3 collector: gathers tokens with overlap for SFX indexation.
///
/// Usage:
/// ```ignore
/// let mut collector = SfxCollectorV3::new();
/// collector.begin_doc();
/// collector.add_value("pthread_mutex_lock");
/// collector.end_doc();
/// let data = collector.into_data();
/// ```
pub struct SfxCollectorV3 {
    // Interned extended tokens: each unique extended text stored once.
    token_intern: HashMap<String, u32>,
    token_texts: Vec<String>,
    // Posting entries indexed by interned ordinal: (doc_id, ti, byte_from, byte_to).
    token_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    // Metadata per interned ordinal (from first occurrence).
    token_meta: Vec<TokenMetaV3>,
    // Word-level stripped entries, built during add_value().
    word_stripped_entries: Vec<WordStrippedEntry>,

    // Word map: structural verification for cross-token chains.
    // Interned segments: segment text (content+sep, lowered) → global word_id.
    word_intern: HashMap<String, u32>,
    // Per intern ordinal: (global_word_id, chunk_index, total_chunks).
    // Populated during add_value, consumed by into_data.
    chunk_to_word: Vec<Vec<(u32, u8, u8)>>,
    // Observed next-word pairs: (prev_word_id, next_word_id).
    next_word_pairs: Vec<(u32, u32)>,
    // Sibling pairs: (intern_ord_a, intern_ord_b, content_len_a) for consecutive chunks and words.
    // content_len_a = content bytes of ordinal A (used by sibling DFS to know how much
    // of the query the first token consumes, excluding overlap).
    // Collected during add_value, remapped to final ordinals in into_data.
    sibling_pairs: Vec<(u32, u32, u16)>,
    // Per-doc word position map: (doc_id, position) → word_id_within_doc.
    word_pos_map: crate::suffix_fst::word_pos_map::WordPosMapWriter,

    // Per-document state
    doc_values: Vec<ValueDataV3>,
    doc_active: bool,
    current_doc_id: u32,
    current_value_ti_start: u32,
    /// Per-doc word counter for word_pos_map (offset per value).
    current_doc_word_offset: u32,

    // Config
    max_token: usize,
    overlap: usize,
    min_suffix_len: usize,
}

/// Metadata stored per unique extended token.
#[derive(Debug, Clone)]
pub struct TokenMetaV3 {
    pub own_len: u16,
    pub sep_len: u8,
    pub overlap_len: u8,
    pub is_word_start: bool,
    pub word_id: usize,
    /// Content-aware overlap for stripped partition: first 2 bytes of the next
    /// CONTENT token (skipping pure-sep tokens). None if same as normal overlap
    /// or if the token has no trailing sep.
    pub content_overlap: Option<String>,
    /// True if this token was interned for a word-stripped entry (partition 0x02 only).
    /// Excluded from the build loop for partitions 0x00/0x01.
    pub is_word_stripped: bool,
}

impl Default for SfxCollectorV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl SfxCollectorV3 {
    pub fn new() -> Self {
        let min = std::env::var("LUCIVY_MIN_SUFFIX_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        Self {
            token_intern: HashMap::new(),
            token_texts: Vec::new(),
            token_postings: Vec::new(),
            token_meta: Vec::new(),
            word_stripped_entries: Vec::new(),
            word_intern: HashMap::new(),
            chunk_to_word: Vec::new(),
            next_word_pairs: Vec::new(),
            sibling_pairs: Vec::new(), // (intern_a, intern_b, content_len_a)
            word_pos_map: crate::suffix_fst::word_pos_map::WordPosMapWriter::new(),
            doc_values: Vec::new(),
            doc_active: false,
            current_doc_id: 0,
            current_value_ti_start: 0,
            current_doc_word_offset: 0,
            max_token: DEFAULT_MAX_TOKEN,
            overlap: DEFAULT_OVERLAP,
            min_suffix_len: min,
        }
    }

    pub fn with_config(max_token: usize, overlap: usize, min_suffix_len: usize) -> Self {
        Self {
            max_token,
            overlap,
            min_suffix_len,
            ..Self::new()
        }
    }

    pub fn begin_doc(&mut self) {
        self.doc_values.clear();
        self.doc_active = true;
        self.current_value_ti_start = 0;
        self.current_doc_word_offset = 0;
    }

    /// Tokenize and add a complete value string.
    ///
    /// Internally: segments the text with EqualChunkTokenizer, computes overlap
    /// between adjacent chunks, and interns the extended tokens.
    pub fn add_value(&mut self, text: &str) {
        let chunks = segment_and_chunk(text, self.max_token);
        if chunks.is_empty() {
            self.doc_values.push(ValueDataV3 {
                ti_start: self.current_value_ti_start,
                num_tokens: 0,
            });
            self.current_value_ti_start += 1; // value boundary gap
            return;
        }

        let num_chunks = chunks.len();
        // Track byte offsets in original text
        let mut offset = 0usize;
        // Per-chunk posting info: (doc_id, ti, byte_from, byte_to) for word-stripped
        let mut chunk_posting_info: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(num_chunks);
        // Per-chunk intern ids for word map building
        let mut chunk_intern_ids: Vec<u32> = Vec::with_capacity(num_chunks);

        for i in 0..num_chunks {
            let (ref chunk_text, ref meta) = chunks[i];
            let chunk_len = chunk_text.len(); // own_len = content + sep

            // Compute normal overlap: first `overlap` bytes of TI+1
            let overlap_bytes: &str = if i + 1 < num_chunks {
                let next_text = &chunks[i + 1].0;
                let ov_len = self.overlap.min(next_text.len());
                let mut end = ov_len;
                while end > 0 && !next_text.is_char_boundary(end) {
                    end -= 1;
                }
                &next_text[..end]
            } else {
                ""
            };
            let overlap_len = overlap_bytes.len() as u8;

            // Compute content-aware overlap: skip pure-sep chunks, take from
            // the next chunk that has alphanumeric content. Used by partition 0x02.
            let content_overlap: Option<String> = if meta.sep_len > 0 {
                let mut co = String::new();
                for j in (i + 1)..num_chunks {
                    let (ref next_text, ref next_meta) = chunks[j];
                    if next_meta.content_len > 0 {
                        // Found content — take first `overlap` bytes
                        let ov_len = self.overlap.min(next_text.len());
                        let mut end = ov_len;
                        while end > 0 && !next_text.is_char_boundary(end) {
                            end -= 1;
                        }
                        co = next_text[..end].to_string();
                        break;
                    }
                    // Pure-sep chunk — skip
                }
                if co.is_empty() && overlap_bytes.is_empty() {
                    None // No content overlap available
                } else if co == overlap_bytes {
                    None // Same as normal overlap, no need for separate
                } else {
                    Some(co)
                }
            } else {
                None // No sep in this token, no need for content overlap
            };

            // Build extended token: chunk + normal overlap
            let extended = if overlap_len > 0 {
                format!("{chunk_text}{overlap_bytes}")
            } else {
                chunk_text.clone()
            };

            let own_len = chunk_len as u16;
            let ti = self.current_value_ti_start + i as u32;

            // Intern the extended token
            let intern_id = self.intern_extended(&extended, TokenMetaV3 {
                own_len,
                sep_len: meta.sep_len as u8,
                overlap_len,
                is_word_start: meta.is_word_start,
                word_id: meta.word_id,
                content_overlap: content_overlap.clone(),
                is_word_stripped: false,
            });

            // Add posting
            let byte_from = offset as u32;
            let byte_to = (offset + meta.content_len + meta.sep_len) as u32;
            self.token_postings[intern_id as usize].push((
                self.current_doc_id, ti, byte_from, byte_to,
            ));

            // ── DIAG: trace collector for target word ──
            if let Ok(target) = std::env::var("V3_DIAG_COLLECTOR") {
                if extended.to_lowercase().contains(&target) {
                    eprintln!("[COLLECTOR] add_value doc={} ti={} chunk[{}] text={:?} extended={:?} intern_id={} own_len={} sep={} ovl={} ws={} byte=[{}..{}] postings_count={}",
                        self.current_doc_id, ti, i, chunk_text, extended,
                        intern_id, own_len, meta.sep_len, overlap_len,
                        meta.is_word_start, byte_from, byte_to,
                        self.token_postings[intern_id as usize].len(),
                    );
                }
            }
            chunk_posting_info.push((self.current_doc_id, ti, byte_from, byte_to));
            chunk_intern_ids.push(intern_id);

            // Record word position: per-doc word_id = local word_id + offset
            let doc_word_id = meta.word_id as u32 + self.current_doc_word_offset;
            self.word_pos_map.add(self.current_doc_id, ti, doc_word_id);

            offset += chunk_len;
        }

        // Collect chunk sibling pairs: consecutive chunks in the same value
        // content_len = destination chunk's content length (used by DFS)
        for w in chunk_intern_ids.windows(2) {
            let meta = &self.token_meta[w[1] as usize];
            let content_len = (meta.own_len as u16).saturating_sub(meta.sep_len as u16);
            self.sibling_pairs.push((w[0], w[1], content_len));
        }

        // Update word offset for next value in same doc
        let max_local_word_id = chunks.iter().map(|(_, m)| m.word_id).max().unwrap_or(0);
        self.current_doc_word_offset += max_local_word_id as u32 + 1;

        // Build word map: group chunks by word_id, intern segments, record mappings.
        {
            let mut segments: std::collections::BTreeMap<usize, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (i, (_, meta)) in chunks.iter().enumerate() {
                segments.entry(meta.word_id).or_default().push(i);
            }

            let seg_ids: Vec<usize> = segments.keys().copied().collect();
            let mut prev_global_wid: Option<u32> = None;

            for &local_wid in &seg_ids {
                let chunk_idxs = &segments[&local_wid];
                // Reconstruct segment text (content+sep) by concatenating chunks
                let seg_text: String = chunk_idxs.iter()
                    .map(|&i| chunks[i].0.to_lowercase())
                    .collect();

                // Intern globally
                let next_id = self.word_intern.len() as u32;
                let global_wid = *self.word_intern.entry(seg_text).or_insert(next_id);

                let total = chunk_idxs.len() as u8;
                for (ci, &chunk_i) in chunk_idxs.iter().enumerate() {
                    let iid = chunk_intern_ids[chunk_i];
                    // Ensure chunk_to_word has enough capacity
                    while self.chunk_to_word.len() <= iid as usize {
                        self.chunk_to_word.push(Vec::new());
                    }
                    let entry = (global_wid, ci as u8, total);
                    if !self.chunk_to_word[iid as usize].contains(&entry) {
                        self.chunk_to_word[iid as usize].push(entry);
                    }
                }

                // Record next-word pair
                if let Some(prev) = prev_global_wid {
                    let pair = (prev, global_wid);
                    if !self.next_word_pairs.contains(&pair) {
                        self.next_word_pairs.push(pair);
                    }
                }
                prev_global_wid = Some(global_wid);
            }
        }

        // Build word-level stripped entries from this value's chunks.
        // Group by word_id, concatenate content, find content_overlap to next word.
        {
            let mut words_in_value: std::collections::BTreeMap<usize, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (i, (_, meta)) in chunks.iter().enumerate() {
                words_in_value.entry(meta.word_id).or_default().push(i);
            }

            let word_ids: Vec<usize> = words_in_value.keys().copied().collect();
            let mut ws_intern_sequence: Vec<(u32, u16)> = Vec::new(); // (intern_ord, content_len)
            for (wi, &word_id) in word_ids.iter().enumerate() {
                let chunk_idxs = &words_in_value[&word_id];

                // Concatenate content bytes
                let mut word_content = String::new();
                for &ci in chunk_idxs {
                    let (ref ct, ref cm) = chunks[ci];
                    let clen = cm.content_len.min(ct.len());
                    word_content.push_str(&ct[..clen]);
                }
                if word_content.is_empty() {
                    continue;
                }

                // Content overlap: first bytes of the next word's first content chunk
                let mut content_overlap = String::new();
                for next_wi in (wi + 1)..word_ids.len() {
                    let next_idxs = &words_in_value[&word_ids[next_wi]];
                    for &ci in next_idxs {
                        let (ref ct, ref cm) = chunks[ci];
                        if cm.content_len > 0 {
                            let ov_len = self.overlap.min(cm.content_len).min(ct.len());
                            let mut end = ov_len;
                            while end > 0 && !ct.is_char_boundary(end) {
                                end -= 1;
                            }
                            content_overlap = ct[..end].to_string();
                            break;
                        }
                    }
                    if !content_overlap.is_empty() { break; }
                }

                let first_ci = chunk_idxs[0];
                let last_ci = *chunk_idxs.last().unwrap();

                // Intern the word-stripped entry as its OWN token (not reusing the
                // first chunk's ordinal). The key is word_content + content_overlap,
                // which is unique per word and won't collide with chunk keys.
                // This ensures "include" and "inclusive" get distinct ordinals.
                let ws_extended = if !content_overlap.is_empty() {
                    format!("{word_content}{content_overlap}")
                } else {
                    word_content.clone()
                };
                let ws_own_len = (word_content.len() + chunks[last_ci].1.sep_len) as u16;
                let ws_intern = self.intern_extended(&ws_extended, TokenMetaV3 {
                    own_len: ws_own_len,
                    sep_len: chunks[last_ci].1.sep_len as u8,
                    overlap_len: content_overlap.len() as u8,
                    is_word_start: chunks[first_ci].1.is_word_start,
                    word_id: chunks[first_ci].1.word_id,
                    content_overlap: Some(content_overlap.clone()),
                    is_word_stripped: true,
                });
                // NO posting: word-stripped uses first chunk's ordinal (via intern_to_final).
                // The first chunk already has the posting. No duplicate.

                let max_token = crate::tokenizer::equal_chunk::DEFAULT_MAX_TOKEN;

                self.word_stripped_entries.push(WordStrippedEntry {
                    word_content: word_content.clone(),
                    content_overlap: content_overlap.clone(),
                    first_intern_ord: ws_intern,
                    first_chunk_intern_ord: chunk_intern_ids[first_ci],
                    last_chunk_intern_ord: chunk_intern_ids[last_ci],
                    first_own_len: ws_own_len,
                    last_sep_len: chunks[last_ci].1.sep_len as u8,
                    is_word_start: chunks[first_ci].1.is_word_start,
                    num_chunks: chunk_idxs.len() as u32,
                });
                ws_intern_sequence.push((ws_intern, word_content.len() as u16));

                // Tail entry for very long words only.
                // The main word-stripped entry indexes suffixes SI=0 to
                // SI=min(content_len, MAX_CHUNK_BYTES=256). For words ≤264 bytes,
                // all suffixes including the last MAX_TOKEN bytes are covered.
                // For longer words, the tail entry covers the last MAX_TOKEN bytes
                // so cross-sep queries near the word end can be found.
                //
                // Note: tail entries use the last chunk's ordinal, so byte ranges
                // from resolve_single_v3 may be approximate for very long words.
                // This is acceptable since the doc match is still correct.
                const MAX_SUFFIX_INDEX: usize = 256; // mirrors builder_v3::MAX_CHUNK_BYTES
                if word_content.len() > MAX_SUFFIX_INDEX + max_token {
                    let tail_start = word_content.len().saturating_sub(max_token);
                    let mut ts = tail_start;
                    while ts < word_content.len() && !word_content.is_char_boundary(ts) { ts += 1; }
                    let tail_content = word_content[ts..].to_string();

                    // Intern tail as its own token (same approach as main word-stripped)
                    let tail_extended = if !content_overlap.is_empty() {
                        format!("{tail_content}{content_overlap}")
                    } else {
                        tail_content.clone()
                    };
                    let tail_own_len = (tail_content.len() + chunks[last_ci].1.sep_len) as u16;
                    let tail_intern = self.intern_extended(&tail_extended, TokenMetaV3 {
                        own_len: tail_own_len,
                        sep_len: chunks[last_ci].1.sep_len as u8,
                        overlap_len: content_overlap.len() as u8,
                        is_word_start: false,
                        word_id: chunks[last_ci].1.word_id,
                        content_overlap: Some(content_overlap.clone()),
                        is_word_stripped: true,
                    });
                    // NO posting: tail uses last chunk's ordinal. No duplicate.

                    self.word_stripped_entries.push(WordStrippedEntry {
                        word_content: tail_content,
                        content_overlap,
                        first_intern_ord: tail_intern,
                        first_chunk_intern_ord: chunk_intern_ids[last_ci],
                        last_chunk_intern_ord: chunk_intern_ids[last_ci],
                        first_own_len: tail_own_len,
                        last_sep_len: chunks[last_ci].1.sep_len as u8,
                        is_word_start: false,
                        num_chunks: 1, // tail entry = single chunk
                    });
                }
            }

            // Collect word sibling pairs: consecutive words in the same value
            // content_len = destination word's content length (used by DFS to
            // know how many bytes of the sibling's text are content vs overlap)
            for w in ws_intern_sequence.windows(2) {
                self.sibling_pairs.push((w[0].0, w[1].0, w[1].1));
            }
        }

        self.doc_values.push(ValueDataV3 {
            ti_start: self.current_value_ti_start,
            num_tokens: num_chunks as u32,
        });
        // Advance: tokens + 1 boundary gap between values
        self.current_value_ti_start += num_chunks as u32 + 1;
    }

    pub fn end_doc(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;
    }

    pub fn end_doc_empty(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;
    }

    /// Intern an extended token, returning its ordinal.
    ///
    /// Chunk entries and word-stripped entries use separate namespaces even
    /// when their text is identical (e.g., "functional" from chunking
    /// "functionality" vs "functional" as a word-stripped entry).
    /// This prevents word-stripped meta from poisoning chunk entries,
    /// which would cause their postings to be skipped in into_data().
    fn intern_extended(&mut self, text: &str, meta: TokenMetaV3) -> u32 {
        // Separate namespace: word-stripped entries get a prefix to avoid collision
        let key = if meta.is_word_stripped {
            format!("\x00ws:{text}")
        } else {
            text.to_string()
        };
        if let Some(&ord) = self.token_intern.get(&key) {
            return ord;
        }
        let ord = self.token_texts.len() as u32;
        self.token_intern.insert(key, ord);
        self.token_texts.push(text.to_string()); // store actual text, not the prefixed key
        self.token_postings.push(Vec::new());
        self.token_meta.push(meta);
        ord
    }

    /// Extract data for DAG-based build.
    ///
    /// Groups extended tokens by content key (text[..own_len], without overlap).
    /// Tokens with the same content but different overlaps share one content ordinal
    /// and their postings are aggregated. This ensures cross-token chains resolve
    /// correctly across all documents regardless of overlap variation.
    pub fn into_data(self) -> SfxCollectorDataV3 {
        let num_tokens = self.token_texts.len();

        // Sort extended tokens alphabetically (for FST building).
        let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
        sorted_indices.sort_by(|&a, &b| {
            self.token_texts[a as usize].cmp(&self.token_texts[b as usize])
        });

        // --- Extended ordinals: each unique extended text → own ordinal + own postings ---
        // No grouping by text[..own_len]. Overlap variants are kept separate in
        // partitions 0x00/0x01, preventing FP from overlap mixing.
        // Word-stripped entries (partition 0x02) get their own ordinal with
        // aggregated postings from all overlap variants of their first chunk.

        // Build content_key → [intern_ords] for word-stripped aggregation.
        // Content key = text[..own_len] (content + sep, no overlap).
        let mut content_key_to_interns: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();
        for (io, text) in self.token_texts.iter().enumerate() {
            if self.token_meta[io].is_word_stripped { continue; }
            let mut ol = self.token_meta[io].own_len as usize;
            ol = ol.min(text.len());
            while ol > 0 && !text.is_char_boundary(ol) { ol -= 1; }
            content_key_to_interns.entry(text[..ol].to_string())
                .or_default().push(io as u32);
        }

        // Collect all ordinal entries into a BTreeMap for deterministic alphabetical order.
        // Chunk and word-stripped entries use SEPARATE keys (prefixed) to prevent
        // collision when their text is identical (e.g., "functional" as chunk
        // from "functionality" vs word-stripped "functional").
        struct OrdEntry {
            text: String, // actual text (no prefix)
            postings: Vec<(u32, u32, u32, u32)>,
            own_len: u16,
            intern_ords: Vec<u32>,
            is_word_stripped: bool,
        }
        let mut ord_map: std::collections::BTreeMap<String, OrdEntry> =
            std::collections::BTreeMap::new();

        // Add chunk entries (non-word-stripped): each gets its own ordinal
        let diag_target = std::env::var("V3_DIAG_COLLECTOR").ok();
        for &io in &sorted_indices {
            if self.token_meta[io as usize].is_word_stripped { continue; }
            let text = &self.token_texts[io as usize];
            let map_key = format!("C:{text}"); // "C:" = chunk namespace
            let postings_before = self.token_postings[io as usize].len();
            let entry = ord_map.entry(map_key).or_insert_with(|| OrdEntry {
                text: text.clone(),
                postings: Vec::new(),
                own_len: self.token_meta[io as usize].own_len,
                intern_ords: Vec::new(),
                is_word_stripped: false,
            });
            entry.postings.extend_from_slice(&self.token_postings[io as usize]);
            entry.intern_ords.push(io);

            if let Some(ref target) = diag_target {
                if text.to_lowercase().contains(target) {
                    eprintln!("[COLLECTOR into_data] intern_ord={} text={:?} postings_count={} ord_map_postings_after={} is_ws=false",
                        io, text, postings_before, entry.postings.len());
                }
            }
        }

        // Add word-stripped entries: own ordinal but NO chunk-level postings.
        // Their postings go into a separate word_postings (WordSfxPost format)
        // with position = last chunk (for cross-word adjacency).
        // They still need an entry in ord_map for ordinal assignment and FST building,
        // but with empty postings (the word sfxpost is separate).
        use crate::suffix_fst::word_sfxpost::WordPostingEntry;

        let mut ws_processed: std::collections::HashSet<u32> = std::collections::HashSet::new();
        // Deferred: word postings collected after ordinal assignment (need final_ord)
        struct DeferredWordPostings {
            intern_ord: u32,
            first_chunk_intern: u32,
            last_chunk_intern: u32,
            num_chunks: u32,
        }
        let mut deferred_ws: Vec<DeferredWordPostings> = Vec::new();

        for ws in &self.word_stripped_entries {
            if !ws_processed.insert(ws.first_intern_ord) { continue; }
            if !self.token_meta[ws.first_intern_ord as usize].is_word_stripped { continue; }

            let ws_text = &self.token_texts[ws.first_intern_ord as usize];
            let map_key = format!("W:{ws_text}"); // "W:" = word-stripped namespace

            // Add to ord_map with EMPTY postings — word postings go to WordSfxPost
            ord_map.entry(map_key).or_insert_with(|| OrdEntry {
                text: ws_text.clone(),
                postings: Vec::new(), // empty! word postings are separate
                own_len: ws.first_own_len,
                intern_ords: vec![ws.first_intern_ord],
                is_word_stripped: true,
            });

            deferred_ws.push(DeferredWordPostings {
                intern_ord: ws.first_intern_ord,
                first_chunk_intern: ws.first_chunk_intern_ord,
                last_chunk_intern: ws.last_chunk_intern_ord,
                num_chunks: ws.num_chunks,
            });
        }

        // Assign final ordinals in BTreeMap (alphabetical) order
        let mut intern_to_final = vec![0u32; num_tokens];
        let mut content_postings: Vec<Vec<(u32, u32, u32, u32)>> = Vec::new();
        // Use a Vec instead of BTreeSet to keep 1:1 correspondence with final ordinals.
        // When chunk and word-stripped have the same text, they get separate entries.
        let mut tokens_vec: Vec<String> = Vec::new();
        let mut own_lens: Vec<u16> = Vec::new();
        let mut final_ord = 0u32;

        for (_map_key, entry) in &ord_map {
            tokens_vec.push(entry.text.clone());
            let mut p = entry.postings.clone();
            let before_dedup = p.len();
            p.sort();
            p.dedup();
            content_postings.push(p.clone());
            own_lens.push(entry.own_len);
            for &io in &entry.intern_ords {
                intern_to_final[io as usize] = final_ord;
            }

            if let Some(ref target) = diag_target {
                if entry.text.to_lowercase().contains(target) {
                    eprintln!("[COLLECTOR final_ord] text={:?} final_ord={} intern_ords={:?} ws={} postings_before_dedup={} postings_after_dedup={} doc_ids={:?}",
                        entry.text, final_ord, entry.intern_ords, entry.is_word_stripped,
                        before_dedup, p.len(),
                        p.iter().map(|x| x.0).collect::<Vec<_>>());
                }
            }

            final_ord += 1;
        }

        // Build word postings (WordSfxPost) — now that ordinals are assigned.
        // Join first/last chunk postings by doc_id for hybrid position/byte_from.
        let content_key_of = |intern_ord: u32| -> String {
            let text = &self.token_texts[intern_ord as usize];
            let meta = &self.token_meta[intern_ord as usize];
            let mut ol = (meta.own_len as usize).min(text.len());
            while ol > 0 && !text.is_char_boundary(ol) { ol -= 1; }
            text[..ol].to_string()
        };

        let mut word_sfxpost_writer = crate::suffix_fst::word_sfxpost::WordSfxPostWriter::new(
            final_ord as usize,
        );
        for dws in &deferred_ws {
            let ws_final_ord = intern_to_final[dws.intern_ord as usize];
            let first_ckey = content_key_of(dws.first_chunk_intern);
            let last_ckey = content_key_of(dws.last_chunk_intern);

            if dws.first_chunk_intern == dws.last_chunk_intern {
                // Mono-chunk word: same position and bytes
                if let Some(variants) = content_key_to_interns.get(&first_ckey) {
                    for &vio in variants {
                        for &(doc_id, ti, bf, bt) in &self.token_postings[vio as usize] {
                            word_sfxpost_writer.add(ws_final_ord, WordPostingEntry {
                                doc_id, first_position: ti, last_position: ti,
                                byte_from: bf, byte_to: bt,
                            });
                        }
                    }
                }
            } else {
                // Multi-chunk: join by doc_id with exact chunk distance.
                //
                // content_key_to_interns maps content keys to ALL intern_ords with that
                // content — including chunks from different words (e.g., "funct" from
                // "function" and "functions"). Without distance check, this creates
                // a cross-product: first_chunk from word A × last_chunk from word B
                // → bogus entries spanning thousands of positions.
                //
                // Fix: last_pos - first_pos must equal num_chunks - 1 (exact distance
                // between first and last chunk of the same word occurrence).
                let expected_distance = dws.num_chunks - 1;

                // Collect ALL first_chunk postings per doc_id (multiple per doc)
                let mut first_by_doc: std::collections::HashMap<u32, Vec<(u32, u32)>> =
                    std::collections::HashMap::new();
                if let Some(variants) = content_key_to_interns.get(&first_ckey) {
                    for &vio in variants {
                        for &(doc_id, ti, bf, _bt) in &self.token_postings[vio as usize] {
                            first_by_doc.entry(doc_id).or_default().push((ti, bf));
                        }
                    }
                }
                for entries in first_by_doc.values_mut() {
                    entries.sort_by_key(|&(ti, _)| ti);
                    entries.dedup();
                }

                if let Some(variants) = content_key_to_interns.get(&last_ckey) {
                    for &vio in variants {
                        for &(doc_id, last_ti, _bf, bt) in &self.token_postings[vio as usize] {
                            if let Some(firsts) = first_by_doc.get(&doc_id) {
                                for &(first_ti, first_bf) in firsts {
                                    if last_ti >= first_ti && last_ti - first_ti == expected_distance {
                                        word_sfxpost_writer.add(ws_final_ord, WordPostingEntry {
                                            doc_id,
                                            first_position: first_ti,
                                            last_position: last_ti,
                                            byte_from: first_bf,
                                            byte_to: bt,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let word_sfxpost_data = word_sfxpost_writer.finish();

        // Build overlap sibling table (now trivial: each ordinal maps to itself,
        // but keep it for potential future use)
        let mut overlap_siblings = crate::suffix_fst::overlap_siblings::OverlapSiblingWriter::new(
            final_ord as usize,
        );
        for (_, entry) in &ord_map {
            let fo = intern_to_final[entry.intern_ords[0] as usize];
            for &io in &entry.intern_ords {
                overlap_siblings.add(fo, io);
            }
        }
        let overlap_siblings_data = overlap_siblings.serialize();

        // Build word maps: remap from intern ordinals to final ordinals.
        let mut chunk_word_writer = crate::suffix_fst::word_map::ChunkWordMapWriter::new(
            final_ord as usize,
        );
        for (intern_ord, entries) in self.chunk_to_word.iter().enumerate() {
            let fo = intern_to_final.get(intern_ord).copied().unwrap_or(0);
            for &(word_id, chunk_idx, total) in entries {
                chunk_word_writer.add(fo, word_id, chunk_idx, total);
            }
        }
        let chunk_word_map_data = chunk_word_writer.serialize();

        let num_words = self.word_intern.len();
        let mut next_word_writer = crate::suffix_fst::word_map::NextWordMapWriter::new(num_words);
        for &(prev, next) in &self.next_word_pairs {
            next_word_writer.add(prev, next);
        }
        let next_word_map_data = next_word_writer.serialize();
        let word_pos_map_data = self.word_pos_map.serialize();

        // Build sibling table v3: remap intern ordinals to final ordinals
        // Sibling table v3: gap_len field stores content_len of the source ordinal
        // (used by sibling_chain_dfs to know how many bytes of the query are consumed
        // by each sibling, excluding the overlap portion).
        let mut sibling_writer = crate::suffix_fst::sibling_table::SiblingTableWriter::new(final_ord);
        for &(a, b, content_len) in &self.sibling_pairs {
            let fa = intern_to_final[a as usize];
            let fb = intern_to_final[b as usize];
            sibling_writer.add(fa, fb, content_len);
        }
        let sibling_v3_data = sibling_writer.serialize();

        SfxCollectorDataV3 {
            sorted_indices,
            intern_to_final,
            token_texts: self.token_texts,
            token_meta: self.token_meta,
            tokens: tokens_vec,
            content_postings,
            own_lens,
            num_content_ords: final_ord as usize,
            num_docs: self.current_doc_id,
            min_suffix_len: self.min_suffix_len,
            overlap_siblings: overlap_siblings_data,
            word_stripped: self.word_stripped_entries,
            word_sfxpost: word_sfxpost_data,
            chunk_word_map: chunk_word_map_data,
            next_word_map: next_word_map_data,
            word_pos_map: word_pos_map_data,
            sibling_v3: sibling_v3_data,
        }
    }

    /// Number of documents processed so far.
    pub fn num_docs(&self) -> u32 {
        self.current_doc_id
    }

    /// Number of unique extended tokens interned.
    pub fn num_unique_tokens(&self) -> usize {
        self.token_texts.len()
    }
}

/// Word-level stripped entry for partition 0x02.
#[derive(Debug, Clone)]
pub struct WordStrippedEntry {
    /// Concatenated content bytes of all chunks in the word (no seps).
    pub word_content: String,
    /// Content overlap: first 2 bytes of the next word's content.
    pub content_overlap: String,
    /// Intern ordinal of the word-stripped token itself (partition 0x02 key).
    pub first_intern_ord: u32,
    /// Intern ordinal of the first CHUNK (partition 0x00/0x01).
    /// Used for byte_from in word-stripped postings.
    pub first_chunk_intern_ord: u32,
    /// Intern ordinal of the last CHUNK (partition 0x00/0x01).
    /// Used for position (token_index) and byte_to in word-stripped postings.
    /// For cross-word chain adjacency, the last chunk is adjacent to the seps/next word.
    pub last_chunk_intern_ord: u32,
    /// own_len of the first chunk.
    pub first_own_len: u16,
    /// sep_len of the last chunk (the one with trailing sep).
    pub last_sep_len: u8,
    /// is_word_start of the first chunk.
    pub is_word_start: bool,
    /// Number of chunks composing this word.
    pub num_chunks: u32,
}

/// Data extracted from SfxCollectorV3, ready for DAG-based build.
///
/// Extended ordinals: each unique extended text gets its own ordinal and postings.
/// Overlap variants are NOT grouped — they have separate ordinals.
/// Word-stripped entries get their own ordinal with aggregated postings from
/// all overlap variants of their first chunk.
pub struct SfxCollectorDataV3 {
    /// Intern ordinals sorted by extended text (for FST key iteration).
    pub sorted_indices: Vec<u32>,
    /// Maps intern ordinal → final ordinal.
    /// Each unique extended text has its own final ordinal (1:1 mapping).
    pub intern_to_final: Vec<u32>,
    /// Extended token texts (indexed by intern ordinal).
    pub token_texts: Vec<String>,
    /// Metadata per extended token (indexed by intern ordinal).
    pub token_meta: Vec<TokenMetaV3>,
    /// Token texts sorted alphabetically (BTreeSet order = final ordinal order).
    /// Contains extended texts for chunks and word-stripped texts for ws entries.
    /// Used by derived index builders (bytemap, freqmap, posmap).
    /// Ordered token texts, 1:1 with content_postings and own_lens by final ordinal.
    pub tokens: Vec<String>,
    /// Postings per final ordinal. Index = final ordinal.
    pub content_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    /// own_len per final ordinal (content+sep bytes, excludes overlap).
    /// Used by derived index builders to truncate extended texts.
    pub own_lens: Vec<u16>,
    /// Number of unique final ordinals (= content_postings.len()).
    pub num_content_ords: usize,
    pub num_docs: u32,
    pub min_suffix_len: usize,
    /// Overlap sibling table: content_ord → [intern_ords with same content].
    /// Serialized binary format, ready to store alongside the SFX file.
    pub overlap_siblings: Vec<u8>,
    /// Word-level stripped entries for partition 0x02.
    pub word_stripped: Vec<WordStrippedEntry>,
    /// Word-level sfxpost (serialized). Separate from chunk sfxpost because
    /// word postings have different semantics: position = last chunk,
    /// byte_from = first chunk start (entire word span).
    pub word_sfxpost: Vec<u8>,
    /// ChunkWordMap: content_ordinal → [(word_id, chunk_index, total_chunks)].
    /// Serialized binary format for structural chain verification.
    pub chunk_word_map: Vec<u8>,
    /// NextWordMap: word_id → [next_word_ids].
    /// Serialized binary format for inter-word chain verification.
    pub next_word_map: Vec<u8>,
    /// WordPosMap: (doc_id, position) → word_id_within_doc.
    /// Per-doc word assignment for exact chain verification.
    pub word_pos_map: Vec<u8>,
    /// Sibling table v3: ordinal → [next_ordinals] for both chunks and words.
    pub sibling_v3: Vec<u8>,
}

/// Build word-level stripped entries from token data.
/// Groups consecutive tokens by word_id, concatenates content bytes.
/// Public alias for use in merge.
pub fn build_word_stripped_pub(
    token_texts: &[String],
    token_meta: &[TokenMetaV3],
    overlap_size: usize,
) -> Vec<WordStrippedEntry> {
    build_word_stripped(token_texts, token_meta, overlap_size)
}

fn build_word_stripped(
    token_texts: &[String],
    token_meta: &[TokenMetaV3],
    overlap_size: usize,
) -> Vec<WordStrippedEntry> {
    if token_texts.is_empty() {
        return Vec::new();
    }

    // Group tokens by word_id (tokens are in intern order, same word_id = same word)
    let mut words: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for (idx, meta) in token_meta.iter().enumerate() {
        words.entry(meta.word_id).or_default().push(idx);
    }

    let mut entries = Vec::new();

    let word_ids: Vec<usize> = words.keys().copied().collect();

    for (wi, &word_id) in word_ids.iter().enumerate() {
        let chunk_indices = &words[&word_id];
        if chunk_indices.is_empty() {
            continue;
        }

        // Concatenate content bytes of all chunks in this word
        let mut word_content = String::new();
        for &idx in chunk_indices {
            let text = &token_texts[idx];
            let meta = &token_meta[idx];
            let content_len = meta.own_len as usize - meta.sep_len as usize;
            // Token text = content + sep + overlap. Take only content.
            let content_end = content_len.min(text.len());
            // Snap to char boundary
            let mut end = content_end;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            word_content.push_str(&text[..end.min(content_end)]);
        }

        if word_content.is_empty() {
            continue; // Pure-sep word, no content
        }

        // Find content_overlap: first `overlap_size` bytes of the next word's content
        let mut content_overlap = String::new();
        for next_wi in (wi + 1)..word_ids.len() {
            let next_word_id = word_ids[next_wi];
            let next_chunks = &words[&next_word_id];
            for &idx in next_chunks {
                let meta = &token_meta[idx];
                let content_len = meta.own_len as usize - meta.sep_len as usize;
                if content_len > 0 {
                    let text = &token_texts[idx];
                    let ov_len = overlap_size.min(content_len).min(text.len());
                    let mut end = ov_len;
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    content_overlap = text[..end].to_string();
                    break;
                }
            }
            if !content_overlap.is_empty() {
                break;
            }
        }

        let first_idx = chunk_indices[0];
        let last_idx = *chunk_indices.last().unwrap();

        entries.push(WordStrippedEntry {
            word_content,
            content_overlap,
            first_intern_ord: first_idx as u32,
            first_chunk_intern_ord: first_idx as u32, // in merge, intern = chunk
            last_chunk_intern_ord: last_idx as u32,
            first_own_len: token_meta[first_idx].own_len,
            last_sep_len: token_meta[last_idx].sep_len,
            is_word_start: token_meta[first_idx].is_word_start,
            num_chunks: chunk_indices.len() as u32,
        });
    }

    entries
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_collection() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        assert_eq!(c.num_docs(), 1);
        // "mutex_" (6) + overlap "lo" → "mutex_lo"
        // "lock" (4) no overlap → "lock"
        assert!(c.num_unique_tokens() >= 2);
    }

    #[test]
    fn test_extended_tokens_interned() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        // Check that "mutex_lo" (extended) is interned, not "mutex_"
        assert!(c.token_intern.contains_key("mutex_lo"), "should intern extended token");
        assert!(!c.token_intern.contains_key("mutex_"), "should NOT intern base token");
        assert!(c.token_intern.contains_key("lock"), "last token has no overlap");
    }

    #[test]
    fn test_overlap_bytes() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock_init");
        c.end_doc();

        // "mutex_" + overlap "lo" → "mutex_lo"
        assert!(c.token_intern.contains_key("mutex_lo"));
        // "lock_" + overlap "in" → "lock_in"
        assert!(c.token_intern.contains_key("lock_in"));
        // "init" → no overlap
        assert!(c.token_intern.contains_key("init"));
    }

    #[test]
    fn test_metadata_preserved() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        let ord = c.token_intern["mutex_lo"];
        let meta = &c.token_meta[ord as usize];
        assert_eq!(meta.own_len, 6); // "mutex_" = 6 bytes
        assert_eq!(meta.sep_len, 1); // "_"
        assert_eq!(meta.overlap_len, 2); // "lo"
        assert!(meta.is_word_start);
        assert_eq!(meta.word_id, 0);

        let ord = c.token_intern["lock"];
        let meta = &c.token_meta[ord as usize];
        assert_eq!(meta.own_len, 4);
        assert_eq!(meta.sep_len, 0);
        assert_eq!(meta.overlap_len, 0);
        assert!(meta.is_word_start);
        assert_eq!(meta.word_id, 1);
    }

    #[test]
    fn test_postings_correct() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        let ord = c.token_intern["mutex_lo"];
        let postings = &c.token_postings[ord as usize];
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].0, 0); // doc_id = 0
        assert_eq!(postings[0].1, 0); // ti = 0
        assert_eq!(postings[0].2, 0); // byte_from = 0
        assert_eq!(postings[0].3, 6); // byte_to = 6 ("mutex_")

        let ord = c.token_intern["lock"];
        let postings = &c.token_postings[ord as usize];
        assert_eq!(postings[0].1, 1); // ti = 1
        assert_eq!(postings[0].2, 6); // byte_from = 6
        assert_eq!(postings[0].3, 10); // byte_to = 10 ("lock")
    }

    #[test]
    fn test_multi_doc() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        c.begin_doc();
        c.add_value("mutex_core");
        c.end_doc();

        assert_eq!(c.num_docs(), 2);
        // "mutex_" followed by "lock" → "mutex_lo"
        // "mutex_" followed by "core" → "mutex_co"
        // These are DIFFERENT extended tokens → different ordinals
        assert!(c.token_intern.contains_key("mutex_lo"));
        assert!(c.token_intern.contains_key("mutex_co"));
    }

    #[test]
    fn test_multi_value() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("hello_world");
        c.add_value("foo_bar");
        c.end_doc();

        // Value 0: ti=0,1 → value boundary → ti=3,4
        let data = c.into_data();
        // Check postings for "foo_ba" (foo_ + overlap "ba")
        // Its ti should be 3 (after value boundary gap at ti=2)
        let intern_ord = data.token_texts.iter().position(|t| t == "foo_ba").unwrap();
        let content_ord = data.intern_to_final[intern_ord] as usize;
        let postings = &data.content_postings[content_ord];
        assert_eq!(postings[0].1, 3); // ti = 3 (after boundary)
    }

    #[test]
    fn test_into_data_sorted() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("zebra_alpha");
        c.end_doc();

        let data = c.into_data();
        let sorted: Vec<&String> = data.tokens.iter().collect();
        // BTreeSet is sorted
        for i in 1..sorted.len() {
            assert!(sorted[i - 1] < sorted[i], "tokens should be sorted");
        }
    }

    #[test]
    fn test_long_separator() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("a________b");
        c.end_doc();

        // "a________" (9 bytes > 8) → split into 2 chunks by equal division
        // "b" → 1 chunk
        assert!(c.num_unique_tokens() >= 2);
    }

    #[test]
    fn test_empty_value() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("");
        c.add_value("hello");
        c.end_doc();

        assert_eq!(c.num_docs(), 1);
    }

    #[test]
    fn test_same_extended_token_shared() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        // Same text → same extended tokens → shared ordinals
        let ord = c.token_intern["mutex_lo"];
        assert_eq!(c.token_postings[ord as usize].len(), 2); // 2 docs
    }

    #[test]
    fn test_build_with_builder_v3() {
        use crate::suffix_fst::builder_v3::*;

        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock_init");
        c.end_doc();

        let data = c.into_data();

        // Feed to builder v3
        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(data.min_suffix_len);
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let content_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(
                text,
                content_ord as u64,
                meta.own_len,
                meta.sep_len,
                meta.overlap_len,
                meta.is_word_start,
            );
        }

        let (fst_bytes, _output_table) = builder.build().unwrap();
        let fst = lucivy_fst::Map::new(fst_bytes).unwrap();
        assert!(fst.len() > 0, "FST should have entries");

        // Verify cross-boundary trigram "x_l" exists in the FST
        let key = [super::super::builder::SI_REST_PREFIX, b'x', b'_', b'l'];
        // Should be a prefix of some entry (x_lo, x_lock_in, etc.)
        use lucivy_fst::{IntoStreamer, Streamer};
        let mut lt = key.to_vec();
        *lt.last_mut().unwrap() += 1; // "x_m"
        let mut stream = fst.range().ge(&key[..]).lt(&lt[..]).into_stream();
        let mut found = false;
        while let Some((_k, _v)) = stream.next() {
            found = true;
            break;
        }
        assert!(found, "cross-boundary trigram 'x_l' should be in FST via overlap");
    }
}
