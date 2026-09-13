//! SFX Collector v3 — overlap-aware token collection for indexation.
//!
//! Differences from v2:
//! - Tokens are extended with 2-byte overlap from the next token
//! - No GapMap, SepMap, or sibling table (separators are in the tokens)
//! - Tracks word_id and is_word_start per token via ChunkMeta
//! - Interns extended token texts (e.g., "mutex_lo" not "mutex_")

use std::hash::{Hash, Hasher};

use hashbrown::HashTable;
use rustc_hash::FxHasher;

use super::termtexts_v3::TermMetaV3;

use crate::tokenizer::equal_chunk::{is_content_char, segment_and_chunk, DEFAULT_MAX_TOKEN};

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

/// Estimated bytes of one chunk posting:
/// `(doc_id, token_index, byte_from, byte_to)` plus the amortised cost of the
/// per-ordinal `Vec` that holds it.
const CHUNK_POSTING_BYTES: usize = 16 + 8;
/// `(doc_id, first_ti, last_ti, byte_from, byte_to)`, same amortisation.
const WORD_POSTING_BYTES: usize = 20 + 8;
/// `(u32, u32, u16)`, padded.
const SIBLING_PAIR_BYTES: usize = 12;
/// What one interned token costs beyond its two copies of the text: a `String`
/// header, its `Vec` of postings, its metadata and a hash slot.
const INTERNED_TOKEN_OVERHEAD: usize = 24 + 24 + 24 + 16;

/// One word-stripped entry beyond its two strings.
const WORD_STRIPPED_OVERHEAD: usize = 24 + 24 + 32;

/// V3 collector: interns overlap-extended chunks and word-stripped entries with
/// their postings and sibling pairs, one document at a time, for the SFX build.
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
    /// Interned extended tokens: one ordinal per `(shape, text)`. The table
    /// holds ordinals only; the hash is computed from the text and its
    /// shape, equality reads `token_texts` / `token_meta` back — no key is
    /// allocated to look a token up, and none is stored (measured 13
    /// September: the formatted key was 7 % of the indexing CPU and its
    /// `String`s a good part of the 12 % spent freeing the collector).
    token_intern: HashTable<u32>,
    token_texts: Vec<String>,
    /// Scratch for the dictionary's intern key, reused across tokens.
    key_scratch: String,
    // Posting entries indexed by interned ordinal: (doc_id, ti, byte_from, byte_to).
    token_postings: Vec<Vec<(u32, u32, u32, u32)>>,
    // Metadata per interned ordinal (from first occurrence).
    token_meta: Vec<TokenMetaV3>,
    /// Word-level stripped entries, one per word-stripped intern ordinal
    /// (the first occurrence's — every field the builders read derives from
    /// the key and the shape). One per *occurrence* until 13 September
    /// 2026: two `String`s per word of every value, deduplicated by the
    /// FST builder afterwards.
    word_stripped_entries: Vec<WordStrippedEntry>,
    /// Per intern ordinal: whether `word_stripped_entries` already names it.
    ws_entry_pushed: Vec<bool>,
    /// Scratch buffers of `add_value`, reused across values: the extended
    /// chunk text, the word's content, its content overlap, the word entry's
    /// key, and the chunk ranges of the value's words.
    scratch: AddValueScratch,
    // Direct word postings: (doc_id, first_ti, last_ti, byte_from, byte_to)
    // indexed by ws intern_ord. Captured directly in add_value() where we know
    // the exact word identity — eliminates the lossy content_key join.
    /// Per word-stripped intern ordinal: `(doc, first_ti, last_ti, byte_from,
    /// content end, tail_off)` — the bytes for the collector's own checks, the
    /// tail offset for the posting (`word_sfxpost`: `WSP5`).
    word_postings: Vec<Vec<(u32, u32, u32, u32, u32, u16)>>,

    // Sibling pairs: (intern_ord_a, intern_ord_b, content_len_a) for consecutive chunks and words.
    // content_len_a = content bytes of ordinal A (used by sibling DFS to know how much
    // of the query the first token consumes, excluding overlap).
    // Collected during add_value, remapped to final ordinals in into_data.
    sibling_pairs: Vec<(u32, u32)>,
    /// `false` on an index without positions (`IndexSettings::positions`):
    /// no sibling pair is collected and neither `.word_pos_map` nor
    /// `.sibling_v3` is built — nothing reads them — and the word postings
    /// are written as documents and frequencies (`WSP6`) directly.
    positions: bool,

    // Per-document state
    doc_active: bool,
    current_doc_id: u32,
    current_value_ti_start: u32,

    /// Running estimate of what this collector holds, in bytes.
    ///
    /// The segment writer decides to flush from `mem_usage()`, which counted
    /// the postings, the fieldnorms, the fast fields and the serializer — and
    /// never this. It was safe only by accident: positions and offsets filled
    /// the postings budget first and cut segments early. Once a v3 index
    /// stopped recording them (25 August), nothing bounded a segment any more:
    /// the same 2 000 documents went from ~56 segments to 4, and the FST
    /// builder, whose peak scales with a segment's tokens, asked for 384 MB in
    /// a browser and aborted the commit.
    ///
    /// Maintained incrementally: `mem_usage` is called once per document, so
    /// it cannot walk millions of interned texts.
    mem_estimate: usize,
    /// The shard dictionary this collector interns against (`sfx_version`
    /// 4), with the field it collects: a text found there keeps its global
    /// id, a new one is minted on the dictionary's counter.
    dictionary: Option<(super::dictionary::DictionarySlot, u32)>,
    /// Global id per intern ordinal (dictionary mode).
    global_ids: Vec<u64>,
    /// Whether this collector minted the id (dictionary mode): its text
    /// goes to `.newtexts`.
    minted: Vec<bool>,

    // Config
    max_token: usize,
    overlap: usize,
    min_suffix_len: usize,
}

/// Metadata stored per unique extended token.
#[derive(Debug, Clone)]
pub struct TokenMetaV3 {
    /// Bytes owned by the chunk itself: content + trailing separators, no overlap.
    pub own_len: u16,
    /// Trailing separator bytes included in `own_len`.
    pub sep_len: u8,
    /// Bytes borrowed from the next token and appended to the extended text.
    pub overlap_len: u8,
    /// True when this chunk is the first chunk of a word.
    pub is_word_start: bool,
    /// Index of the word this chunk belongs to, within its value (from the tokenizer).
    pub word_id: usize,
    /// True if this token was interned for a word-stripped entry (partition 0x02 only).
    /// Excluded from the build loop for partitions 0x00/0x01.
    pub is_word_stripped: bool,
}

/// The buffers `add_value` reuses from one value to the next.
#[derive(Default)]
struct AddValueScratch {
    extended: String,
    word_content: String,
    content_overlap: String,
    ws_extended: String,
    chunk_posting_info: Vec<(u32, u32, u32, u32)>,
    chunk_intern_ids: Vec<u32>,
    /// `(word_id, first chunk, one past the last chunk)` of the value's words.
    word_ranges: Vec<(usize, usize, usize)>,
    ws_intern_sequence: Vec<(u32, u16)>,
}

impl Default for SfxCollectorV3 {
    fn default() -> Self {
        Self::new()
    }
}

impl SfxCollectorV3 {
    /// Empty collector with default chunk size and overlap; the minimum suffix
    /// length comes from `LUCIVY_MIN_SUFFIX_LEN` (default 1).
    pub fn new() -> Self {
        let min = std::env::var("LUCIVY_MIN_SUFFIX_LEN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        Self {
            token_intern: HashTable::new(),
            token_texts: Vec::new(),
            key_scratch: String::new(),
            token_postings: Vec::new(),
            token_meta: Vec::new(),
            word_stripped_entries: Vec::new(),
            ws_entry_pushed: Vec::new(),
            scratch: AddValueScratch::default(),
            word_postings: Vec::new(),
            sibling_pairs: Vec::new(), // (intern_a, intern_b)
            positions: true,
            doc_active: false,
            current_doc_id: 0,
            current_value_ti_start: 0,
            mem_estimate: 0,
            max_token: DEFAULT_MAX_TOKEN,
            overlap: DEFAULT_OVERLAP,
            min_suffix_len: min,
            dictionary: None,
            global_ids: Vec::new(),
            minted: Vec::new(),
        }
    }

    /// Collect for an index without positions (`positions: false`): see
    /// the `positions` field.
    pub fn without_positions(mut self) -> Self {
        self.positions = false;
        self
    }

    /// Intern against a shard dictionary: ordinals become global ids.
    pub fn with_dictionary(mut self, dictionary: super::dictionary::DictionarySlot, field_id: u32) -> Self {
        self.dictionary = Some((dictionary, field_id));
        self
    }

    /// Collector with explicit chunk size, overlap bytes and minimum suffix length.
    pub fn with_config(max_token: usize, overlap: usize, min_suffix_len: usize) -> Self {
        Self {
            mem_estimate: 0,
            max_token,
            overlap,
            min_suffix_len,
            ..Self::new()
        }
    }

    /// Start a new document: resets per-document state and the token index.
    pub fn begin_doc(&mut self) {
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
            self.current_value_ti_start += 1; // value boundary gap
            return;
        }

        let num_chunks = chunks.len();
        // Track byte offsets in original text
        let mut offset = 0usize;
        // The scratch buffers, taken for the duration of the call (the
        // intern step borrows `self` mutably).
        let mut scratch = std::mem::take(&mut self.scratch);
        // Per-chunk posting info: (doc_id, ti, byte_from, byte_to) for word-stripped
        let chunk_posting_info = &mut scratch.chunk_posting_info;
        chunk_posting_info.clear();
        // Per-chunk intern ids for sibling pairs and word-stripped entries
        let chunk_intern_ids = &mut scratch.chunk_intern_ids;
        chunk_intern_ids.clear();
        let extended = &mut scratch.extended;

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

            // Build extended token: chunk + normal overlap
            extended.clear();
            extended.push_str(chunk_text);
            extended.push_str(overlap_bytes);

            let own_len = chunk_len as u16;
            let ti = self.current_value_ti_start + i as u32;

            // Intern the extended token
            let intern_id = self.intern_extended(extended, TokenMetaV3 {
                own_len,
                sep_len: meta.sep_len as u8,
                overlap_len,
                is_word_start: meta.is_word_start,
                word_id: meta.word_id,
                is_word_stripped: false,
            });

            // Add posting
            let byte_from = offset as u32;
            let byte_to = (offset + meta.content_len + meta.sep_len) as u32;
            self.mem_estimate += CHUNK_POSTING_BYTES;
            self.token_postings[intern_id as usize].push((
                self.current_doc_id, ti, byte_from, byte_to,
            ));

            // ── DIAG: trace collector for target word ──
            if let Some(target) = diag_collector_target() {
                if extended.to_lowercase().contains(target) {
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
            // word_pos_map is no longer written here: it is derived in into_data
            // from word_postings, the same data word_sfxpost is built from.

            offset += chunk_len;
        }

        // Collect chunk sibling pairs: consecutive chunks in the same value
        // content_len = destination chunk's content length (used by DFS)
        // The destination's content length, once carried here for the DFS,
        // is read from `.termtexts` META since 4 September 2026.
        if self.positions {
            for w in chunk_intern_ids.windows(2) {
                self.mem_estimate += SIBLING_PAIR_BYTES;
                self.sibling_pairs.push((w[0], w[1]));
            }
        }

        // Build word-level stripped entries from this value's chunks.
        // Group by word_id, concatenate content, find content_overlap to next word.
        {
            // The chunks of a word are consecutive and the words come in
            // order (the tokenizer numbers them as it splits): a word is a
            // range of chunk indices. (A `BTreeMap<word, Vec<chunk>>` per
            // value, before 13 September 2026, was an allocation per word.)
            let word_ranges = &mut scratch.word_ranges;
            word_ranges.clear();
            for (i, (_, meta)) in chunks.iter().enumerate() {
                match word_ranges.last_mut() {
                    Some((wid, _, end)) if *wid == meta.word_id => *end = i + 1,
                    _ => word_ranges.push((meta.word_id, i, i + 1)),
                }
            }

            let word_content = &mut scratch.word_content;
            let content_overlap = &mut scratch.content_overlap;
            let ws_extended = &mut scratch.ws_extended;
            let ws_intern_sequence = &mut scratch.ws_intern_sequence; // (intern_ord, content_len)
            ws_intern_sequence.clear();
            for wi in 0..word_ranges.len() {
                let (_, first_ci, end_ci) = word_ranges[wi];
                let last_ci = end_ci - 1;

                // Concatenate content bytes
                word_content.clear();
                for ci in first_ci..end_ci {
                    let (ref ct, ref cm) = chunks[ci];
                    let clen = cm.content_len.min(ct.len());
                    word_content.push_str(&ct[..clen]);
                }
                if word_content.is_empty() {
                    continue;
                }

                // Content overlap: first bytes of the NEXT word with content —
                // and that word only. Its first character may not fit in
                // `overlap` bytes (`“underscan`, `文件`): the overlap is then
                // empty, as for a word that ends the value. Until 13
                // September 2026 the search went on to the word after (`D
                // \n,,“underscan”,ENUM` gave `D` the overlap `EN`), and the
                // entry `den` then claimed an adjacency the text does not
                // have: relaxed `de` rendered `D\n,,“` as a match, six times
                // on the kernel, ending inside the multi-byte character.
                content_overlap.clear();
                'next_word: for &(_, next_first, next_end) in &word_ranges[wi + 1..] {
                    for ci in next_first..next_end {
                        let (ref ct, ref cm) = chunks[ci];
                        if cm.content_len > 0 {
                            let ov_len = self.overlap.min(cm.content_len).min(ct.len());
                            let mut end = ov_len;
                            while end > 0 && !ct.is_char_boundary(end) {
                                end -= 1;
                            }
                            content_overlap.push_str(&ct[..end]);
                            break 'next_word;
                        }
                    }
                }

                // Intern the word-stripped entry as its OWN token (not reusing the
                // first chunk's ordinal). The key is word_content + content_overlap,
                // which is unique per word and won't collide with chunk keys.
                // This ensures "include" and "inclusive" get distinct ordinals.
                ws_extended.clear();
                ws_extended.push_str(word_content);
                ws_extended.push_str(content_overlap);
                // Saturating: a word beyond u16 must still read back as "long",
                // the STATS section of `.termtexts` depends on it.
                let ws_own_len = (word_content.len() + chunks[last_ci].1.sep_len).min(u16::MAX as usize) as u16;
                let ws_intern = self.intern_extended(ws_extended, TokenMetaV3 {
                    own_len: ws_own_len,
                    sep_len: chunks[last_ci].1.sep_len as u8,
                    overlap_len: content_overlap.len() as u8,
                    is_word_start: chunks[first_ci].1.is_word_start,
                    word_id: chunks[first_ci].1.word_id,
                    is_word_stripped: true,
                });
                // NO chunk-level posting here. Word postings are captured directly
                // below and stored in self.word_postings for WordSfxPost.

                let max_token = crate::tokenizer::equal_chunk::DEFAULT_MAX_TOKEN;

                // The estimate still counts every occurrence: the budget that
                // cuts segments keeps its meaning (same segments as before),
                // only the memory and the work go.
                self.mem_estimate += WORD_STRIPPED_OVERHEAD;
                if self.mark_ws_entry(ws_intern) {
                    self.word_stripped_entries.push(WordStrippedEntry {
                        word_content: word_content.clone(),
                        content_overlap: content_overlap.clone(),
                        first_intern_ord: ws_intern,
                        first_chunk_intern_ord: chunk_intern_ids[first_ci],
                        last_chunk_intern_ord: chunk_intern_ids[last_ci],
                        first_own_len: ws_own_len,
                        last_sep_len: chunks[last_ci].1.sep_len as u8,
                        is_word_start: chunks[first_ci].1.is_word_start,
                        num_chunks: (end_ci - first_ci) as u32,
                    });
                }

                // Capture word posting directly — we know the exact word identity here.
                let first_posting = &chunk_posting_info[first_ci];
                let last_posting = &chunk_posting_info[last_ci];
                while self.word_postings.len() <= ws_intern as usize {
                    self.word_postings.push(Vec::new());
                }
                // byte_to is the end of the word's CONTENT, not of its last
                // chunk: the key "init" is "in"+"it" in one document and the
                // word "init" in another, under one ordinal, so only the
                // posting can say where this occurrence's content stops. The
                // content is contiguous from the first chunk start.
                self.mem_estimate += WORD_POSTING_BYTES;
                self.word_postings[ws_intern as usize].push((
                    self.current_doc_id,
                    first_posting.1, // first_ti (position of first chunk)
                    last_posting.1,  // last_ti (position of last chunk)
                    first_posting.2, // byte_from (start of first chunk)
                    first_posting.2 + word_content.len() as u32, // content end
                    0,               // a word starts its first chunk
                ));

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
                    let tail_len = tail_content.len() as u32;

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
                        is_word_stripped: true,
                    });
                    // NO chunk-level posting. Tail word posting captured below.

                    // The tail's bytes are the last `max_token` content bytes
                    // of the word, contiguous from the first chunk start. The
                    // chunk they start in is the word's last chunk — unless
                    // the word's trailing separators spilled into a chunk of
                    // their own (`解。\n\n` fills a chunk, `.. ` starts the
                    // next): then it is the chunk before. `first_position`
                    // used to be the last chunk's regardless, so three words
                    // of the kernel pointed at a separator-only chunk while
                    // `byte_from` said the chunk before (5 September 2026).
                    let tail_from = first_posting.2 + ts as u32;
                    let tail_first_ci = (first_ci..end_ci)
                        .find(|&ci| { let p = &chunk_posting_info[ci]; p.2 <= tail_from && tail_from < p.3 })
                        .unwrap_or(last_ci);

                    self.mem_estimate += WORD_STRIPPED_OVERHEAD;
                    if self.mark_ws_entry(tail_intern) {
                        self.word_stripped_entries.push(WordStrippedEntry {
                            word_content: tail_content,
                            content_overlap: content_overlap.clone(),
                            first_intern_ord: tail_intern,
                            first_chunk_intern_ord: chunk_intern_ids[tail_first_ci],
                            last_chunk_intern_ord: chunk_intern_ids[last_ci],
                            first_own_len: tail_own_len,
                            last_sep_len: chunks[last_ci].1.sep_len as u8,
                            is_word_start: false,
                            num_chunks: (last_ci - tail_first_ci + 1) as u32,
                        });
                    }

                    while self.word_postings.len() <= tail_intern as usize {
                        self.word_postings.push(Vec::new());
                    }
                    self.mem_estimate += WORD_POSTING_BYTES;
                    self.word_postings[tail_intern as usize].push((
                        self.current_doc_id,
                        chunk_posting_info[tail_first_ci].1, // the chunk holding `tail_from`
                        chunk_posting_info[last_ci].1,       // the word's last chunk, as for the word itself
                        tail_from,
                        tail_from + tail_len, // content end
                        // Where the tail starts within that chunk: the one
                        // thing a posting without byte spans has to say.
                        (tail_from - chunk_posting_info[tail_first_ci].2) as u16,
                    ));
                }
            }

            // Collect word sibling pairs: consecutive words in the same value
            // content_len = destination word's content length (used by DFS to
            // know how many bytes of the sibling's text are content vs overlap)
            if self.positions {
                for w in ws_intern_sequence.windows(2) {
                    self.mem_estimate += SIBLING_PAIR_BYTES;
                    self.sibling_pairs.push((w[0].0, w[1].0));
                }
            }
        }

        // Advance: tokens + 1 boundary gap between values
        self.current_value_ti_start += num_chunks as u32 + 1;
        self.scratch = scratch;
    }

    /// True the first time a word-stripped intern ordinal is seen: its
    /// entry goes to `word_stripped_entries` then, and never again.
    fn mark_ws_entry(&mut self, intern: u32) -> bool {
        let i = intern as usize;
        if i >= self.ws_entry_pushed.len() {
            self.ws_entry_pushed.resize(i + 1, false);
        }
        if self.ws_entry_pushed[i] {
            return false;
        }
        self.ws_entry_pushed[i] = true;
        true
    }

    /// Close the current document and advance to the next doc_id.
    pub fn end_doc(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;
    }

    /// Close a document that received no values; still consumes a doc_id so
    /// that doc ids stay aligned with the segment's.
    pub fn end_doc_empty(&mut self) {
        self.doc_active = false;
        self.current_doc_id += 1;
    }

    }

/// The collector's intern key of an entry: its text (case kept) and its
/// shape — what makes two dictionary ids distinct. Also the key the shard
/// dictionary's Bloom filter hashes, so a `.termtexts` entry must rebuild
/// exactly this (`SfxDictionary::filter`).
pub fn intern_key(text: &str, is_word_stripped: bool, own_len: u16, sep_len: u8, is_word_start: bool) -> String {
    let mut key = String::with_capacity(text.len() + 16);
    write_intern_key(&mut key, text, is_word_stripped, own_len, sep_len, is_word_start);
    key
}

/// `intern_key` into a caller's buffer (cleared first): the same bytes, no
/// allocation once the buffer has grown.
pub fn write_intern_key(key: &mut String, text: &str, is_word_stripped: bool, own_len: u16, sep_len: u8, is_word_start: bool) {
    use std::fmt::Write;
    key.clear();
    if is_word_stripped {
        let _ = write!(key, "\x00ws:{text}\x00{}", own_len - sep_len as u16);
    } else {
        let _ = write!(key, "{text}\x00{}:{}:{}", own_len, sep_len, is_word_start as u8);
    }
}

/// What makes two interned tokens the same entry, beyond their text: the
/// fields `intern_key` writes. A word-stripped entry is keyed by its content
/// length alone (`own_len - sep_len`), a chunk by its full shape.
#[inline]
fn intern_shape(meta: &TokenMetaV3) -> (bool, u16, u8, bool) {
    if meta.is_word_stripped {
        (true, meta.own_len - meta.sep_len as u16, 0, false)
    } else {
        (false, meta.own_len, meta.sep_len, meta.is_word_start)
    }
}

#[inline]
fn intern_hash(shape: (bool, u16, u8, bool), text: &str) -> u64 {
    let mut h = FxHasher::default();
    shape.hash(&mut h);
    text.as_bytes().hash(&mut h);
    h.finish()
}

/// `V3_DIAG_COLLECTOR=<needle>`: trace the collector's work on every token
/// containing the needle. Read once — this used to be an environment lookup
/// per chunk.
fn diag_collector_target() -> Option<&'static str> {
    static TARGET: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TARGET.get_or_init(|| std::env::var("V3_DIAG_COLLECTOR").ok()).as_deref()
}

impl SfxCollectorV3 {
    /// Intern an extended token, returning its ordinal.
    ///
    /// Chunk entries and word-stripped entries use separate namespaces even
    /// when their text is identical (e.g., "functional" from chunking
    /// "functionality" vs "functional" as a word-stripped entry).
    /// This prevents word-stripped meta from poisoning chunk entries,
    /// which would cause their postings to be skipped in into_data().
    fn intern_extended(&mut self, text: &str, meta: TokenMetaV3) -> u32 {
        // Separate namespace: word-stripped entries get a prefix to avoid collision.
        // They are also keyed by their content length: the key "init" is the
        // word "init" in one document and "in" + overlap "it" in another. One
        // ordinal for both would carry a single (own_len, overlap_len) — the
        // first occurrence's — and the chain walk would resume the query at the
        // wrong byte for every posting of the other shape. The FST takes several
        // parents per key, so each shape gets its own ordinal.
        // Chunks have the same problem with their own metadata: "spinlock" is
        // a whole chunk (own_len 8) in one document and "spinlo" + overlap
        // "ck" (own_len 6) in another. Anything that rebuilds text from
        // termtexts — the literal verification, the window for relaxed
        // matches — reads own_len, so each shape needs its own ordinal.
        let shape = intern_shape(&meta);
        let hash = intern_hash(shape, text);
        let texts = &self.token_texts;
        let metas = &self.token_meta;
        if let Some(&ord) = self.token_intern.find(hash, |&o| {
            intern_shape(&metas[o as usize]) == shape && texts[o as usize] == text
        }) {
            return ord;
        }
        let ord = self.token_texts.len() as u32;
        // The text, its per-ordinal Vec, meta and hash slot.
        self.mem_estimate += text.len() + INTERNED_TOKEN_OVERHEAD;
        if let Some((slot, field_id)) = &self.dictionary {
            let dict = slot.read().unwrap().clone()
                .expect("a dictionary index always holds a dictionary");
            let key = &mut self.key_scratch;
            write_intern_key(key, text, meta.is_word_stripped, meta.own_len, meta.sep_len, meta.is_word_start);
            // The collector's hash of (shape, text) also keys the dictionary's
            // shared cache of found ids, mixed with the field.
            let key_hash = hash ^ (*field_id as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let (global, minted) = dict.lookup_or_mint_hashed(*field_id, key, key_hash, text, &meta);
            self.global_ids.push(global);
            self.minted.push(minted);
        }
        let texts = &self.token_texts;
        let metas = &self.token_meta;
        self.token_intern.insert_unique(hash, ord, |&o| {
            intern_hash(intern_shape(&metas[o as usize]), &texts[o as usize])
        });
        self.token_texts.push(text.to_string());
        self.token_postings.push(Vec::new());
        self.token_meta.push(meta);
        ord
    }

    /// What this collector currently holds, in bytes — an estimate, maintained
    /// as tokens and postings are added.
    ///
    /// This is what the segment writer has to include in its budget: the FST
    /// builder's peak scales with these, and nothing else bounds a v3 segment.
    pub fn mem_usage(&self) -> usize {
        self.mem_estimate
    }

    /// Test helper: ordinal of the chunk entry with this extended text,
    /// whatever its shape (intern keys carry the shape after a NUL).
    #[cfg(test)]
    fn chunk_ord(&self, text: &str) -> Option<u32> {
        (0..self.token_texts.len() as u32)
            .find(|&o| !self.token_meta[o as usize].is_word_stripped && self.token_texts[o as usize] == text)
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

        // --- Final ordinals ---
        // The intern table already gives one ordinal per `(shape, text)`, so
        // a final ordinal is an intern ordinal in a chosen order: chunk
        // entries first, then word-stripped entries, each group by text and
        // shape — or, with a shard dictionary, by increasing global id, which
        // makes the segment's `.gmap` a sorted list (local → global by index,
        // global → local by binary search). Word-stripped entries only count
        // when a word entry names them (`word_stripped_entries`), as before.
        let diag_target = diag_collector_target();
        let dictionary_mode = self.dictionary.is_some();
        let mut is_ws_entry = vec![false; num_tokens];
        for ws in &self.word_stripped_entries {
            let io = ws.first_intern_ord as usize;
            if self.token_meta[io].is_word_stripped {
                is_ws_entry[io] = true;
            }
        }
        let mut order: Vec<u32> = (0..num_tokens as u32)
            .filter(|&io| {
                let m = &self.token_meta[io as usize];
                if m.is_word_stripped { is_ws_entry[io as usize] } else { true }
            })
            .collect();
        if dictionary_mode {
            order.sort_by_key(|&io| self.global_ids[io as usize]);
        } else {
            order.sort_by(|&a, &b| {
                let (ma, mb) = (&self.token_meta[a as usize], &self.token_meta[b as usize]);
                (ma.is_word_stripped, self.token_texts[a as usize].as_bytes(), intern_shape(ma))
                    .cmp(&(mb.is_word_stripped, self.token_texts[b as usize].as_bytes(), intern_shape(mb)))
            });
        }

        let mut intern_to_final = vec![0u32; num_tokens];
        let mut content_postings: Vec<Vec<(u32, u32)>> = Vec::with_capacity(order.len());
        let mut tokens_vec: Vec<String> = Vec::with_capacity(order.len());
        let mut own_lens: Vec<u16> = Vec::with_capacity(order.len());
        let mut globals: Vec<u32> = Vec::with_capacity(if dictionary_mode { order.len() } else { 0 });
        let mut newtexts: Vec<(u32, String, TermMetaV3)> = Vec::new();
        let mut token_postings = self.token_postings;
        // Word-stripped entries whose word postings are written once ordinals exist.
        let mut deferred_ws: Vec<u32> = Vec::new();

        for (final_ord, &io) in order.iter().enumerate() {
            let final_ord = final_ord as u32;
            let io = io as usize;
            let m = &self.token_meta[io];
            if dictionary_mode {
                let global = self.global_ids[io];
                debug_assert!(global <= u32::MAX as u64, "global id beyond u32");
                globals.push(global as u32);
                if self.minted[io] {
                    newtexts.push((global as u32, self.token_texts[io].clone(), TermMetaV3 {
                        own_len: m.own_len,
                        sep_len: m.sep_len,
                        overlap_len: m.overlap_len,
                        is_word_start: m.is_word_start,
                        is_word_stripped: m.is_word_stripped,
                    }));
                }
            }
            tokens_vec.push(self.token_texts[io].clone());
            // A word-stripped entry has no chunk postings: its word postings
            // go to `.word_sfxpost`.
            let mut p: Vec<(u32, u32)> = if m.is_word_stripped {
                deferred_ws.push(io as u32);
                Vec::new()
            } else {
                std::mem::take(&mut token_postings[io]).into_iter().map(|(d, ti, _, _)| (d, ti)).collect()
            };
            let before_dedup = p.len();
            p.sort_unstable();
            p.dedup();
            if let Some(target) = diag_target {
                if self.token_texts[io].to_lowercase().contains(target) {
                    eprintln!("[COLLECTOR final_ord] text={:?} final_ord={} intern_ord={} ws={} postings_before_dedup={} postings_after_dedup={} doc_ids={:?}",
                        self.token_texts[io], final_ord, io, m.is_word_stripped,
                        before_dedup, p.len(),
                        p.iter().map(|x| x.0).collect::<Vec<_>>());
                }
            }
            content_postings.push(p);
            own_lens.push(m.own_len);
            intern_to_final[io] = final_ord;
        }
        let final_ord = order.len() as u32;
        drop(token_postings);

        use crate::suffix_fst::word_sfxpost::WordPostingEntry;

        // Build word postings (WordSfxPost) — now that ordinals are assigned.
        // Word postings were captured directly in add_value() where we know the
        // exact word identity. No content_key join needed — zero cross-word leaks.
        let mut word_sfxpost_writer = if self.positions {
            crate::suffix_fst::word_sfxpost::WordSfxPostWriter::new(final_ord as usize)
        } else {
            crate::suffix_fst::word_sfxpost::WordSfxPostWriter::docs_only(final_ord as usize)
        };
        // word_pos_map is fed from the same loop, so it is the exact inverse of
        // word_sfxpost by construction.
        let mut word_pos_map = crate::suffix_fst::word_pos_map::WordPosMapWriter::new();
        for &intern_ord in &deferred_ws {
            let ws_final_ord = intern_to_final[intern_ord as usize];
            let io = intern_ord as usize;
            if io < self.word_postings.len() {
                for &(doc_id, first_ti, last_ti, bf, bt, tail_off) in &self.word_postings[io] {
                    word_sfxpost_writer.add(ws_final_ord, WordPostingEntry {
                        doc_id,
                        first_position: first_ti,
                        last_position: last_ti,
                        byte_from: bf,
                        byte_to: bt,
                        tail_off,
                    });
                    if self.positions {
                        word_pos_map.add_word(doc_id, first_ti, last_ti, ws_final_ord);
                    }
                }
            }
        }
        let word_sfxpost_data = word_sfxpost_writer.finish();

        let word_pos_map_data = if self.positions { word_pos_map.serialize() } else { Vec::new() };

        // Build sibling table v3: remap intern ordinals to final ordinals
        // Sibling table v3: gap_len field stores content_len of the source ordinal
        // (used by sibling_chain_dfs to know how many bytes of the query are consumed
        // by each sibling, excluding the overlap portion).
        let mut sibling_writer = crate::suffix_fst::sibling_table::SiblingTableWriter::new(final_ord);
        // No gap on any link → the writer emits the gap-less `SIB3` layout.
        for &(a, b) in &self.sibling_pairs {
            let fa = intern_to_final[a as usize];
            let fb = intern_to_final[b as usize];
            sibling_writer.add(fa, fb, 0);
        }
        let sibling_v3_data = if self.positions { sibling_writer.serialize() } else { Vec::new() };

        // What `.termtexts` STATS says for a segment with its own texts,
        // kept in the `.gmap` of a dictionary segment (see `gmap.rs`).
        let max_word_content_len: u16 = self.token_meta.iter()
            .filter(|m| m.is_word_stripped)
            .map(|m| m.own_len.saturating_sub(m.sep_len as u16))
            .max()
            .unwrap_or(0);
        let max_word_content_len = Some(max_word_content_len);

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
            word_stripped: self.word_stripped_entries,
            word_sfxpost: word_sfxpost_data,
            word_pos_map: word_pos_map_data,
            sibling_v3: sibling_v3_data,
            globals: if dictionary_mode { Some(globals) } else { None },
            newtexts,
            max_word_content_len,
            positions: self.positions,
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
    /// Used by derived index builders (bytemap, posmap).
    /// Ordered token texts, 1:1 with content_postings and own_lens by final ordinal.
    pub tokens: Vec<String>,
    /// Postings per final ordinal, `(doc, position)`. Index = final ordinal.
    /// No byte span: `.sfxpost` is written as `SFP5`, the offsets derive
    /// from `.posmap` (`PMP4`) and the texts' `own_len`.
    pub content_postings: Vec<Vec<(u32, u32)>>,
    /// own_len per final ordinal (content+sep bytes, excludes overlap).
    /// Used by derived index builders to truncate extended texts.
    pub own_lens: Vec<u16>,
    /// Number of unique final ordinals (= content_postings.len()).
    pub num_content_ords: usize,
    /// Number of documents the collector saw (`end_doc` calls).
    pub num_docs: u32,
    /// Shortest suffix (in bytes) the FST builder should index for SI>0.
    pub min_suffix_len: usize,
    /// Word-level stripped entries for partition 0x02.
    pub word_stripped: Vec<WordStrippedEntry>,
    /// Word-level sfxpost (serialized). Separate from chunk sfxpost because
    /// word postings have different semantics: position = last chunk,
    /// byte_from = first chunk start (entire word span).
    pub word_sfxpost: Vec<u8>,
    /// WordPosMap: (doc_id, position) → word_id_within_doc.
    /// Per-doc word assignment for exact chain verification.
    pub word_pos_map: Vec<u8>,
    /// Sibling table v3: ordinal → [next_ordinals] for both chunks and words.
    pub sibling_v3: Vec<u8>,
    /// Shard dictionary mode: the global id of each final ordinal (sorted —
    /// the `.gmap`). `None` for a segment with its own dictionary.
    pub globals: Option<Vec<u32>>,
    /// Shard dictionary mode: the ids this segment minted, with their text
    /// and meta (the `.newtexts`), in id order.
    pub newtexts: Vec<(u32, String, TermMetaV3)>,
    /// Longest word-stripped content of the segment, when known: written
    /// in the `.gmap` of a dictionary segment (`.termtexts` STATS otherwise).
    pub max_word_content_len: Option<u16>,
    /// Whether the postings are written with their positions (`SFP5`,
    /// `WSP5`) or as documents and term frequencies (`SFP6`, `WSP6`,
    /// `IndexSettings::positions`). Set by the segment writer from the
    /// index settings; a merge keeps its sources' layout.
    pub positions: bool,
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

        // Find content_overlap: first `overlap_size` bytes of the next word's
        // content — that word only, empty when its first character does not
        // fit (see `add_value`: the word after it is never the overlap).
        let mut content_overlap = String::new();
        'next_word: for next_wi in (wi + 1)..word_ids.len() {
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
                    break 'next_word;
                }
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
        assert!(c.chunk_ord("mutex_lo").is_some(), "should intern extended token");
        assert!(!c.chunk_ord("mutex_").is_some(), "should NOT intern base token");
        assert!(c.chunk_ord("lock").is_some(), "last token has no overlap");
    }

    #[test]
    fn test_overlap_bytes() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock_init");
        c.end_doc();

        // "mutex_" + overlap "lo" → "mutex_lo"
        assert!(c.chunk_ord("mutex_lo").is_some());
        // "lock_" + overlap "in" → "lock_in"
        assert!(c.chunk_ord("lock_in").is_some());
        // "init" → no overlap
        assert!(c.chunk_ord("init").is_some());
    }

    #[test]
    fn test_metadata_preserved() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        let ord = c.chunk_ord("mutex_lo").unwrap();
        let meta = &c.token_meta[ord as usize];
        assert_eq!(meta.own_len, 6); // "mutex_" = 6 bytes
        assert_eq!(meta.sep_len, 1); // "_"
        assert_eq!(meta.overlap_len, 2); // "lo"
        assert!(meta.is_word_start);
        assert_eq!(meta.word_id, 0);

        let ord = c.chunk_ord("lock").unwrap();
        let meta = &c.token_meta[ord as usize];
        assert_eq!(meta.own_len, 4);
        assert_eq!(meta.sep_len, 0);
        assert_eq!(meta.overlap_len, 0);
        assert!(meta.is_word_start);
        assert_eq!(meta.word_id, 1);
    }

    /// A very long word (a Chinese line, no separator inside, over 264
    /// bytes, so it gets a tail entry) whose trailing separators spill into
    /// a chunk of their own (`解。\n\n` fills the chunk, `.. ` starts the
    /// next): the three word postings of the kernel whose `first_position`
    /// disagreed with their `byte_from`
    /// (`postings_measure::byte_spans_are_derivable`, 5 September 2026).
    /// Every word posting's `first_position` must be the chunk holding its
    /// `byte_from`.
    #[test]
    fn word_position_when_separators_spill_into_the_next_chunk() {
        let text = format!("{}解。\n\n.. toctree::\n", "可以理".repeat(30));
        let text = text.as_str();
        let chunks = segment_and_chunk(text, crate::tokenizer::equal_chunk::DEFAULT_MAX_TOKEN);
        for (i, (t, m)) in chunks.iter().enumerate() {
            eprintln!("chunk {i}: {t:?} content {} sep {} word_id {} word_start {}", m.content_len, m.sep_len, m.word_id, m.is_word_start);
        }
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value(text);
        c.end_doc();
        let mut offsets = Vec::new();
        let mut off = 0u32;
        for (t, _) in &chunks { offsets.push(off); off += t.len() as u32; }
        for (ord, posts) in c.word_postings.iter().enumerate() {
            for &(doc, first, last, from, to, _) in posts {
                let meta = &c.token_meta[ord];
                eprintln!("word ord {ord} {:?} own {} sep {}: doc {doc} first {first} last {last} from {from} to {to}; chunk at first starts at byte {:?}",
                    c.token_texts.get(ord).map(|s| s.as_str()).unwrap_or("?"), meta.own_len, meta.sep_len, offsets.get(first as usize));
                let start = offsets[first as usize];
                let end = offsets.get(first as usize + 1).copied().unwrap_or(text.len() as u32);
                assert!(start <= from && from < end,
                    "the word's first position must be the chunk its bytes start in: from {from}, chunk {first} is {start}..{end}");
            }
        }
    }

    #[test]
    fn test_postings_correct() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("mutex_lock");
        c.end_doc();

        let ord = c.chunk_ord("mutex_lo").unwrap();
        let postings = &c.token_postings[ord as usize];
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].0, 0); // doc_id = 0
        assert_eq!(postings[0].1, 0); // ti = 0
        assert_eq!(postings[0].2, 0); // byte_from = 0
        assert_eq!(postings[0].3, 6); // byte_to = 6 ("mutex_")

        let ord = c.chunk_ord("lock").unwrap();
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
        assert!(c.chunk_ord("mutex_lo").is_some());
        assert!(c.chunk_ord("mutex_co").is_some());
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
        // Two shapes of the same text (a whole chunk and a word-stripped
        // entry can share their extended text): equal neighbours are legal
        // since interning went by (text, shape). The builder needs the
        // texts NON-DECREASING; strict order was the pre-shape invariant.
        c.begin_doc();
        c.add_value("zebra alphazeb ra");
        c.end_doc();

        let data = c.into_data();
        // The pre-shape invariant "tokens sorted by text" is gone: interning
        // keys carry a shape suffix and a partition prefix, so `tokens`
        // (final-ordinal order) interleaves. What must hold instead:
        // `sorted_indices` orders the extended texts for the FST builder,
        // and every final ordinal has a text and a posting list.
        assert_eq!(data.tokens.len(), data.content_postings.len());
        assert_eq!(data.tokens.len(), data.num_content_ords);
        assert!(data.tokens.iter().all(|t| !t.is_empty()));
        for w in data.sorted_indices.windows(2) {
            let (a, b) = (&data.token_texts[w[0] as usize], &data.token_texts[w[1] as usize]);
            assert!(a <= b, "sorted_indices out of order: {a:?} > {b:?}");
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

    /// One word entry per word-stripped ordinal, the first occurrence's —
    /// and its shape is the one `.termtexts` records for that ordinal. A
    /// word followed by different separators is one ordinal (the separator
    /// is not in the intern key); until 13 September 2026 every occurrence
    /// pushed an entry with its own separator length, and the FST record
    /// kept whichever the builder saw last, disagreeing with `.termtexts`.
    #[test]
    fn one_word_entry_per_ordinal_with_the_first_occurrence_shape() {
        let mut c = SfxCollectorV3::new();
        c.begin_doc();
        c.add_value("value 0x00000000\n\t\tnext");
        c.end_doc();
        c.begin_doc();
        c.add_value("value 0x00000000 next");
        c.end_doc();
        c.begin_doc();
        c.add_value("value 0x00000000\n\t\tnext");
        c.end_doc();
        let data = c.into_data();
        let mut per_ordinal = std::collections::HashMap::new();
        for ws in &data.word_stripped {
            assert!(per_ordinal.insert(ws.first_intern_ord, ws).is_none(), "ordinal {} entered twice", ws.first_intern_ord);
            let final_ord = data.intern_to_final[ws.first_intern_ord as usize] as usize;
            assert_eq!(ws.first_own_len, data.own_lens[final_ord], "entry {:?}: own_len differs from the ordinal's meta", ws.word_content);
            assert_eq!(ws.last_sep_len, data.token_meta[ws.first_intern_ord as usize].sep_len);
        }
        let zeros: Vec<_> = data.word_stripped.iter().filter(|w| w.word_content == "0x00000000").collect();
        assert_eq!(zeros.len(), 1, "one ordinal for the word whatever follows it");
        assert_eq!((zeros[0].first_own_len, zeros[0].last_sep_len), (13, 3), "the first occurrence's separators");
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
        let ord = c.chunk_ord("mutex_lo").unwrap();
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

        // The cross-boundary trigram "x_l" is the key "x_" plus the overlap
        // "lo" its record carries (keys stop at the token boundary).
        let key = [super::super::builder::SI_REST_PREFIX, b'x', b'_'];
        let val = fst.get(key).expect("x_ at SI>0");
        let parents = crate::suffix_fst::builder_v3::decode_parent_entries_v8(
            lucivy_fst::OutputTable::new(&_output_table).get(val), &key);
        assert!(parents.iter().any(|p| p.overlap[..2] == *b"lo"),
            "cross-boundary trigram 'x_l' should be in the record's overlap");
    }
}
