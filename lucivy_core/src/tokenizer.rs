//! Custom tokenizers for lucivy.
//!
//! Provides an n-gram (trigram) filter for substring search via n-gram index.

use std::mem;

use ld_lucivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

/// Size of character-level n-grams to generate.
const NGRAM_SIZE: usize = 3;

/// Token filter that expands each token into character-level n-grams (trigrams).
///
/// Tokens shorter than [`NGRAM_SIZE`] characters are passed through as-is.
/// Longer tokens produce all sliding windows of [`NGRAM_SIZE`] characters.
///
/// # Examples
///
/// - `"programming"` → `["pro", "rog", "ogr", "gra", "ram", "amm", "mmi", "min", "ing"]`
/// - `"ab"` → `["ab"]` (shorter than trigram size, passed through)
/// - `"abc"` → `["abc"]` (exactly trigram size, one trigram)
#[derive(Clone)]
pub struct NgramFilter;

impl TokenFilter for NgramFilter {
    type Tokenizer<T: Tokenizer> = NgramFilterWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> NgramFilterWrapper<T> {
        NgramFilterWrapper { inner: tokenizer }
    }
}

#[derive(Clone)]
pub struct NgramFilterWrapper<T> {
    inner: T,
}

impl<T: Tokenizer> Tokenizer for NgramFilterWrapper<T> {
    type TokenStream<'a> = NgramFilterStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        NgramFilterStream {
            tail: self.inner.token_stream(text),
            buffer: Vec::new(),
            buffer_idx: 0,
            token: Token {
                offset_from: 0,
                offset_to: 0,
                position: 0,
                text: String::new(),
                position_length: 1,
            },
        }
    }
}

/// Token stream that emits trigrams from each source token.
pub struct NgramFilterStream<T> {
    tail: T,
    /// Trigrams generated from the current source token.
    buffer: Vec<String>,
    /// Index of the next trigram to emit from `buffer`.
    buffer_idx: usize,
    /// Reusable token for emission.
    token: Token,
}

impl<T: TokenStream> TokenStream for NgramFilterStream<T> {
    fn advance(&mut self) -> bool {
        // Emit remaining trigrams from buffer.
        if self.buffer_idx < self.buffer.len() {
            self.token.text.clear();
            self.token.text.push_str(&self.buffer[self.buffer_idx]);
            self.buffer_idx += 1;
            return true;
        }

        // Pull next token from inner stream.
        if !self.tail.advance() {
            return false;
        }

        // Copy metadata and take ownership of text to avoid borrow conflicts.
        let (offset_from, offset_to, position, position_length, text) = {
            let src = self.tail.token_mut();
            (
                src.offset_from,
                src.offset_to,
                src.position,
                src.position_length,
                mem::take(&mut src.text),
            )
        };

        self.token.offset_from = offset_from;
        self.token.offset_to = offset_to;
        self.token.position = position;
        self.token.position_length = position_length;

        // Generate character-level trigrams.
        self.buffer.clear();
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < NGRAM_SIZE {
            // Token shorter than trigram size: index as-is.
            self.buffer.push(text);
        } else {
            for w in chars.windows(NGRAM_SIZE) {
                self.buffer.push(w.iter().collect());
            }
        }

        self.buffer_idx = 1;
        self.token.text.clear();
        self.token.text.push_str(&self.buffer[0]);
        true
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

#[cfg(test)]
mod tests {
    use ld_lucivy::tokenizer::{LowerCaser, SimpleTokenizer, TextAnalyzer, TokenStream};

    use super::NgramFilter;

    fn tokenize(text: &str) -> Vec<String> {
        let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(NgramFilter)
            .build();
        let mut stream = analyzer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }

    #[test]
    fn test_trigrams_basic() {
        assert_eq!(tokenize("hello"), vec!["hel", "ell", "llo"]);
    }

    #[test]
    fn test_short_token_passthrough() {
        assert_eq!(tokenize("ab"), vec!["ab"]);
        assert_eq!(tokenize("a"), vec!["a"]);
    }

    #[test]
    fn test_exact_trigram_length() {
        assert_eq!(tokenize("abc"), vec!["abc"]);
    }

    #[test]
    fn test_multi_token() {
        assert_eq!(
            tokenize("hello world"),
            vec!["hel", "ell", "llo", "wor", "orl", "rld"]
        );
    }

    #[test]
    fn test_lowercase() {
        assert_eq!(tokenize("Hello"), vec!["hel", "ell", "llo"]);
    }

    #[test]
    fn test_programming() {
        assert_eq!(
            tokenize("programming"),
            vec!["pro", "rog", "ogr", "gra", "ram", "amm", "mmi", "min", "ing"]
        );
    }

    #[test]
    fn test_mixed_lengths() {
        // "I am OK" → tokens "i", "am", "ok" → all < 3 chars → passthrough
        assert_eq!(tokenize("I am OK"), vec!["i", "am", "ok"]);
    }

    #[test]
    fn test_unicode() {
        // "café" → chars ['c','a','f','é'] → trigrams: "caf", "afé"
        assert_eq!(tokenize("café"), vec!["caf", "afé"]);
    }

    #[test]
    fn test_positions_preserved() {
        let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(NgramFilter)
            .build();
        let mut stream = analyzer.token_stream("hello world");
        let mut positions = Vec::new();
        while stream.advance() {
            positions.push((stream.token().text.clone(), stream.token().position));
        }
        // All trigrams from "hello" share position 0, from "world" share position 1
        assert_eq!(
            positions,
            vec![
                ("hel".into(), 0),
                ("ell".into(), 0),
                ("llo".into(), 0),
                ("wor".into(), 1),
                ("orl".into(), 1),
                ("rld".into(), 1),
            ]
        );
    }
}
