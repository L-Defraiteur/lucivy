//! Measurements and consistency checks on the posting files of an index on
//! disk. Nothing here is used by the engine; it answered the sizing
//! questions of `docs/05-09-2026/04-progression-et-a-faire.md` §2 before the
//! spans were dropped from the postings (`SFP5`, `WSP5`), and it checks,
//! after, that what replaces them agrees with the files: the posmap's byte
//! checkpoints against a full prefix sum, and every word posting against
//! the chunks under it.

use std::path::Path;

use super::gmap::GmapReader;
use super::posmap::PosMapReader;
use super::sfxpost_v2::{SfxPostReaderV2, SfxPostWriterV2};
use super::termtexts_v3::TermTextsReaderV3;
use super::word_sfxpost::{WordPostingEntry, WordSfxPostReader, WordSfxPostWriter};

#[derive(Default, Debug)]
struct Tally {
    files: usize,
    entries: u64,
    /// Bytes on disk.
    on_disk: u64,
    /// Bytes of the same entries re-encoded by today's writer (the
    /// reference: checks the writer reproduces the file's size).
    reencoded: u64,
    /// Bytes of the same entries in the positions-only layout (`SFP5`,
    /// `WSP5`): the file without the byte spans.
    without_spans: u64,
    /// Files whose layout carries the spans (`SFP2`-`SFP4`, `WSP2`-`WSP4`).
    spanned_files: usize,
    /// Distinct `(term, doc)` pairs: what a documents-only layout would hold.
    doc_pairs: u64,
    /// Bytes of those pairs as `varint(doc delta) + varint(tf)`, per term —
    /// the estimate of a documents-only layout before any block packing.
    doc_only: u64,
}

fn varint_len(mut v: u64) -> u64 {
    let mut n = 1;
    while v >= 0x80 { v >>= 7; n += 1; }
    n
}

/// Fold one `(term ordinal, doc)` occurrence into the documents-only tally:
/// a new pair costs its doc delta, every occurrence a tf increment (priced
/// when the pair closes).
struct DocOnly { prev_doc: Option<u32>, tf: u64, pairs: u64, bytes: u64 }
impl DocOnly {
    fn new() -> Self { DocOnly { prev_doc: None, tf: 0, pairs: 0, bytes: 0 } }
    fn see(&mut self, doc: u32) {
        match self.prev_doc {
            Some(d) if d == doc => { self.tf += 1; }
            prev => {
                if prev.is_some() { self.bytes += varint_len(self.tf); }
                let delta = prev.map(|d| doc.abs_diff(d) as u64).unwrap_or(doc as u64);
                self.bytes += varint_len(delta);
                self.pairs += 1;
                self.prev_doc = Some(doc);
                self.tf = 1;
            }
        }
    }
    fn close(&mut self, t: &mut Tally) {
        if self.prev_doc.is_some() { self.bytes += varint_len(self.tf); }
        t.doc_pairs += self.pairs;
        t.doc_only += self.bytes;
        *self = DocOnly::new();
    }
}

fn mb(b: u64) -> f64 { b as f64 / 1048576.0 }

/// `.sfxpost`: every entry is `(doc, token_index[, byte_from, byte_to])`.
/// A spanned file is re-encoded in both layouts; an `SFP5` file in its own
/// (the re-encoded size checks the writer reproduces the file).
fn measure_sfxpost(bytes: &[u8], t: &mut Tally) {
    let Some(reader) = SfxPostReaderV2::open_slice(bytes) else { return };
    let n = reader.num_terms() as usize;
    let spanned = reader.has_byte_spans();
    let mut full = if spanned { SfxPostWriterV2::new(n) } else { SfxPostWriterV2::positions_only(n) };
    let mut bare = SfxPostWriterV2::positions_only(n);
    let mut entries = 0u64;
    let mut docs = DocOnly::new();
    for ordinal in 0..n as u32 {
        reader.for_each_entry(ordinal, |doc, ti, bf, bt| {
            full.add_entry(ordinal, doc, ti, bf, bt);
            bare.add_position(ordinal, doc, ti);
            docs.see(doc);
            entries += 1;
        });
        docs.close(t);
    }
    t.files += 1;
    t.spanned_files += spanned as usize;
    t.entries += entries;
    t.on_disk += bytes.len() as u64;
    t.reencoded += full.finish().len() as u64;
    t.without_spans += bare.finish().len() as u64;
}

/// `.word_sfxpost`: `(doc, first, last[, byte_from, byte_to] | tail_off)`.
fn measure_word_sfxpost(bytes: &[u8], t: &mut Tally) {
    let Some(reader) = WordSfxPostReader::open(bytes) else { return };
    let n = reader.num_ordinals() as usize;
    let spanned = reader.has_byte_spans();
    let mut full = if spanned { WordSfxPostWriter::with_byte_spans(n) } else { WordSfxPostWriter::new(n) };
    let mut bare = WordSfxPostWriter::new(n);
    let mut entries = 0u64;
    let mut docs = DocOnly::new();
    for ordinal in 0..n as u32 {
        reader.for_each_entry(ordinal, |e| {
            docs.see(e.doc_id);
            full.add(ordinal, e.clone());
            bare.add(ordinal, WordPostingEntry { byte_from: 0, byte_to: 0, ..e });
            entries += 1;
        });
        docs.close(t);
    }
    t.files += 1;
    t.spanned_files += spanned as usize;
    t.entries += entries;
    t.on_disk += bytes.len() as u64;
    t.reencoded += full.finish().len() as u64;
    t.without_spans += bare.finish().len() as u64;
}

fn report(name: &str, t: &Tally) {
    eprintln!("{name}: {} files ({} with byte spans), {} entries, {:.1} MB on disk, re-encoded {:.1} MB ({:.2} B/entry), positions only {:.1} MB ({:.2} B/entry, -{:.1} %)",
        t.files, t.spanned_files, t.entries, mb(t.on_disk), mb(t.reencoded),
        t.reencoded as f64 / t.entries.max(1) as f64,
        mb(t.without_spans), t.without_spans as f64 / t.entries.max(1) as f64,
        100.0 * (1.0 - t.without_spans as f64 / t.reencoded.max(1) as f64));
    eprintln!("{name}: documents only — {} (term, doc) pairs ({:.2} positions per pair), {:.1} MB as varint deltas + tf ({:.2} B/pair, -{:.1} % against positions only)",
        t.doc_pairs, t.entries as f64 / t.doc_pairs.max(1) as f64, mb(t.doc_only),
        t.doc_only as f64 / t.doc_pairs.max(1) as f64,
        100.0 * (1.0 - t.doc_only as f64 / t.without_spans.max(1) as f64));
}

/// `LUCIVY_POSTINGS_DIR` names an index directory (one shard): every
/// `.sfxpost` and `.word_sfxpost` is decoded and re-encoded in its own
/// layout and in the positions-only one. On an index written with byte
/// spans the difference is what dropping them saves; on an index written
/// without, both sizes agree and the writer is shown to reproduce the file.
/// `LUCIVY_POSTINGS_MAX_FILES` caps the files visited per kind (a sample),
/// default all.
#[test]
#[ignore]
fn postings_without_byte_spans() {
    let Ok(dir) = std::env::var("LUCIVY_POSTINGS_DIR") else { return };
    let max_files: usize = std::env::var("LUCIVY_POSTINGS_MAX_FILES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    names.sort();
    let (mut chunk, mut word) = (Tally::default(), Tally::default());
    let mut other: std::collections::BTreeMap<String, u64> = Default::default();
    let t0 = std::time::Instant::now();
    for name in &names {
        let path = Path::new(&dir).join(name);
        let Some(ext) = name.rsplit('.').next() else { continue };
        match ext {
            "sfxpost" if chunk.files < max_files => {
                let bytes = std::fs::read(&path).unwrap();
                measure_sfxpost(&bytes, &mut chunk);
            }
            "word_sfxpost" if word.files < max_files => {
                let bytes = std::fs::read(&path).unwrap();
                measure_word_sfxpost(&bytes, &mut word);
            }
            "posmap" | "word_pos_map" | "sibling_v3" | "gmap" | "store" | "sfx" | "termtexts" => {
                let key = if name.starts_with("dict-") { format!("dict.{ext}") } else { ext.to_string() };
                *other.entry(key).or_default() += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
            _ => {}
        }
    }
    eprintln!("index {dir}: {} files, {:.1} s", names.len(), t0.elapsed().as_secs_f64());
    report(".sfxpost", &chunk);
    report(".word_sfxpost", &word);
    for (k, v) in &other {
        eprintln!("  {k}: {:.1} MB", mb(*v));
    }
    let total_postings = chunk.on_disk + word.on_disk;
    let saved = (chunk.reencoded - chunk.without_spans) + (word.reencoded - word.without_spans);
    let total: u64 = total_postings + other.values().sum::<u64>();
    eprintln!("byte spans in the postings: {:.1} MB = {:.1} % of the postings, {:.1} % of the index files counted ({:.1} MB)",
        mb(saved), 100.0 * saved as f64 / total_postings.max(1) as f64,
        100.0 * saved as f64 / total.max(1) as f64, mb(total));
}

/// Do the files agree on where every token is? For every segment of the
/// index in `LUCIVY_POSTINGS_DIR`, with the byte offset of every position
/// recomputed as a full prefix sum of the `own_len` of the ordinals at the
/// positions before it (`.posmap` + the texts' META, an empty slot
/// restarting a value at 0):
///
/// - a `PMP4` posmap: `byte_at` (checkpoint + walk) against the prefix sum
///   at every position — the derivation the queries use, checked against
///   an independent computation (`posmap_bad`);
/// - every word posting, whatever the layout: the text of its ordinal
///   (word or tail, `own_len - sep_len` content bytes) against the bytes of
///   the chunks at its positions, read from `tail_off` on — the check that
///   places a tail entry where its chunk really holds it (`word_text_bad`);
/// - a segment whose postings still carry spans (`SFP2`-`SFP4`,
///   `WSP2`-`WSP4`): those spans against the prefix sum and the META, as
///   before the spans were dropped (`chunk_*_bad`, `word_*_bad`).
///
/// Counts the disagreements and prints the first few.
/// `LUCIVY_POSTINGS_MAX_FILES` caps the segments.
#[test]
#[ignore]
fn byte_spans_are_derivable() {
    let Ok(dir) = std::env::var("LUCIVY_POSTINGS_DIR") else { return };
    let max_segments: usize = std::env::var("LUCIVY_POSTINGS_MAX_FILES").ok()
        .and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let read = |name: &str| std::fs::read(Path::new(&dir).join(name)).ok();
    let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string())).collect();
    names.sort();
    // Dictionary texts per field, all live generations (an index without a
    // dictionary has its texts per segment instead).
    let mut dict_texts: std::collections::BTreeMap<u32, Vec<Vec<u8>>> = Default::default();
    for n in &names {
        if let Some(rest) = n.strip_prefix("dict-") {
            let parts: Vec<&str> = rest.split('.').collect();
            if parts.len() == 3 && parts[2] == "termtexts" {
                if let (Some(bytes), Ok(f)) = (read(n), parts[1].parse::<u32>()) {
                    dict_texts.entry(f).or_default().push(bytes);
                }
            }
        }
    }
    #[derive(Default, Debug)]
    struct Counts {
        segments: usize, spanned_segments: usize, docs: u64, positions: u64, empty_slots: u64,
        posmap_checked: u64, posmap_bad: u64,
        chunk_entries: u64, chunk_from_bad: u64, chunk_to_bad: u64,
        word_entries: u64, word_tails: u64, word_text_bad: u64, word_from_bad: u64, word_to_bad: u64,
        leading_offset_docs: u64,
    }
    let mut c = Counts::default();
    let mut examples: Vec<String> = Vec::new();
    let t0 = std::time::Instant::now();
    for n in &names {
        let Some(prefix) = n.strip_suffix(".sfxpost") else { continue };
        if prefix.starts_with("dict-") || c.segments >= max_segments { continue }
        let field: u32 = prefix.rsplit('.').next().and_then(|f| f.parse().ok()).unwrap_or(0);
        let (Some(sfx), Some(wsp), Some(pm)) = (read(n), read(&format!("{prefix}.word_sfxpost")), read(&format!("{prefix}.posmap"))) else { continue };
        let gmap_bytes = read(&format!("{prefix}.gmap"));
        let gmap = gmap_bytes.as_deref().and_then(GmapReader::open);
        let seg_texts = read(&format!("{prefix}.termtexts"));
        let texts = match (&gmap, dict_texts.get(&field), &seg_texts) {
            (Some(_), Some(parts), _) => {
                let parts: Vec<&[u8]> = parts.iter().map(|b| b.as_slice()).collect();
                TermTextsReaderV3::open_parts(&parts)
            }
            (None, _, Some(bytes)) => TermTextsReaderV3::open(bytes),
            _ => None,
        };
        let Some(texts) = texts else { continue };
        let (Some(sfx), Some(wsp), Some(pm)) = (SfxPostReaderV2::open_slice(&sfx), WordSfxPostReader::open(&wsp), PosMapReader::open(&pm)) else { continue };
        let to_global = |local: u32| gmap.as_ref().map(|g| g.global(local)).unwrap_or(local);
        let meta = |local: u32| texts.meta(to_global(local));
        let spanned = sfx.has_byte_spans() && wsp.has_byte_spans();
        c.segments += 1;
        c.spanned_segments += spanned as usize;
        // Byte offset of every position of every document, by prefix sum;
        // an empty slot is a value boundary, the next value starts at 0.
        let mut prefix_sums: Vec<Vec<u32>> = Vec::with_capacity(pm.num_docs() as usize);
        for doc in 0..pm.num_docs() {
            let n_pos = pm.num_tokens(doc);
            let mut sums = Vec::with_capacity(n_pos as usize + 1);
            let mut off = 0u32;
            for p in 0..n_pos {
                sums.push(off);
                match pm.ordinal_at(doc, p).and_then(&meta) {
                    Some(m) => off += m.own_len as u32,
                    None => { c.empty_slots += 1; off = 0; }
                }
            }
            sums.push(off);
            c.positions += n_pos as u64;
            prefix_sums.push(sums);
        }
        c.docs += pm.num_docs() as u64;
        // The posmap's own derivation, at every position.
        if pm.has_byte_checkpoints() {
            for doc in 0..pm.num_docs() {
                for p in 0..pm.num_tokens(doc) {
                    // The posmap here is opened without the `.gmap`, so it hands
                    // back local ordinals (the engine opens it with the gmap and
                    // gets global ids): translate before asking the texts.
                    let derived = pm.byte_at(doc, p, |local| meta(local).map(|m| m.own_len));
                    let expected = if pm.ordinal_at(doc, p).is_some() { prefix_sums[doc as usize].get(p as usize).copied() } else { None };
                    c.posmap_checked += 1;
                    if derived != expected {
                        c.posmap_bad += 1;
                        if examples.len() < 12 { examples.push(format!("{prefix}: byte_at doc {doc} p {p}: {derived:?}, prefix sum {expected:?}")); }
                    }
                }
            }
        }
        // Every word posting against the chunks under it: the entry's text
        // (word or tail) must be what the chunks hold from `tail_off` on.
        for w in 0..wsp.num_ordinals() {
            let Some(word_text) = texts.text(to_global(w)) else { continue };
            let Some(m) = meta(w) else { continue };
            let content = m.own_len.saturating_sub(m.sep_len as u16) as usize;
            let word_content = &word_text.as_bytes()[..content.min(word_text.len())];
            wsp.for_each_entry(w, |e| {
                c.word_entries += 1;
                if e.tail_off != 0 { c.word_tails += 1; }
                let mut under: Vec<u8> = Vec::new();
                for p in e.first_position..=e.last_position {
                    let Some(o) = pm.ordinal_at(e.doc_id, p) else { break };
                    let (Some(t), Some(cm)) = (texts.text(to_global(o)), meta(o)) else { break };
                    under.extend_from_slice(&t.as_bytes()[..(cm.own_len as usize).min(t.len())]);
                }
                let start = e.tail_off as usize;
                let ok = under.len() >= start + word_content.len() && &under[start..start + word_content.len()] == word_content;
                if !ok {
                    c.word_text_bad += 1;
                    if examples.len() < 12 {
                        examples.push(format!("{prefix}: word ord {w} {word_text:?} doc {} first {} last {} tail_off {}: chunks hold {:?}",
                            e.doc_id, e.first_position, e.last_position, e.tail_off, String::from_utf8_lossy(&under)));
                    }
                }
            });
        }
        if !spanned { continue; }
        // Documents whose first chunk does not start at byte 0 (leading
        // separators): the prefix sum alone cannot know that offset.
        let mut leading: std::collections::HashSet<u32> = Default::default();
        for o in 0..sfx.num_terms() {
            sfx.for_each_entry(o, |doc, ti, bf, bt| {
                c.chunk_entries += 1;
                let expected_from = prefix_sums.get(doc as usize).and_then(|s| s.get(ti as usize)).copied();
                if ti == 0 && bf != 0 { leading.insert(doc); }
                if expected_from != Some(bf) {
                    c.chunk_from_bad += 1;
                    if examples.len() < 12 { examples.push(format!("{prefix}: chunk ord {o} doc {doc} ti {ti}: from {bf}, prefix sum {expected_from:?}")); }
                }
                let own = meta(o).map(|m| m.own_len as u32);
                if own != Some(bt - bf) {
                    c.chunk_to_bad += 1;
                    if examples.len() < 12 { examples.push(format!("{prefix}: chunk ord {o} doc {doc} ti {ti}: to-from {}, own_len {own:?}", bt - bf)); }
                }
            });
        }
        c.leading_offset_docs += leading.len() as u64;
        for w in 0..wsp.num_ordinals() {
            wsp.for_each_entry(w, |e| {
                let expected_from = prefix_sums.get(e.doc_id as usize).and_then(|s| s.get(e.first_position as usize)).copied();
                if expected_from != Some(e.byte_from) {
                    c.word_from_bad += 1;
                    if examples.len() < 12 {
                        let word_text = texts.text(to_global(w)).unwrap_or("?");
                        let around: Vec<String> = (e.first_position.saturating_sub(2)..=e.last_position + 1)
                            .filter_map(|p| pm.ordinal_at(e.doc_id, p).map(|o| {
                                let stored: Vec<String> = sfx.entries_for_doc(o, e.doc_id).iter()
                                    .map(|c| format!("ti{} {}..{}", c.token_index, c.byte_from, c.byte_to)).collect();
                                format!("p{p}={:?} sum {} stored [{}]", texts.text(to_global(o)).unwrap_or("?"),
                                    prefix_sums[e.doc_id as usize].get(p as usize).copied().unwrap_or(0), stored.join(", "))
                            }))
                            .collect();
                        examples.push(format!("{prefix}: word ord {w} {word_text:?} doc {} first {} last {}: from {} to {}, prefix sum {expected_from:?}; chunks {}", e.doc_id, e.first_position, e.last_position, e.byte_from, e.byte_to, around.join(" ")));
                    }
                }
                let content = meta(w).map(|m| m.own_len.saturating_sub(m.sep_len as u16) as u32);
                if content != Some(e.byte_to - e.byte_from) {
                    c.word_to_bad += 1;
                    if examples.len() < 12 { examples.push(format!("{prefix}: word ord {w} doc {} first {}: to-from {}, content {content:?}", e.doc_id, e.first_position, e.byte_to - e.byte_from)); }
                }
            });
        }
    }
    eprintln!("{c:#?}\n{:.1} s", t0.elapsed().as_secs_f64());
    for e in &examples { eprintln!("  {e}"); }
}
