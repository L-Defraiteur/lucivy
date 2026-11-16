//! Matching cores on immutable, already selected bytes. No paths or filesystem I/O.
//! Fuzzy occurrences share the engine's definition, not its index execution path.

pub type Span = (usize, usize);

#[derive(Debug)]
pub enum OracleError {
    Utf8(std::str::Utf8Error),
    InvalidSpan { from: usize, to: usize, length: usize },
}

impl From<std::str::Utf8Error> for OracleError {
    fn from(error: std::str::Utf8Error) -> Self { Self::Utf8(error) }
}

pub fn is_content_char(c: char) -> bool {
    !c.is_ascii() || c.is_ascii_alphanumeric()
}

pub fn strip_seps(s: &str) -> String {
    s.chars().filter(|c| is_content_char(*c)).collect()
}

fn fold(text: &str, strip: bool) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut out = Vec::with_capacity(text.len());
    let mut back = Vec::with_capacity(text.len());
    for (offset, ch) in text.char_indices() {
        if strip && !is_content_char(ch) { continue; }
        for lower in ch.to_lowercase() {
            let mut buffer = [0; 4];
            for byte in lower.encode_utf8(&mut buffer).bytes() {
                out.push(byte);
                back.push((offset, ch.len_utf8()));
            }
        }
    }
    (out, back)
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() { return out; }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match hay[i..].iter().position(|&b| b == first) {
            None => break,
            Some(p) => {
                let start = i + p;
                if start + needle.len() <= hay.len() && &hay[start..start + needle.len()] == needle {
                    out.push(start);
                }
                i = start + 1;
            }
        }
    }
    out
}

pub fn literal_spans(bytes: &[u8], needle: &str, strict: bool) -> Result<Vec<Span>, OracleError> {
    let text = std::str::from_utf8(bytes)?;
    let (needle, _) = fold(needle, !strict);
    let (hay, back) = fold(text, !strict);
    let mut spans = Vec::new();
    for start in find_all(&hay, &needle) {
        let (last, length) = back[start + needle.len() - 1];
        spans.push((back[start].0, last + length));
    }
    Ok(spans)
}

pub fn fuzzy_spans(bytes: &[u8], needle: &str, distance: u8) -> Result<Vec<Span>, OracleError> {
    let text = std::str::from_utf8(bytes)?;
    let needle = strip_seps(&needle.to_lowercase()).into_bytes();
    if needle.is_empty() { return Ok(Vec::new()); }
    let (stripped, back) = fold(text, true);
    let mut spans = Vec::new();
    for (start, end, _) in ld_lucivy::suffix_fst::briques::fuzzy_spans::fuzzy_spans(
        &needle, &stripped, distance as usize,
    ) {
        let (last, length) = back[end - 1];
        spans.push((back[start].0, last + length));
    }
    Ok(spans)
}

pub fn regex_spans(bytes: &[u8], regex: &regex::Regex) -> Result<Vec<Span>, OracleError> {
    let text = std::str::from_utf8(bytes)?;
    Ok(regex.find_iter(text).filter(|m| m.start() != m.end())
        .map(|m| (m.start(), m.end())).collect())
}

pub fn filter_boundaries(bytes: &[u8], spans: &[Span], anchor: bool, exact: bool)
    -> Result<Vec<Span>, OracleError>
{
    let text = std::str::from_utf8(bytes)?;
    let mut out = Vec::new();
    for &(from, to) in spans {
        if from > to || to > bytes.len() || !text.is_char_boundary(from) || !text.is_char_boundary(to) {
            return Err(OracleError::InvalidSpan { from, to, length: bytes.len() });
        }
        let before_ok = from == 0 || !text[..from].chars().next_back().is_some_and(is_content_char);
        let after_ok = to == bytes.len() || !text[to..].chars().next().is_some_and(is_content_char);
        if (!anchor && !exact) || (before_ok && (!exact || after_ok)) { out.push((from, to)); }
    }
    Ok(out)
}
