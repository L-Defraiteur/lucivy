//! SFX Collector v3 — overlap-aware token collection for indexation.
//!
//! Differences from v2:
//! - Tokens are extended with 2-byte overlap from the next token
//! - No GapMap, SepMap, or sibling table (separators are in the tokens)
//! - Tracks word_id and is_word_start per token via ChunkMeta
//! - Interns extended token texts (e.g., "mutex_lo" not "mutex_")

use std::collections::HashMap;

use crate::tokenizer::equal_chunk::{segment_and_chunk, ChunkMeta, DEFAULT_MAX_TOKEN};

/// Default overlap size in bytes.
pub const DEFAULT_OVERLAP: usize = 2;

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
            });

            // Add posting
            let byte_from = offset as u32;
            let byte_to = (offset + meta.content_len + meta.sep_len) as u32;
            self.token_postings[intern_id as usize].push((
                self.current_doc_id, ti, byte_from, byte_to,
            ));

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
                // Get the intern_id of the first chunk for this word
                // We need the intern_id that was assigned during the loop above.
                // Reconstruct it by re-interning (it already exists, so returns existing ord).
                let first_chunk_text = &chunks[first_ci].0;
                let first_overlap: &str = if first_ci + 1 < num_chunks {
                    let next_text = &chunks[first_ci + 1].0;
                    let ov_len = self.overlap.min(next_text.len());
                    let mut end = ov_len;
                    while end > 0 && !next_text.is_char_boundary(end) { end -= 1; }
                    &next_text[..end]
                } else { "" };
                let first_extended = if !first_overlap.is_empty() {
                    format!("{first_chunk_text}{first_overlap}")
                } else {
                    first_chunk_text.clone()
                };
                let first_intern = *self.token_intern.get(&first_extended).unwrap_or(&0);

                let max_token = crate::tokenizer::equal_chunk::DEFAULT_MAX_TOKEN;

                self.word_stripped_entries.push(WordStrippedEntry {
                    word_content: word_content.clone(),
                    content_overlap: content_overlap.clone(),
                    first_intern_ord: first_intern,
                    first_own_len: chunks[first_ci].0.len() as u16,
                    last_sep_len: chunks[last_ci].1.sep_len as u8,
                    is_word_start: chunks[first_ci].1.is_word_start,
                });

                // Tail entry for long words: cover the last MAX_TOKEN bytes of word content
                // so cross-sep queries near the end of the word can be found.
                if word_content.len() > max_token {
                    let tail_start = word_content.len().saturating_sub(max_token);
                    // Snap to UTF-8 char boundary
                    let mut ts = tail_start;
                    while ts < word_content.len() && !word_content.is_char_boundary(ts) { ts += 1; }
                    let tail_content = word_content[ts..].to_string();

                    // Use the last chunk's ordinal for posting resolution
                    let last_chunk_text = &chunks[last_ci].0;
                    let last_overlap: &str = if last_ci + 1 < num_chunks {
                        let next_text = &chunks[last_ci + 1].0;
                        let ov_len = self.overlap.min(next_text.len());
                        let mut end = ov_len;
                        while end > 0 && !next_text.is_char_boundary(end) { end -= 1; }
                        &next_text[..end]
                    } else { "" };
                    let last_extended = if !last_overlap.is_empty() {
                        format!("{last_chunk_text}{last_overlap}")
                    } else {
                        last_chunk_text.clone()
                    };
                    let last_intern = *self.token_intern.get(&last_extended).unwrap_or(&0);

                    self.word_stripped_entries.push(WordStrippedEntry {
                        word_content: tail_content,
                        content_overlap,
                        first_intern_ord: last_intern,
                        first_own_len: chunks[last_ci].0.len() as u16,
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
    pub fn into_data(self) -> SfxCollectorDataV3 {
        let num_tokens = self.token_texts.len();

        // Sort tokens alphabetically → final ordinals
        let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
        sorted_indices.sort_by(|&a, &b| {
            self.token_texts[a as usize].cmp(&self.token_texts[b as usize])
        });

        let mut intern_to_final = vec![0u32; num_tokens];
        for (new_ord, &old_ord) in sorted_indices.iter().enumerate() {
            intern_to_final[old_ord as usize] = new_ord as u32;
        }

        let tokens: std::collections::BTreeSet<String> = sorted_indices.iter()
            .map(|&old_ord| self.token_texts[old_ord as usize].clone())
            .collect();

        SfxCollectorDataV3 {
            tokens,
            sorted_indices,
            intern_to_final,
            token_texts: self.token_texts,
            token_postings: self.token_postings,
            token_meta: self.token_meta,
            num_docs: self.current_doc_id,
            min_suffix_len: self.min_suffix_len,
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
pub struct SfxCollectorDataV3 {
    pub tokens: std::collections::BTreeSet<String>,
    pub sorted_indices: Vec<u32>,
    pub intern_to_final: Vec<u32>,
    pub token_texts: Vec<String>,
    pub token_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    pub token_meta: Vec<TokenMetaV3>,
    pub num_docs: u32,
    pub min_suffix_len: usize,
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
        let ord = data.token_texts.iter().position(|t| t == "foo_ba").unwrap();
        let postings = &data.token_postings[ord];
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
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            builder.add_token(
                text,
                final_ord as u64,
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
