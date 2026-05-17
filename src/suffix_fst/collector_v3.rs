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

    // Per-document state
    doc_values: Vec<ValueDataV3>,
    doc_active: bool,
    current_doc_id: u32,
    current_value_ti_start: u32,

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
            doc_values: Vec::new(),
            doc_active: false,
            current_doc_id: 0,
            current_value_ti_start: 0,
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
            chunk_posting_info.push((self.current_doc_id, ti, byte_from, byte_to));

            offset += chunk_len;
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
                // Add posting for this word-stripped ordinal (from first chunk's position)
                let (doc_id, ti, bf, bt) = chunk_posting_info[first_ci];
                self.token_postings[ws_intern as usize].push((doc_id, ti, bf, bt));

                let max_token = crate::tokenizer::equal_chunk::DEFAULT_MAX_TOKEN;

                self.word_stripped_entries.push(WordStrippedEntry {
                    word_content: word_content.clone(),
                    content_overlap: content_overlap.clone(),
                    first_intern_ord: ws_intern,
                    first_own_len: ws_own_len,
                    last_sep_len: chunks[last_ci].1.sep_len as u8,
                    is_word_start: chunks[first_ci].1.is_word_start,
                });

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
                    // Posting from last chunk's position
                    let (doc_id, _ti, _bf, bt) = chunk_posting_info[last_ci];
                    let last_ti = chunk_posting_info[last_ci].1;
                    let last_bf = chunk_posting_info[last_ci].2;
                    self.token_postings[tail_intern as usize].push((doc_id, last_ti, last_bf, bt));

                    self.word_stripped_entries.push(WordStrippedEntry {
                        word_content: tail_content,
                        content_overlap,
                        first_intern_ord: tail_intern,
                        first_own_len: tail_own_len,
                        last_sep_len: chunks[last_ci].1.sep_len as u8,
                        is_word_start: false,
                    });
                }
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
    fn intern_extended(&mut self, text: &str, meta: TokenMetaV3) -> u32 {
        if let Some(&ord) = self.token_intern.get(text) {
            return ord;
        }
        let ord = self.token_texts.len() as u32;
        self.token_intern.insert(text.to_string(), ord);
        self.token_texts.push(text.to_string());
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

        // Sort extended tokens alphabetically (for FST building — each unique
        // extended text becomes a separate FST key, but they may share ordinals).
        let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
        sorted_indices.sort_by(|&a, &b| {
            self.token_texts[a as usize].cmp(&self.token_texts[b as usize])
        });

        // Group intern ordinals by content key = leading content chars of text.
        // Scanned from the actual bytes (not from metadata) so it works for both
        // regular chunks ("mutex_lo" → "mutex") and word-stripped entries
        // ("mutexlo" → "mutexlo", all content chars).
        // All sep/overlap variants of the same content share one ordinal.
        let mut content_key_map: std::collections::BTreeMap<String, Vec<u32>> =
            std::collections::BTreeMap::new();
        for (intern_ord, text) in self.token_texts.iter().enumerate() {
            let content_key = extract_content_prefix(text);
            content_key_map.entry(content_key).or_default().push(intern_ord as u32);
        }

        // Assign content final ordinals (sorted alphabetically by content key).
        let mut intern_to_final = vec![0u32; num_tokens];
        let mut content_postings: Vec<Vec<(u32, u32, u32, u32)>> = Vec::new();
        let mut content_final_ord = 0u32;
        for (_content_key, intern_ords) in &content_key_map {
            // Aggregate postings from all overlap variants
            let mut agg: Vec<(u32, u32, u32, u32)> = Vec::new();
            for &io in intern_ords {
                agg.extend_from_slice(&self.token_postings[io as usize]);
            }
            // Sort postings for deterministic sfxpost
            agg.sort();
            agg.dedup();
            content_postings.push(agg);

            for &io in intern_ords {
                intern_to_final[io as usize] = content_final_ord;
            }
            content_final_ord += 1;
        }

        // Content keys as BTreeSet (sorted = ordinal order)
        let tokens: std::collections::BTreeSet<String> = content_key_map.keys().cloned().collect();

        // Build overlap sibling table: content_ord → [intern_ords]
        let mut overlap_siblings = crate::suffix_fst::overlap_siblings::OverlapSiblingWriter::new(
            content_final_ord as usize,
        );
        for (_content_key, intern_ords) in &content_key_map {
            let content_ord = intern_to_final[intern_ords[0] as usize];
            for &io in intern_ords {
                overlap_siblings.add(content_ord, io);
            }
        }
        let overlap_siblings_data = overlap_siblings.serialize();

        SfxCollectorDataV3 {
            sorted_indices,
            intern_to_final,
            token_texts: self.token_texts,
            token_meta: self.token_meta,
            tokens,
            content_postings,
            num_content_ords: content_final_ord as usize,
            num_docs: self.current_doc_id,
            min_suffix_len: self.min_suffix_len,
            overlap_siblings: overlap_siblings_data,
            word_stripped: self.word_stripped_entries,
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
    /// Ordinal of the first chunk of this word.
    pub first_intern_ord: u32,
    /// own_len of the first chunk.
    pub first_own_len: u16,
    /// sep_len of the last chunk (the one with trailing sep).
    pub last_sep_len: u8,
    /// is_word_start of the first chunk.
    pub is_word_start: bool,
}

/// Data extracted from SfxCollectorV3, ready for DAG-based build.
///
/// Extended tokens are sorted for FST building (sorted_indices + token_texts).
/// Postings are aggregated by content ordinal (content_postings).
/// `intern_to_final` maps each intern ordinal to its content final ordinal.
/// Data extracted from SfxCollectorV3, ready for DAG-based build.
///
/// Extended tokens are sorted for FST building (sorted_indices + token_texts).
/// Postings are aggregated by content ordinal (content_postings).
/// `intern_to_final` maps each intern ordinal to its content final ordinal.
pub struct SfxCollectorDataV3 {
    /// Intern ordinals sorted by extended text (for FST key iteration).
    pub sorted_indices: Vec<u32>,
    /// Maps intern ordinal → content final ordinal.
    /// Multiple extended variants (different overlaps) share the same content ordinal.
    pub intern_to_final: Vec<u32>,
    /// Extended token texts (indexed by intern ordinal).
    pub token_texts: Vec<String>,
    /// Metadata per extended token (indexed by intern ordinal).
    pub token_meta: Vec<TokenMetaV3>,
    /// Content keys sorted alphabetically (BTreeSet order = content ordinal order).
    /// Each key = text[..own_len] (content+sep, without overlap).
    /// Used by derived index builders (bytemap, freqmap, posmap).
    pub tokens: std::collections::BTreeSet<String>,
    /// Postings aggregated by content ordinal. Index = content final ordinal.
    pub content_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    /// Number of unique content ordinals (= content_postings.len()).
    pub num_content_ords: usize,
    pub num_docs: u32,
    pub min_suffix_len: usize,
    /// Overlap sibling table: content_ord → [intern_ords with same content].
    /// Serialized binary format, ready to store alongside the SFX file.
    pub overlap_siblings: Vec<u8>,
    /// Word-level stripped entries for partition 0x02.
    pub word_stripped: Vec<WordStrippedEntry>,
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
            first_own_len: token_meta[first_idx].own_len,
            last_sep_len: token_meta[last_idx].sep_len,
            is_word_start: token_meta[first_idx].is_word_start,
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
