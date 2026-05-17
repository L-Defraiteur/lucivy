use super::{Token, TokenStream, Tokenizer};

/// Default maximum token size in bytes (content + trailing separator).
pub const DEFAULT_MAX_TOKEN: usize = 8;

/// Metadata attached to each chunk produced by [`EqualChunkTokenizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMeta {
    /// Number of content bytes in this chunk (non-separator).
    pub content_len: usize,
    /// Number of trailing separator bytes in this chunk.
    pub sep_len: usize,
    /// True if this chunk starts a new word (first chunk of a segment).
    pub is_word_start: bool,
    /// Word index (logical word this chunk belongs to).
    pub word_id: usize,
}

/// A tokenizer that splits text into segments (word + trailing separator),
/// then divides each segment into roughly equal-sized chunks.
///
/// The trailing separator bytes are part of the segment and get chunked
/// together with the content — no orphan separator tokens of 1 byte.
///
/// # Examples
///
/// ```rust
/// use ld_lucivy::tokenizer::equal_chunk::{EqualChunkTokenizer, segment_and_chunk, DEFAULT_MAX_TOKEN};
///
/// let chunks = segment_and_chunk("mutex_lock", DEFAULT_MAX_TOKEN);
/// assert_eq!(chunks.len(), 2);
/// assert_eq!(chunks[0].0, "mutex_");
/// assert_eq!(chunks[1].0, "lock");
/// ```
#[derive(Clone)]
pub struct EqualChunkTokenizer {
    max_token: usize,
    token: Token,
    meta: Vec<ChunkMeta>,
}

impl Default for EqualChunkTokenizer {
    fn default() -> Self {
        Self {
            max_token: DEFAULT_MAX_TOKEN,
            token: Token::default(),
            meta: Vec::new(),
        }
    }
}

impl EqualChunkTokenizer {
    pub fn with_max_token(max_token: usize) -> Self {
        assert!(max_token >= 2, "max_token must be at least 2");
        Self {
            max_token,
            ..Default::default()
        }
    }

    /// Access the chunk metadata for the last tokenized stream.
    /// Index matches the token position.
    pub fn meta(&self) -> &[ChunkMeta] {
        &self.meta
    }
}

/// Split text into segments, then chunk each segment. Returns (chunk_text, meta) pairs.
pub fn segment_and_chunk(text: &str, max_token: usize) -> Vec<(String, ChunkMeta)> {
    let segments = split_into_segments(text);
    let mut result = Vec::new();
    let mut word_id = 0usize;

    for seg in &segments {
        let chunks = equal_chunks(seg.content, seg.sep, max_token);
        for (i, (chunk_str, content_len, sep_len)) in chunks.into_iter().enumerate() {
            result.push((
                chunk_str,
                ChunkMeta {
                    content_len,
                    sep_len,
                    is_word_start: i == 0,
                    word_id,
                },
            ));
        }
        word_id += 1;
    }

    result
}

// ─── Segment splitting ─────────────────────────────────────────────────────

struct Segment<'a> {
    content: &'a str,
    sep: &'a str,
}

/// Content = anything that's not ASCII punctuation/whitespace/control.
/// Non-ASCII chars (emoji, CJK, accented letters, etc.) are always content.
/// Only ASCII non-alphanumeric chars (_, -, ., ::, spaces, tabs, etc.) are separators.
#[inline]
pub fn is_content_char(c: char) -> bool {
    !c.is_ascii() || c.is_ascii_alphanumeric()
}

/// Split text into segments: each segment is a word (content run) followed
/// by its trailing separator (non-content run until next word).
fn split_into_segments(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    // Skip leading separators (they become a segment with empty content)
    let first_content = text.char_indices()
        .find(|(_, c)| is_content_char(*c))
        .map(|(i, _)| i)
        .unwrap_or(len);

    if first_content > 0 {
        segments.push(Segment {
            content: "",
            sep: &text[..first_content],
        });
    }
    pos = first_content;

    while pos < len {
        // Find end of content
        let content_start = pos;
        let content_end = text[pos..].char_indices()
            .find(|(_, c)| !is_content_char(*c))
            .map(|(i, _)| pos + i)
            .unwrap_or(len);

        // Find end of separator
        let sep_start = content_end;
        let sep_end = text[content_end..].char_indices()
            .find(|(_, c)| is_content_char(*c))
            .map(|(i, _)| content_end + i)
            .unwrap_or(len);

        segments.push(Segment {
            content: &text[content_start..content_end],
            sep: &text[sep_start..sep_end],
        });

        pos = sep_end;
    }

    segments
}

// ─── Equal chunking ────────────────────────────────────────────────────────

/// Divide a segment (content + sep) into roughly equal chunks.
/// Returns Vec<(chunk_string, content_len_in_chunk, sep_len_in_chunk)>.
fn equal_chunks(content: &str, sep: &str, max_token: usize) -> Vec<(String, usize, usize)> {
    let combined = format!("{content}{sep}");
    let total = combined.len();

    if total == 0 {
        return Vec::new();
    }

    if total <= max_token {
        return vec![(
            combined,
            content.len(),
            sep.len(),
        )];
    }

    let num_chunks = (total + max_token - 1) / max_token;
    let base = total / num_chunks;
    let extra = total % num_chunks;

    let mut chunks = Vec::with_capacity(num_chunks);
    let mut offset = 0;

    for i in 0..num_chunks {
        let target = if i < extra { base + 1 } else { base };
        let mut end = offset + target;

        // Respect UTF-8 char boundaries
        while end < total && !combined.is_char_boundary(end) {
            end += 1;
        }
        if end > total {
            end = total;
        }

        let chunk_str = &combined[offset..end];
        let chunk_len = chunk_str.len();

        // Determine how much of this chunk is content vs separator.
        // Content occupies [0..content.len()) in the combined string.
        // Separator occupies [content.len()..total).
        let content_in_chunk = if offset >= content.len() {
            0
        } else if offset + chunk_len <= content.len() {
            chunk_len
        } else {
            content.len() - offset
        };
        let sep_in_chunk = chunk_len - content_in_chunk;

        chunks.push((chunk_str.to_string(), content_in_chunk, sep_in_chunk));
        offset = end;
    }

    chunks
}

// ─── TokenStream implementation ────────────────────────────────────────────

pub struct EqualChunkStream<'a> {
    chunks: Vec<(String, ChunkMeta)>,
    index: usize,
    token: &'a mut Token,
    meta: &'a mut Vec<ChunkMeta>,
}

impl Tokenizer for EqualChunkTokenizer {
    type TokenStream<'a> = EqualChunkStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> EqualChunkStream<'a> {
        self.token.reset();
        self.meta.clear();
        let chunks = segment_and_chunk(text, self.max_token);
        EqualChunkStream {
            chunks,
            index: 0,
            token: &mut self.token,
            meta: &mut self.meta,
        }
    }
}

impl TokenStream for EqualChunkStream<'_> {
    fn advance(&mut self) -> bool {
        if self.index >= self.chunks.len() {
            return false;
        }

        let (ref text, ref meta) = self.chunks[self.index];
        self.token.text.clear();
        self.token.text.push_str(text);
        self.token.position = self.index;
        // offset_from/offset_to are relative to the chunk sequence, not the original text.
        // For SFX usage these are recomputed by the collector from byte offsets.
        self.token.offset_from = 0;
        self.token.offset_to = text.len();
        self.meta.push(meta.clone());
        self.index += 1;
        true
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(text: &str, max_token: usize) -> Vec<(String, ChunkMeta)> {
        segment_and_chunk(text, max_token)
    }

    fn chunk_texts(text: &str, max_token: usize) -> Vec<String> {
        segment_and_chunk(text, max_token).into_iter().map(|(t, _)| t).collect()
    }

    // ── Basic cases ──

    #[test]
    fn test_simple_two_words() {
        let c = chunks("mutex_lock", 8);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, "mutex_");
        assert_eq!(c[0].1, ChunkMeta { content_len: 5, sep_len: 1, is_word_start: true, word_id: 0 });
        assert_eq!(c[1].0, "lock");
        assert_eq!(c[1].1, ChunkMeta { content_len: 4, sep_len: 0, is_word_start: true, word_id: 1 });
    }

    #[test]
    fn test_three_words() {
        let c = chunks("pthread_mutex_lock", 8);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].0, "pthread_");
        assert_eq!(c[0].1.content_len, 7);
        assert_eq!(c[0].1.sep_len, 1);
        assert_eq!(c[1].0, "mutex_");
        assert_eq!(c[1].1.content_len, 5);
        assert_eq!(c[1].1.sep_len, 1);
        assert_eq!(c[2].0, "lock");
        assert_eq!(c[2].1.content_len, 4);
        assert_eq!(c[2].1.sep_len, 0);
    }

    #[test]
    fn test_no_separator() {
        let c = chunks("lock", 8);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "lock");
        assert_eq!(c[0].1.sep_len, 0);
    }

    // ── Equal division ──

    #[test]
    fn test_long_word_equal_split() {
        // "getElementById" = 14 bytes > 8 → 2 chunks of (7, 7)
        let c = chunks("getElementById", 8);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0.len(), 7);
        assert_eq!(c[1].0.len(), 7);
        assert!(c[0].1.is_word_start);
        assert!(!c[1].1.is_word_start);
        assert_eq!(c[0].1.word_id, c[1].1.word_id);
    }

    #[test]
    fn test_long_word_three_chunks() {
        // "internationalization" = 20 bytes > 8 → 3 chunks: ceil(20/8)=3, 20/3 → (7,7,6)
        let c = chunks("internationalization", 8);
        assert_eq!(c.len(), 3);
        let total: usize = c.iter().map(|(t, _)| t.len()).sum();
        assert_eq!(total, 20);
        // All chunks should be within 1 byte of each other
        let max = c.iter().map(|(t, _)| t.len()).max().unwrap();
        let min = c.iter().map(|(t, _)| t.len()).min().unwrap();
        assert!(max - min <= 1);
    }

    // ── Separator handling ──

    #[test]
    fn test_long_separator() {
        // "mutex________" = 13 bytes > 8 → 2 chunks: (7, 6)
        let c = chunks("mutex________b", 8);
        // segment 1: "mutex________" (5 content + 8 sep = 13) → 2 chunks (7, 6)
        // segment 2: "b" → 1 chunk
        assert_eq!(c.len(), 3);
        let seg1_total: usize = c[..2].iter().map(|(t, _)| t.len()).sum();
        assert_eq!(seg1_total, 13);
        assert_eq!(c[2].0, "b");
    }

    #[test]
    fn test_double_colon_separator() {
        let c = chunks("Error::LucivyError", 8);
        // segment 1: "Error::" = 7 bytes ≤ 8 → 1 chunk
        // segment 2: "LucivyError" = 11 bytes > 8 → 2 chunks (6, 5)
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].0, "Error::");
        assert_eq!(c[0].1, ChunkMeta { content_len: 5, sep_len: 2, is_word_start: true, word_id: 0 });
    }

    #[test]
    fn test_leading_separator() {
        let c = chunks("__init", 8);
        // segment 0: content="" sep="__" → chunk "__"
        // segment 1: content="init" sep="" → chunk "init"
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, "__");
        assert_eq!(c[0].1.content_len, 0);
        assert_eq!(c[0].1.sep_len, 2);
        assert_eq!(c[1].0, "init");
        assert_eq!(c[1].1.content_len, 4);
    }

    // ── No orphans ──

    #[test]
    fn test_no_orphan_single_byte() {
        // 9 bytes → 2 chunks of (5, 4), NOT (8, 1)
        let c = chunk_texts("abcdefgh_", 8);
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|t| t.len() >= 2), "no orphan chunks: {:?}", c);
    }

    #[test]
    fn test_no_orphan_separator() {
        // "a_b" → segment "a_", segment "b". "a_" fits in 8.
        let c = chunks("a_b", 8);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, "a_");
        assert_eq!(c[1].0, "b");
    }

    // ── UTF-8 ──

    #[test]
    fn test_utf8_content() {
        let c = chunks("café_latte", 8);
        // "café_" = 6 bytes (é=2 bytes) → fits in 8
        assert_eq!(c[0].0, "café_");
        assert_eq!(c[0].1.content_len, 5); // c-a-f-é(2 bytes)
        assert_eq!(c[0].1.sep_len, 1);
    }

    #[test]
    fn test_utf8_split_respects_boundary() {
        // "naïveté" = 9 bytes (2 multi-byte chars) > 8
        // Must not split in the middle of a multi-byte char
        let c = chunks("naïveté", 8);
        for (text, _) in &c {
            assert!(text.is_char_boundary(0));
            assert!(text.is_char_boundary(text.len()));
            // Verify valid UTF-8
            let _ = text.as_str();
        }
    }

    // ── Edge cases ──

    #[test]
    fn test_empty() {
        let c = chunks("", 8);
        assert!(c.is_empty());
    }

    #[test]
    fn test_only_separators() {
        let c = chunks("____", 8);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "____");
        assert_eq!(c[0].1.content_len, 0);
        assert_eq!(c[0].1.sep_len, 4);
    }

    #[test]
    fn test_single_char() {
        let c = chunks("a", 8);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "a");
    }

    #[test]
    fn test_small_max_token() {
        // MAX_TOKEN=3, "mutex_lock"
        let c = chunks("mutex_lock", 3);
        // segment "mutex_" (6 bytes) → 2 chunks of 3: "mut", "ex_"
        // segment "lock" (4 bytes) → 2 chunks of 2: "lo", "ck"
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].0, "mut");
        assert_eq!(c[1].0, "ex_");
        assert_eq!(c[2].0, "lo");
        assert_eq!(c[3].0, "ck");
    }

    // ── Word IDs ──

    #[test]
    fn test_word_ids_consistent() {
        let c = chunks("pthread_mutex_lock", 8);
        assert_eq!(c[0].1.word_id, 0); // pthread_
        assert_eq!(c[1].1.word_id, 1); // mutex_
        assert_eq!(c[2].1.word_id, 2); // lock
    }

    #[test]
    fn test_word_ids_with_split() {
        let c = chunks("getElementById_init", 8);
        // "getElementById_" → 15 bytes → 2 chunks, both word_id=0
        // "init" → 1 chunk, word_id=1
        assert_eq!(c[0].1.word_id, 0);
        assert_eq!(c[1].1.word_id, 0);
        assert_eq!(c[2].1.word_id, 1);
    }

    // ── TokenStream ──

    #[test]
    fn test_token_stream() {
        let mut tok = EqualChunkTokenizer::with_max_token(8);
        let mut stream = tok.token_stream("mutex_lock");
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        assert_eq!(tokens, vec!["mutex_", "lock"]);
    }

    #[test]
    fn test_token_stream_positions() {
        let mut tok = EqualChunkTokenizer::with_max_token(8);
        let mut stream = tok.token_stream("pthread_mutex_lock");
        let mut positions = Vec::new();
        while stream.advance() {
            positions.push((stream.token().text.clone(), stream.token().position));
        }
        assert_eq!(positions[0], ("pthread_".into(), 0));
        assert_eq!(positions[1], ("mutex_".into(), 1));
        assert_eq!(positions[2], ("lock".into(), 2));
    }
}
