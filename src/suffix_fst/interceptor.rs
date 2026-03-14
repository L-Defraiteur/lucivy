use tokenizer_api::{BoxTokenStream, Token, TokenStream};

/// Captured token data from the interception.
#[derive(Debug, Clone)]
pub struct CapturedToken {
    /// Lowercase token text.
    pub text: String,
    /// Byte offset of the start of this token in the original text.
    pub offset_from: usize,
    /// Byte offset of the end of this token in the original text.
    pub offset_to: usize,
}

/// Wraps a BoxTokenStream to capture tokens as they flow through.
/// Passes all tokens unchanged — just a Vec::push per token overhead.
///
/// Usage in segment_writer:
/// ```ignore
/// let token_stream = text_analyzer.token_stream(text);
/// let mut interceptor = SfxTokenInterceptor::wrap(token_stream);
/// postings_writer.index_text(doc_id, &mut interceptor, ...);
/// let captured = interceptor.take_captured();
/// ```
pub struct SfxTokenInterceptor<'a> {
    inner: BoxTokenStream<'a>,
    captured: Vec<CapturedToken>,
}

impl<'a> SfxTokenInterceptor<'a> {
    /// Wrap a BoxTokenStream for interception.
    pub fn wrap(inner: BoxTokenStream<'a>) -> Self {
        Self {
            inner,
            captured: Vec::new(),
        }
    }

    /// Take the captured tokens, leaving the internal vec empty.
    pub fn take_captured(&mut self) -> Vec<CapturedToken> {
        std::mem::take(&mut self.captured)
    }
}

impl TokenStream for SfxTokenInterceptor<'_> {
    fn advance(&mut self) -> bool {
        if self.inner.advance() {
            let tok = self.inner.token();
            self.captured.push(CapturedToken {
                text: tok.text.clone(),
                offset_from: tok.offset_from,
                offset_to: tok.offset_to,
            });
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        self.inner.token()
    }

    fn token_mut(&mut self) -> &mut Token {
        self.inner.token_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Simple test token stream
    struct TestTokenStream {
        tokens: Vec<Token>,
        pos: usize,
    }

    impl TestTokenStream {
        fn new(words: &[(&str, usize, usize)]) -> Self {
            Self {
                tokens: words
                    .iter()
                    .enumerate()
                    .map(|(i, (text, from, to))| Token {
                        text: text.to_string(),
                        offset_from: *from,
                        offset_to: *to,
                        position: i,
                        position_length: 1,
                    })
                    .collect(),
                pos: 0,
            }
        }
    }

    impl TokenStream for TestTokenStream {
        fn advance(&mut self) -> bool {
            if self.pos < self.tokens.len() {
                self.pos += 1;
                true
            } else {
                false
            }
        }

        fn token(&self) -> &Token {
            &self.tokens[self.pos - 1]
        }

        fn token_mut(&mut self) -> &mut Token {
            &mut self.tokens[self.pos - 1]
        }
    }

    fn make_stream(words: &[(&str, usize, usize)]) -> BoxTokenStream<'static> {
        BoxTokenStream::new(TestTokenStream::new(words))
    }

    #[test]
    fn test_interceptor_captures_all_tokens() {
        let stream = make_stream(&[
            ("import", 0, 6),
            ("rag3db", 7, 13),
            ("from", 14, 18),
        ]);

        let mut interceptor = SfxTokenInterceptor::wrap(stream);

        // Consume the stream (as postings_writer would)
        let mut count = 0;
        while interceptor.advance() {
            let _ = interceptor.token();
            count += 1;
        }
        assert_eq!(count, 3);

        let captured = interceptor.take_captured();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].text, "import");
        assert_eq!(captured[0].offset_from, 0);
        assert_eq!(captured[0].offset_to, 6);
        assert_eq!(captured[1].text, "rag3db");
        assert_eq!(captured[2].text, "from");
    }

    #[test]
    fn test_interceptor_passthrough() {
        let stream = make_stream(&[("hello", 0, 5), ("world", 6, 11)]);
        let mut interceptor = SfxTokenInterceptor::wrap(stream);

        assert!(interceptor.advance());
        assert_eq!(interceptor.token().text, "hello");
        assert_eq!(interceptor.token().offset_from, 0);

        assert!(interceptor.advance());
        assert_eq!(interceptor.token().text, "world");

        assert!(!interceptor.advance());
    }

    #[test]
    fn test_interceptor_empty_stream() {
        let stream = make_stream(&[]);
        let mut interceptor = SfxTokenInterceptor::wrap(stream);

        assert!(!interceptor.advance());
        assert!(interceptor.take_captured().is_empty());
    }
}
