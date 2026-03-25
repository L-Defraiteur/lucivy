use super::{Token, TokenFilter, TokenStream, Tokenizer};

const MIN_CHUNK_CHARS: usize = 4;
const MAX_CHUNK_BYTES: usize = 256;

/// A [`TokenFilter`] that splits tokens at camelCase boundaries and
/// letter↔digit transitions, then force-splits remaining long chunks.
///
/// After initial split, chunks shorter than 4 characters are merged
/// with the following chunk. The last chunk, if too short, is merged
/// back into the previous one.
///
/// Any chunk exceeding 256 bytes is force-split at UTF-8 char boundaries.
///
/// # Examples
///
/// ```rust
/// use ld_lucivy::tokenizer::{SimpleTokenizer, CamelCaseSplitFilter, TextAnalyzer};
///
/// let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
///     .filter(CamelCaseSplitFilter)
///     .build();
///
/// let mut stream = tokenizer.token_stream("getElementById");
/// assert_eq!(stream.next().unwrap().text, "getElement");
/// assert_eq!(stream.next().unwrap().text, "ById");
/// assert_eq!(stream.next(), None);
/// ```
#[derive(Clone)]
pub struct CamelCaseSplitFilter;

impl TokenFilter for CamelCaseSplitFilter {
    type Tokenizer<T: Tokenizer> = CamelCaseSplitWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> CamelCaseSplitWrapper<T> {
        CamelCaseSplitWrapper {
            inner: tokenizer,
            buffer: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct CamelCaseSplitWrapper<T> {
    inner: T,
    buffer: Vec<Token>,
}

impl<T: Tokenizer> Tokenizer for CamelCaseSplitWrapper<T> {
    type TokenStream<'a> = CamelCaseSplitStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.buffer.clear();
        CamelCaseSplitStream {
            tail: self.inner.token_stream(text),
            buffer: &mut self.buffer,
            position_offset: 0,
        }
    }
}

pub struct CamelCaseSplitStream<'a, T> {
    tail: T,
    buffer: &'a mut Vec<Token>,
    /// Extra position offset accumulated from previous splits in this stream.
    position_offset: usize,
}

impl<T: TokenStream> CamelCaseSplitStream<'_, T> {
    fn split_current_token(&mut self) {
        let token = self.tail.token_mut();

        // Apply position offset from previous splits
        token.position += self.position_offset;

        let text = &token.text;

        // Find camelCase + digit boundaries
        let byte_ranges = split_and_merge(text);

        if byte_ranges.len() <= 1 && text.len() <= MAX_CHUNK_BYTES {
            // No split needed — token passes through as-is (position already adjusted)
            return;
        }

        // Apply force-split on long chunks
        let mut final_ranges = Vec::new();
        for (start, end) in &byte_ranges {
            let chunk = &text[*start..*end];
            if chunk.len() > MAX_CHUNK_BYTES {
                final_ranges.extend(force_split_long(chunk, *start));
            } else {
                final_ranges.push((*start, *end));
            }
        }

        if final_ranges.len() <= 1 {
            return;
        }

        // Build sub-tokens in reverse (for pop order)
        let base_offset_from = token.offset_from;
        let base_position = token.position;

        for (i, &(start, end)) in final_ranges.iter().enumerate().rev() {
            self.buffer.push(Token {
                text: text[start..end].to_string(),
                offset_from: base_offset_from + start,
                offset_to: base_offset_from + end,
                position: base_position + i,
                position_length: 1,
            });
        }

        // Account for extra positions consumed by this split
        self.position_offset += final_ranges.len() - 1;
    }
}

impl<T: TokenStream> TokenStream for CamelCaseSplitStream<'_, T> {
    fn advance(&mut self) -> bool {
        // Pop buffered sub-token if available
        self.buffer.pop();
        if !self.buffer.is_empty() {
            return true;
        }

        // Advance inner stream
        if !self.tail.advance() {
            return false;
        }

        self.split_current_token();
        true
    }

    fn token(&self) -> &Token {
        self.buffer.last().unwrap_or_else(|| self.tail.token())
    }

    fn token_mut(&mut self) -> &mut Token {
        self.buffer.last_mut().unwrap_or_else(|| self.tail.token_mut())
    }
}

// ─── Splitting logic ────────────────────────────────────────────────────────

/// Find camelCase + digit boundaries, merge small chunks, return byte ranges.
fn split_and_merge(text: &str) -> Vec<(usize, usize)> {
    let boundaries = find_boundaries(text);

    // Build raw ranges from boundaries
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for i in 0..boundaries.len() {
        let start = boundaries[i];
        let end = if i + 1 < boundaries.len() {
            boundaries[i + 1]
        } else {
            text.len()
        };
        if start < end {
            ranges.push((start, end));
        }
    }

    if ranges.len() <= 1 {
        return ranges;
    }

    // Forward merge: accumulate chunks < MIN_CHUNK_CHARS with the next,
    // but never merge more than MAX_MERGED_CHUNKS raw chunks together.
    // This prevents long absorptions like "ag3weaver" → 1 token.
    // No backward merge: short last chunks stay separate.
    const MAX_MERGED_CHUNKS: usize = 2;

    let mut merged: Vec<(usize, usize)> = Vec::new();
    let mut acc_start = ranges[0].0;
    let mut acc_end = ranges[0].1;
    let mut acc_chunks = 1usize;

    for i in 1..ranges.len() {
        let acc_chars = text[acc_start..acc_end].chars().count();
        if acc_chars < MIN_CHUNK_CHARS && acc_chunks < MAX_MERGED_CHUNKS {
            // Merge: extend accumulator
            acc_end = ranges[i].1;
            acc_chunks += 1;
        } else {
            // Flush and start new accumulator
            merged.push((acc_start, acc_end));
            acc_start = ranges[i].0;
            acc_end = ranges[i].1;
            acc_chunks = 1;
        }
    }
    merged.push((acc_start, acc_end));

    merged
}

/// Find byte positions where a split should occur.
///
/// Rules:
/// - lower→UPPER: `getElement` → split before `E`
/// - UPPER→UPPER+lower: `HTMLParser` → split before `P` (acronym end)
/// - letter↔digit: `var123` → split before `1`
///
/// NOT split: ALL_CAPS runs (`FUNCTION` stays as one chunk).
fn find_boundaries(text: &str) -> Vec<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut boundaries = vec![0usize];

    for i in 1..chars.len() {
        let (byte_pos, cur) = chars[i];
        let (_, prev) = chars[i - 1];

        let split =
            // lower→UPPER: standard camelCase boundary
            (prev.is_lowercase() && cur.is_uppercase())
            // UPPER→UPPER+lower: end of acronym (e.g., HTML|Parser, HTML|AParser)
            // Split before cur when cur is uppercase and NEXT is lowercase
            || (i + 1 < chars.len() && cur.is_uppercase()
                && prev.is_uppercase() && chars[i + 1].1.is_lowercase())
            // letter↔digit transitions
            || (prev.is_alphabetic() && cur.is_ascii_digit())
            || (prev.is_ascii_digit() && cur.is_alphabetic());

        if split {
            boundaries.push(byte_pos);
        }
    }

    boundaries
}

/// Force-split a chunk longer than MAX_CHUNK_BYTES at char boundaries.
/// Returns byte ranges relative to the original token text.
fn force_split_long(chunk: &str, base_offset: usize) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut pos = 0;
    let bytes = chunk.as_bytes();

    while pos < bytes.len() {
        let remaining = bytes.len() - pos;
        let chunk_end = if remaining <= MAX_CHUNK_BYTES {
            bytes.len()
        } else {
            // Find char boundary at or before pos + MAX_CHUNK_BYTES
            let mut end = pos + MAX_CHUNK_BYTES;
            while end > pos && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            if end == pos {
                // Pathological: single char > MAX_CHUNK_BYTES (impossible for UTF-8, but safe)
                end = pos + 1;
                while end < bytes.len() && !chunk.is_char_boundary(end) {
                    end += 1;
                }
            }
            end
        };
        result.push((base_offset + pos, base_offset + chunk_end));
        pos = chunk_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenizer::{SimpleTokenizer, TextAnalyzer};

    fn split_token(text: &str) -> Vec<String> {
        let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(CamelCaseSplitFilter)
            .build();
        let mut stream = tokenizer.token_stream(text);
        let mut tokens = Vec::new();
        while let Some(tok) = stream.next() {
            tokens.push(tok.text.clone());
        }
        tokens
    }

    fn split_offsets(text: &str) -> Vec<(usize, usize)> {
        let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(CamelCaseSplitFilter)
            .build();
        let mut stream = tokenizer.token_stream(text);
        let mut offsets = Vec::new();
        while let Some(tok) = stream.next() {
            offsets.push((tok.offset_from, tok.offset_to));
        }
        offsets
    }

    // ── CamelCase tests ──

    #[test]
    fn test_get_element_by_id() {
        assert_eq!(split_token("getElementById"), vec!["getElement", "ById"]);
    }

    #[test]
    fn test_html_a_parser() {
        // HTMLA|Parser: split before P (last upper before lower transition)
        assert_eq!(split_token("HTMLAParser"), vec!["HTMLA", "Parser"]);
    }

    #[test]
    fn test_html_parser() {
        assert_eq!(split_token("HTMLParser"), vec!["HTML", "Parser"]);
    }

    #[test]
    fn test_rag3db() {
        // No backward merge: "db" stays separate.
        assert_eq!(split_token("rag3db"), vec!["rag3", "db"]);
    }

    #[test]
    fn test_my_var_123() {
        // No backward merge: "123" stays separate.
        assert_eq!(split_token("myVar"), vec!["myVar"]);
        assert_eq!(split_token("myVar123"), vec!["myVar", "123"]);
    }

    #[test]
    fn test_allcaps() {
        assert_eq!(split_token("ALLCAPS"), vec!["ALLCAPS"]);
    }

    #[test]
    fn test_function_allcaps() {
        // ALL_CAPS words must NOT be split — they're constants, not camelCase
        assert_eq!(split_token("FUNCTION"), vec!["FUNCTION"]);
        assert_eq!(split_token("SCHEDULER"), vec!["SCHEDULER"]);
        assert_eq!(split_token("DIRECTION"), vec!["DIRECTION"]);
        assert_eq!(split_token("INITIALIZATION"), vec!["INITIALIZATION"]);
    }

    #[test]
    fn test_lowercase() {
        assert_eq!(split_token("lowercase"), vec!["lowercase"]);
    }

    #[test]
    fn test_cafe_latte() {
        assert_eq!(split_token("caféLatte"), vec!["café", "Latte"]);
    }

    #[test]
    fn test_single_char() {
        assert_eq!(split_token("A"), vec!["A"]);
    }

    #[test]
    fn test_get_x() {
        // "get"(3<4) forward merges with "X" → "getX" (single token).
        assert_eq!(split_token("getX"), vec!["getX"]);
    }

    // ── Long token tests ──

    #[test]
    fn test_long_token_split() {
        let long = "a".repeat(300);
        let tokens = split_token(&long);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].len(), 256);
        assert_eq!(tokens[1].len(), 44);
    }

    #[test]
    fn test_very_long_token() {
        let long = "a".repeat(65530);
        let tokens = split_token(&long);
        assert!(tokens.len() > 1);
        for tok in &tokens[..tokens.len() - 1] {
            assert_eq!(tok.len(), 256);
        }
        // Last chunk is the remainder
        assert_eq!(tokens.last().unwrap().len(), 65530 % 256);
    }

    // ── Offset tests ──

    #[test]
    fn test_offsets_adjusted() {
        // "getElementById" → "getElement" + "ById"
        let offsets = split_offsets("getElementById");
        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets[0], (0, 10)); // "getElement"
        assert_eq!(offsets[1], (10, 14)); // "ById"
    }

    #[test]
    fn test_offsets_no_split() {
        let offsets = split_offsets("lowercase");
        assert_eq!(offsets.len(), 1);
        assert_eq!(offsets[0], (0, 9));
    }

    #[test]
    fn test_offsets_with_prefix() {
        // "hello getElementById" → "hello" + "getElement" + "ById"
        let offsets = split_offsets("hello getElementById");
        assert_eq!(offsets.len(), 3);
        assert_eq!(offsets[0], (0, 5));   // "hello"
        assert_eq!(offsets[1], (6, 16));  // "getElement"
        assert_eq!(offsets[2], (16, 20)); // "ById"
    }

    // ── Multi-token tests ──

    #[test]
    fn test_multi_word_sentence() {
        let tokens = split_token("hello camelCase world");
        // "camel"(5>=4) + "Case"(4>=4) → both big enough, split
        assert_eq!(tokens, vec!["hello", "camel", "Case", "world"]);
    }

    #[test]
    fn test_positions_increment() {
        let mut tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(CamelCaseSplitFilter)
            .build();
        let mut stream = tokenizer.token_stream("hello getElementById world");
        let mut positions = Vec::new();
        while let Some(tok) = stream.next() {
            positions.push((tok.text.clone(), tok.position));
        }
        // "hello"(0), "getElement"(1), "ById"(2), "world"(3)
        assert_eq!(positions[0], ("hello".into(), 0));
        assert_eq!(positions[1], ("getElement".into(), 1));
        assert_eq!(positions[2], ("ById".into(), 2));
        assert_eq!(positions[3], ("world".into(), 3));
    }

}
