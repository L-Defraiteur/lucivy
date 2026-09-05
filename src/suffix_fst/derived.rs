//! The three derived sidecars of a v3 segment, rebuilt from its postings.
//!
//! `.posmap` is the inverse of `.sfxpost` (position → chunk ordinal, plus
//! the byte checkpoints of `PMP4`), `.word_pos_map` the inverse of
//! `.word_sfxpost` (position → the word starting there), `.sibling_v3` the
//! links between consecutive tokens of a value — chunks that follow each
//! other, words that follow each other. None holds a byte the postings and
//! the texts' META do not: together they were 27 % of a kernel index (1.6 GB
//! of 4.9). An index created with `derived_in_ram: true` does not write them;
//! its segments rebuild them here, in RAM, when they are opened — before any
//! query, never lazily: a first query that pays for what the others do not
//! would lie about the engine's speed.
//!
//! The rebuild reproduces the files **byte for byte** — the same writers,
//! fed in the same order as the collector and the merges feed them
//! (`rebuild_matches_the_collector`, and `derived_files_match_the_index` on
//! a whole index) — so a reader cannot tell a rebuilt sidecar from a
//! written one, and a segment written with the files and one without answer
//! alike.
//!
//! The price is where the bytes live: a mapped file costs RAM only where a
//! query touches it, a rebuilt structure is resident whole, and the rebuild
//! reads every posting of the segment. Hence an option, never the default.

use super::posmap::PosMapWriter;
use super::sfxpost_v2::SfxPostReaderV2;
use super::sibling_table::SiblingTableWriter;
use super::word_pos_map::WordPosMapWriter;
use super::word_sfxpost::WordSfxPostReader;

/// The three sidecars an index with `derived_in_ram` does not write.
pub const DERIVED_EXTENSIONS: [&str; 3] = ["posmap", "word_pos_map", "sibling_v3"];

/// The rebuilt sidecars of one segment and field, as the file slices a
/// reader opens — built when the segment reader opens (`SegmentReader::open`),
/// kept in the index's cache (`Index::derived_cache`) so a reload does not
/// rebuild the segments it already had.
#[derive(Clone)]
pub struct DerivedSlices {
    /// `.posmap` (`PMP4`).
    pub posmap: common::file_slice::FileSlice,
    /// `.word_pos_map` (`WMP3`).
    pub word_pos_map: common::file_slice::FileSlice,
    /// `.sibling_v3` (`SIB4`).
    pub sibling_v3: common::file_slice::FileSlice,
}

impl Default for DerivedSlices {
    fn default() -> Self {
        use common::file_slice::FileSlice;
        Self { posmap: FileSlice::empty(), word_pos_map: FileSlice::empty(), sibling_v3: FileSlice::empty() }
    }
}

impl From<DerivedFiles> for DerivedSlices {
    fn from(f: DerivedFiles) -> Self {
        use common::file_slice::FileSlice;
        let slice = |v: Vec<u8>| FileSlice::new(std::sync::Arc::new(common::OwnedBytes::new(v)));
        Self { posmap: slice(f.posmap), word_pos_map: slice(f.word_pos_map), sibling_v3: slice(f.sibling_v3) }
    }
}

/// The rebuilt sidecars, as the bytes of the files they replace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedFiles {
    /// `.posmap`, layout `PMP4` (byte checkpoints from `own_len`).
    pub posmap: Vec<u8>,
    /// `.word_pos_map`, layout `WMP3`.
    pub word_pos_map: Vec<u8>,
    /// `.sibling_v3`, layout `SIB4`.
    pub sibling_v3: Vec<u8>,
}

/// Rebuild the three sidecars of one segment and field from its `.sfxpost`,
/// its `.word_sfxpost` (absent on a segment without word entries) and the
/// `own_len` of its ordinals — by **local** ordinal, the one the postings
/// use; on a dictionary segment the caller translates through the `.gmap`
/// before asking the shard's META.
pub fn rebuild(
    sfxpost: &SfxPostReaderV2,
    word_sfxpost: Option<&WordSfxPostReader<'_>>,
    own_len: &dyn Fn(u32) -> Option<u16>,
) -> DerivedFiles {
    let num_ordinals = sfxpost.num_terms();

    // .posmap: every chunk occurrence, in ordinal order, as the registry
    // builder feeds `PosMapIndex::on_posting`.
    let mut posmap = PosMapWriter::new();
    for ord in 0..num_ordinals {
        sfxpost.for_each_position(ord, |doc, ti| posmap.add(doc, ti, ord));
    }
    let posmap_bytes = posmap.serialize_with_lens(Some(own_len));

    // .word_pos_map: every word occurrence, in ordinal order, as the
    // collector and the merges feed `add_word`. Kept for the sibling pairs:
    // (doc, first, last, ordinal).
    let mut wpm = WordPosMapWriter::new();
    let mut words: Vec<(u32, u32, u32, u32)> = Vec::new();
    if let Some(wsp) = word_sfxpost {
        for ord in 0..wsp.num_ordinals().min(num_ordinals) {
            wsp.for_each_entry(ord, |e| {
                wpm.add_word(e.doc_id, e.first_position, e.last_position, ord);
                words.push((e.doc_id, e.first_position, e.last_position, ord));
            });
        }
    }
    let word_pos_map = wpm.serialize();

    // .sibling_v3: consecutive chunks of a value (consecutive positions, both
    // held — a value boundary is an empty position), and consecutive words
    // of a value. The collector links the words it interns in text order,
    // tail entries excluded: a tail shares its word's last position and
    // starts after it, so of two entries ending on one position the word is
    // the one starting first.
    let pm = super::posmap::PosMapReader::open(&posmap_bytes);
    let mut sib = SiblingTableWriter::new(num_ordinals);
    if let Some(pm) = &pm {
        for doc in 0..pm.num_docs() {
            let n = pm.num_tokens(doc);
            let mut prev: Option<u32> = None;
            for p in 0..n {
                let cur = pm.ordinal_at(doc, p);
                if let (Some(a), Some(b)) = (prev, cur) {
                    sib.add(a, b, 0);
                }
                prev = cur;
            }
        }
        words.sort_unstable();
        // Main words only: one entry per (doc, last position), the earliest.
        let mut main: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(words.len());
        for w in &words {
            match main.last() {
                Some(m) if m.0 == w.0 && m.2 == w.2 && m.1 <= w.1 => {}
                _ => main.push(*w),
            }
        }
        for pair in main.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a.0 != b.0 { continue; }
            // Same value: no empty position between the two words.
            let same_value = (a.2 + 1..b.1).all(|p| pm.ordinal_at(a.0, p).is_some());
            if same_value {
                sib.add(a.3, b.3, 0);
            }
        }
    }
    let sibling_v3 = sib.serialize();

    DerivedFiles { posmap: posmap_bytes, word_pos_map, sibling_v3 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::sfxpost_v2::SfxPostWriterV2;

    /// A segment as the collector writes it: the postings and the three
    /// sidecars, from documents of several values, an empty one, a line of
    /// Chinese long enough for a tail entry, and repeated words.
    fn collect(docs: &[&[&str]]) -> (Vec<u8>, Vec<u8>, DerivedFiles, Vec<u16>) {
        let mut c = SfxCollectorV3::new();
        for values in docs {
            c.begin_doc();
            for v in *values { c.add_value(v); }
            c.end_doc();
        }
        let data = c.into_data();
        let mut pw = SfxPostWriterV2::positions_only(data.num_content_ords);
        for (ord, postings) in data.content_postings.iter().enumerate() {
            for &(d, ti) in postings { pw.add_position(ord as u32, d, ti); }
        }
        let sfxpost = pw.finish();
        let derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(
            &data.tokens, Some(&sfxpost), Some(&data.own_lens));
        let posmap = derived.iter().find(|(e, _)| e == "posmap").map(|(_, d)| d.clone()).unwrap_or_default();
        let written = DerivedFiles { posmap, word_pos_map: data.word_pos_map.clone(), sibling_v3: data.sibling_v3.clone() };
        (sfxpost, data.word_sfxpost.clone(), written, data.own_lens.clone())
    }

    #[test]
    fn rebuild_matches_the_collector() {
        let long_cjk = format!("{}解。\n\n.. toctree::\n", "可以理".repeat(30));
        let docs: Vec<Vec<&str>> = vec![
            vec!["mutex_lock(&dev->lock);\n\treturn -ENOMEM;"],
            vec!["hello_world", "foo_bar baz", "hello_world again"],
            vec![],
            vec!["", "spin lock init; spin_lock_init(x)"],
            vec![long_cjk.as_str(), "après le mot long, déjà vu"],
            vec!["a________b   c  d", "rag3weaver rag3_weaver rag3-weaver"],
        ];
        let refs: Vec<&[&str]> = docs.iter().map(|d| d.as_slice()).collect();
        let (sfxpost, wsp, written, own_lens) = collect(&refs);
        let sp = SfxPostReaderV2::open_slice(&sfxpost).unwrap();
        let wr = WordSfxPostReader::open(&wsp).unwrap();
        let rebuilt = rebuild(&sp, Some(&wr), &|o| own_lens.get(o as usize).copied());
        assert_eq!(&rebuilt.posmap[0..4], b"PMP4");
        assert_eq!(rebuilt.posmap, written.posmap, ".posmap differs");
        assert_eq!(rebuilt.word_pos_map, written.word_pos_map, ".word_pos_map differs");
        assert_eq!(rebuilt.sibling_v3, written.sibling_v3, ".sibling_v3 differs");
        assert!(!rebuilt.sibling_v3.is_empty() && !rebuilt.word_pos_map.is_empty());
    }

    /// `LUCIVY_DERIVED_DIR` names an index directory (one shard): every
    /// segment's three sidecars are rebuilt from its postings and compared,
    /// byte for byte, to the files on disk. Dictionary segments translate
    /// their local ordinals through the `.gmap` to read the shard's META.
    #[test]
    #[ignore]
    fn derived_files_match_the_index() {
        use crate::suffix_fst::gmap::GmapReader;
        use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;
        let Ok(dir) = std::env::var("LUCIVY_DERIVED_DIR") else { return };
        let read = |name: &str| std::fs::read(std::path::Path::new(&dir).join(name)).ok();
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string())).collect();
        names.sort();
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
        let (mut segments, mut same, mut differ) = (0usize, [0usize; 3], [0usize; 3]);
        let t0 = std::time::Instant::now();
        for n in &names {
            let Some(prefix) = n.strip_suffix(".sfxpost") else { continue };
            if prefix.starts_with("dict-") { continue }
            let field: u32 = prefix.rsplit('.').next().and_then(|f| f.parse().ok()).unwrap_or(0);
            let Some(sfx) = read(n) else { continue };
            let wsp = read(&format!("{prefix}.word_sfxpost"));
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
            let (Some(texts), Some(sp)) = (texts, SfxPostReaderV2::open_slice(&sfx)) else { continue };
            let wr = wsp.as_deref().and_then(WordSfxPostReader::open);
            let own_len = |local: u32| {
                let g = gmap.as_ref().map(|g| g.global(local)).unwrap_or(local);
                texts.meta(g).map(|m| m.own_len)
            };
            let rebuilt = rebuild(&sp, wr.as_ref(), &own_len);
            segments += 1;
            for (i, (ext, bytes)) in [("posmap", &rebuilt.posmap), ("word_pos_map", &rebuilt.word_pos_map), ("sibling_v3", &rebuilt.sibling_v3)].iter().enumerate() {
                // A managed file ends with the directory's footer (CRC,
                // version), which a reader never sees: compare the body.
                let raw = read(&format!("{prefix}.{ext}")).unwrap_or_default();
                let on_disk = crate::directory::footer::Footer::extract_footer(crate::directory::FileSlice::from(raw.clone()))
                    .ok().and_then(|(_, body)| body.read_bytes().ok()).map(|b| b.to_vec()).unwrap_or(raw);
                if &on_disk == *bytes { same[i] += 1 } else {
                    differ[i] += 1;
                    if differ[i] <= 3 { eprintln!("{prefix}.{ext}: rebuilt {} B, on disk {} B", bytes.len(), on_disk.len()); }
                }
            }
        }
        eprintln!("{segments} segments in {:.1} s: identical posmap {}/{}, word_pos_map {}/{}, sibling_v3 {}/{}",
            t0.elapsed().as_secs_f64(), same[0], same[0] + differ[0], same[1], same[1] + differ[1], same[2], same[2] + differ[2]);
        assert_eq!(differ, [0, 0, 0]);
    }
}
