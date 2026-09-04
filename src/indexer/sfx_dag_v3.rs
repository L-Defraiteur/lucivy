//! DAG for SFX v3 index build and merge.
//!
//! Simpler than v2: no gapmap, no sibling table, no sepmap, no or_merge.
//!
//! Initial build DAG:
//! ```text
//! prepare_data ──┬── build_fst_v3 ───────┐
//!                └── build_sfxpost ───────┼── assemble_v3 → SfxBuildOutputV3
//! ```
//!
//! Merge DAG:
//! ```text
//! collect_tokens_v3 ──┬── build_fst_v3 ──────────┐
//!                     └── merge_sfxpost ──────────┼── write_v3
//! ```


use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
use crate::suffix_fst::collector_v3::{SfxCollectorDataV3, TokenMetaV3};
use crate::suffix_fst::file_v3::SfxFileWriterV3;
use crate::suffix_fst::termtexts_v3::TermTextsWriterV3;

/// Output of a v3 SFX build.
pub struct SfxBuildOutputV3 {
    /// .sfx file bytes (section-based, SFX3 format).
    pub sfx: Vec<u8>,
    /// .sfxpost file bytes (postings, same format as v2).
    pub sfxpost: Option<Vec<u8>>,
    /// .termtexts file bytes (TTX3 format with metadata).
    pub termtexts: Vec<u8>,
    /// Additional registry files: (extension, bytes).
    pub registry_files: Vec<(String, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// PrepareDataV3Node
// ---------------------------------------------------------------------------

struct PrepareDataV3Node {
    data: Option<SfxCollectorDataV3>,
}

impl Node for PrepareDataV3Node {
    fn node_type(&self) -> &'static str { "sfx_v3_prepare" }
    fn outputs(&self) -> Vec<PortDef> {
        // No `tokens` port: nothing connects it in this DAG, and publishing it
        // cloned the whole Vec<String> — one allocation per token, 335k of them
        // on a merged segment — straight into the bin. It also declared
        // BTreeSet<String> while holding a Vec<String>.
        vec![PortDef::required("collector_data", PortType::of::<SfxCollectorDataV3>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = self.data.take().ok_or("data already consumed")?;
        ctx.metric("tokens", data.tokens.len() as f64);
        ctx.set_output("collector_data", PortValue::new(data));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildFstV3Node
// ---------------------------------------------------------------------------

struct BuildFstV3Node;

impl Node for BuildFstV3Node {
    fn node_type(&self) -> &'static str { "sfx_v3_build_fst" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("collector_data", PortType::of::<SfxCollectorDataV3>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = ctx.input("collector_data")
            .ok_or("missing collector_data")?
            .downcast::<SfxCollectorDataV3>()
            .ok_or("wrong type")?;

        let mut builder = SuffixFstBuilderV3::with_min_suffix_len(data.min_suffix_len);
        // Chunk-level entries (partitions 0x00/0x01)
        // Extended ordinals: each unique extended text → its own FST ordinal.
        // Overlap variants have different ordinals, preventing FP from mixing.
        for &intern_ord in &data.sorted_indices {
            let meta = &data.token_meta[intern_ord as usize];
            if meta.is_word_stripped { continue; }
            let text = &data.token_texts[intern_ord as usize];
            let final_ord = data.intern_to_final[intern_ord as usize];
            builder.add_token(
                text,
                final_ord as u64,
                meta.own_len,
                meta.sep_len,
                meta.overlap_len,
                meta.is_word_start,
            );
        }
        // Word-level stripped entries (partition 0x02)
        // Each word-stripped has its own ordinal with aggregated postings
        // from all overlap variants of its first chunk.
        for ws in &data.word_stripped {
            let ws_ord = data.intern_to_final[ws.first_intern_ord as usize];
            builder.add_word_stripped(
                &ws.word_content,
                &ws.content_overlap,
                ws_ord as u64,
                ws.first_own_len,
                ws.last_sep_len,
                ws.is_word_start,
            );
        }

        // lucivy_fst::Error's Display hides the io::Error message ("I/O error");
        // surface the inner text, it is the only thing worth reading.
        let (fst_data, parent_data) = builder.build()
            .map_err(|e| match e {
                lucivy_fst::Error::Io(io) => format!("build_fst_v3: {io}"),
                other => format!("build_fst_v3: {other}"),
            })?;
        ctx.metric("fst_bytes", fst_data.len() as f64);
        ctx.set_output("fst", PortValue::new((fst_data, parent_data)));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildSfxPostV3Node — same posting format as v2
// ---------------------------------------------------------------------------

struct BuildSfxPostV3Node;

impl Node for BuildSfxPostV3Node {
    fn node_type(&self) -> &'static str { "sfx_v3_build_sfxpost" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("collector_data", PortType::of::<SfxCollectorDataV3>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = ctx.input("collector_data")
            .ok_or("missing collector_data")?
            .downcast::<SfxCollectorDataV3>()
            .ok_or("wrong type")?;

        let num_terms = data.num_content_ords;
        let mut writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (content_ord, postings) in data.content_postings.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in postings {
                writer.add_entry(content_ord as u32, doc_id, ti, bf, bt);
            }
        }
        let sfxpost_data = writer.finish();
        ctx.metric("sfxpost_bytes", sfxpost_data.len() as f64);
        ctx.set_output("sfxpost", PortValue::new(Some(sfxpost_data)));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AssembleV3Node — produce SfxBuildOutputV3
// ---------------------------------------------------------------------------

struct AssembleV3Node;

impl Node for AssembleV3Node {
    fn node_type(&self) -> &'static str { "sfx_v3_assemble" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>()),
            PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>()),
            PortDef::required("collector_data", PortType::of::<SfxCollectorDataV3>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("output", PortType::of::<SfxBuildOutputV3>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let (fst_data, parent_data) = ctx.take_input("fst")
            .ok_or("missing fst")?.take::<(Vec<u8>, Vec<u8>)>().ok_or("fst type")?;
        let sfxpost_data = ctx.take_input("sfxpost")
            .ok_or("missing sfxpost")?.take::<Option<Vec<u8>>>().ok_or("sfxpost type")?;
        let data = ctx.input("collector_data")
            .ok_or("missing collector_data")?
            .downcast::<SfxCollectorDataV3>()
            .ok_or("wrong type")?;

        // Build termtexts v3 (extended texts + metadata, keyed by final ordinal).
        // With extended ordinals, each unique extended text has its own ordinal,
        // including word-stripped entries.
        let termtexts = TermTextsWriterV3::from_collector_v3(data).serialize();

        // Build .sfx v3 file
        let sfx_writer = SfxFileWriterV3::new(fst_data, parent_data);
        let sfx = sfx_writer.to_bytes();

        // EventDriven registry indexes (posmap; bytemap is v2-only since 4 September 2026)
        let mut derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(
            &data.tokens,
            sfxpost_data.as_deref(),
            Some(&data.own_lens),
        );

        // Add word-level indexes to registry files
        derived.push(("word_pos_map".to_string(), data.word_pos_map.clone()));
        derived.push(("word_sfxpost".to_string(), data.word_sfxpost.clone()));
        derived.push(("sibling_v3".to_string(), data.sibling_v3.clone()));

        ctx.set_output("output", PortValue::new(SfxBuildOutputV3 {
            sfx,
            sfxpost: sfxpost_data,
            termtexts,
            registry_files: derived,
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public factory: build_initial_sfx_dag_v3
// ---------------------------------------------------------------------------

/// Build a DAG for initial SFX v3 index creation from collector data.
///
/// ```text
/// prepare ──┬── build_fst_v3 ───────┐
///           └── build_sfxpost ──────┼── assemble_v3 → SfxBuildOutputV3
/// ```
pub(crate) fn build_initial_sfx_dag_v3(
    data: SfxCollectorDataV3,
) -> Dag {
    let mut dag = Dag::new();

    dag.add_node("prepare", PrepareDataV3Node { data: Some(data) });

    dag.add_node("build_fst", BuildFstV3Node);
    dag.connect("prepare", "collector_data", "build_fst", "collector_data").unwrap();

    dag.add_node("build_sfxpost", BuildSfxPostV3Node);
    dag.connect("prepare", "collector_data", "build_sfxpost", "collector_data").unwrap();

    dag.add_node("assemble", AssembleV3Node);
    dag.connect("build_fst", "fst", "assemble", "fst").unwrap();
    dag.connect("build_sfxpost", "sfxpost", "assemble", "sfxpost").unwrap();
    dag.connect("prepare", "collector_data", "assemble", "collector_data").unwrap();

    dag
}

// ===========================================================================
// Merge support — reconstruct SfxCollectorDataV3 from termtexts v3 + sfxpost
// ===========================================================================

use crate::suffix_fst::section_file::detect_termtexts_version;
use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;

/// All persisted SFX v3 files of one source segment, plus its doc_id remap.
pub struct SegmentSfxV3<'a> {
    pub termtexts: &'a [u8],
    pub sfxpost: Option<&'a [u8]>,
    pub word_sfxpost: Option<&'a [u8]>,
    pub sibling_v3: Option<&'a [u8]>,
    /// old_doc_id → new_doc_id. Absent key = deleted document.
    pub doc_remap: &'a std::collections::HashMap<u32, u32>,
}

/// Merge v3 segments by REMAPPING ordinals, without re-tokenising anything.
///
/// The alternative — feeding the source text back through the collector — keeps a
/// single code path but rebuilds every intern table, posting list and word posting
/// in RAM: measured at ~18 GB to fuse 50k kernel documents into one segment. This
/// walks the persisted files instead and never holds more than the merged output.
///
/// What makes it possible without a format change is the partition tag now carried
/// in TTX3: a word-stripped entry stores `word_content + content_overlap` as its
/// text and `overlap_len` says where to cut, so both halves come back exactly.
/// Everything else — postings, siblings — is a remap of doc_ids and ordinals.
///
/// The intern key is `(is_word_stripped, text)`, NOT the text alone. Chunk and
/// word-stripped entries can share a text while their postings live in different
/// files with different coordinate semantics; keying on text alone is the exact
/// partition leak this codebase has been paying for since May.
pub fn merge_segments_v3(
    segments: &[SegmentSfxV3<'_>],
) -> Result<SfxCollectorDataV3, String> {
    use crate::suffix_fst::collector_v3::WordStrippedEntry;

    for (i, seg) in segments.iter().enumerate() {
        match detect_termtexts_version(seg.termtexts) {
            Some(3) => {}
            Some(v) => return Err(format!("segment {i}: termtexts version {v}, expected 3 — reindex required")),
            None => return Err(format!("segment {i}: invalid termtexts format")),
        }
    }

    // Phase timings, printed under V3_PROFILE. Merging is the slowest step in any
    // bench that wants a realistic index shape, so it gets the same treatment as
    // the query path: measure the phases, do not guess which one is heavy.
    let prof = crate::suffix_fst::briques::profile::enabled();
    let t_start = std::time::Instant::now();
    let ns_intern = 0u128;
    let mut ns_postings = 0u128;
    let mut ns_sibling = 0u128;

    // ── Arenas ─────────────────────────────────────────────────────────
    // Everything below is sized once from the sources and never
    // reallocated: the merge of 14 kernel segments interns ~650k tokens and
    // ~10M postings, and doing that with a String per key, a String per
    // token and a Vec per ordinal meant millions of small allocations —
    // 9 s of the 14 s a merge took in the browser, measurable natively too.
    //
    // - texts live in one byte arena, addressed by (start, len);
    // - the intern table is open addressing over a hash of (shape, text),
    //   compared against the arena, so a hit allocates nothing;
    // - chunk and word postings go to two flat vectors tagged with their
    //   intern ordinal, sorted once, then cut per ordinal.
    let term_counts: Vec<u32> = segments.iter()
        .map(|s| TermTextsReaderV3::open(s.termtexts).map(|t| t.num_terms()).unwrap_or(0))
        .collect();
    let total_terms: usize = term_counts.iter().map(|&n| n as usize).sum();
    let total_text_bytes: usize = segments.iter().map(|s| s.termtexts.len()).sum();

    let mut text_buf: Vec<u8> = Vec::with_capacity(total_text_bytes);
    let mut text_start: Vec<u32> = Vec::with_capacity(total_terms);
    let mut text_len: Vec<u32> = Vec::with_capacity(total_terms);
    let mut token_meta: Vec<TokenMetaV3> = Vec::with_capacity(total_terms);
    let mut token_hash: Vec<u64> = Vec::with_capacity(total_terms);

    // Distinct tokens never exceed the sum of the sources' terms, so a table
    // of twice that never needs to grow.
    let table_cap = (total_terms * 2).next_power_of_two().max(1024);
    let table_mask = table_cap - 1;
    let mut table: Vec<u32> = vec![u32::MAX; table_cap];

    fn shape_hash(is_ws: bool, own_len: u16, sep_len: u8, is_word_start: bool, text: &[u8]) -> u64 {
        use std::hash::Hasher;
        let mut h = rustc_hash::FxHasher::default();
        h.write_u8(is_ws as u8);
        h.write_u16(own_len);
        h.write_u8(sep_len);
        h.write_u8(is_word_start as u8);
        h.write(text);
        h.finish()
    }

    let mut chunk_post: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
    let mut word_post: Vec<(u32, crate::suffix_fst::word_sfxpost::WordPostingEntry)> = Vec::new();
    let mut sibling_pairs: Vec<(u32, u32)> = Vec::new();
    let mut wpm_writer = crate::suffix_fst::word_pos_map::WordPosMapWriter::new();

    for (seg_idx, seg) in segments.iter().enumerate() {
        let tt = TermTextsReaderV3::open(seg.termtexts)
            .ok_or_else(|| format!("segment {seg_idx}: failed to open termtexts v3"))?;
        let sfxpost = seg.sfxpost.and_then(SfxPostReaderV2::open_slice);
        let wsp = seg.word_sfxpost
            .and_then(crate::suffix_fst::word_sfxpost::WordSfxPostReader::open);

        let mut seg_ord_to_global: Vec<u32> = Vec::with_capacity(tt.num_terms() as usize);
        // The doc remap is consulted once per posting (millions per
        // segment): a dense table beats a hash lookup by a wide margin.
        let remap_len = seg.doc_remap.keys().max().map(|&d| d as usize + 1).unwrap_or(0);
        let mut remap: Vec<u32> = vec![u32::MAX; remap_len];
        for (&old, &new) in seg.doc_remap.iter() {
            remap[old as usize] = new;
        }
        let remap_doc = |d: u32| -> Option<u32> {
            match remap.get(d as usize) {
                Some(&n) if n != u32::MAX => Some(n),
                _ => None,
            }
        };
        if prof {
            let len = |b: Option<&[u8]>| b.map(|x| x.len()).unwrap_or(0);
            eprintln!(
                "  [merge] seg {seg_idx}: {} terms, {} docs kept | termtexts {} B, sfxpost {} B, word_sfxpost {} B, sibling {} B | global tokens so far {}",
                tt.num_terms(), seg.doc_remap.len(),
                seg.termtexts.len(), len(seg.sfxpost), len(seg.word_sfxpost), len(seg.sibling_v3),
                text_start.len(),
            );
        }
        let t_seg = std::time::Instant::now();
        for old_ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(old_ord)
                .ok_or_else(|| format!("segment {seg_idx}: missing entry at ordinal {old_ord}"))?;
            // Same shape key as the collector: a text carries one set of
            // (own_len, sep_len, is_word_start) per ordinal, never a winner's.
            let h = shape_hash(meta.is_word_stripped, meta.own_len, meta.sep_len, meta.is_word_start, text.as_bytes());
            let mut slot = (h as usize) & table_mask;
            let global_ord = loop {
                let cand = table[slot];
                if cand == u32::MAX {
                    let new_ord = text_start.len() as u32;
                    table[slot] = new_ord;
                    text_start.push(text_buf.len() as u32);
                    text_len.push(text.len() as u32);
                    text_buf.extend_from_slice(text.as_bytes());
                    token_hash.push(h);
                    token_meta.push(TokenMetaV3 {
                        own_len: meta.own_len,
                        sep_len: meta.sep_len,
                        overlap_len: meta.overlap_len,
                        is_word_start: meta.is_word_start,
                        // Only used while collecting (word-stripped grouping).
                        word_id: 0,
                        content_overlap: None,
                        is_word_stripped: meta.is_word_stripped,
                    });
                    break new_ord;
                }
                let c = cand as usize;
                if token_hash[c] == h {
                    let m = &token_meta[c];
                    let s = text_start[c] as usize;
                    let l = text_len[c] as usize;
                    if m.is_word_stripped == meta.is_word_stripped
                        && m.own_len == meta.own_len
                        && m.sep_len == meta.sep_len
                        && m.is_word_start == meta.is_word_start
                        && &text_buf[s..s + l] == text.as_bytes()
                    {
                        break cand;
                    }
                }
                slot = (slot + 1) & table_mask;
            };
            seg_ord_to_global.push(global_ord);

            // Chunk postings (.sfxpost) — chunk-level coordinates.
            if let Some(r) = &sfxpost {
                r.for_each_entry(old_ord, |doc_id, ti, bf, bt| {
                    if let Some(doc) = remap_doc(doc_id) {
                        chunk_post.push((global_ord, doc, ti, bf, bt));
                    }
                });
            }
            // Word postings (.word_sfxpost) — word-level coordinates, own file.
            if let Some(r) = &wsp {
                r.for_each_entry(old_ord, |e| {
                    if let Some(doc) = remap_doc(e.doc_id) {
                        word_post.push((global_ord, crate::suffix_fst::word_sfxpost::WordPostingEntry {
                            doc_id: doc, ..e
                        }));
                    }
                });
            }
        }

        ns_postings += t_seg.elapsed().as_nanos();

        // Sibling table: both ends are ordinals of THIS segment.
        let t_sib = std::time::Instant::now();
        if let Some(data) = seg.sibling_v3 {
            if let Some(sib) = crate::suffix_fst::sibling_table::SiblingTableReader::open(data) {
                for ord in 0..sib.num_ordinals().min(seg_ord_to_global.len() as u32) {
                    let from = seg_ord_to_global[ord as usize];
                    for e in sib.siblings(ord) {
                        if (e.next_ordinal as usize) < seg_ord_to_global.len() {
                            // A pre-SIB3 source carries the destination's content
                            // length here; META has it, the merged table drops it.
                            sibling_pairs.push((from, seg_ord_to_global[e.next_ordinal as usize]));
                        }
                    }
                }
            }
        }

        ns_sibling += t_sib.elapsed().as_nanos();

        // word_pos_map is not read from the sources: it is derived below from the
        // merged word postings, exactly as the collector derives it from its own.
    }
    drop(table);
    let ns_seg_loop = t_start.elapsed().as_nanos();
    let t_final = std::time::Instant::now();

    let text_of = |i: usize| -> &str {
        let s = text_start[i] as usize;
        let l = text_len[i] as usize;
        // The arena only ever receives whole `&str`s.
        unsafe { std::str::from_utf8_unchecked(&text_buf[s..s + l]) }
    };

    // Group the flat postings by ordinal with a stable counting pass: the
    // sources were walked in segment order and remapped doc ids grow with
    // the segment, and each source lists an ordinal's entries in (doc,
    // position) order, so every bucket comes out already sorted — the
    // order the per-ordinal sort used to produce, byte-identical output.
    // A bucket that is not sorted (it never happens; asserted in debug)
    // is sorted on the spot rather than trusted.
    let num_tokens = text_start.len();
    fn bucket_by_ordinal<T: Clone + Ord>(items: Vec<(u32, T)>, num: usize) -> (Vec<(u32, T)>, Vec<u32>) {
        let mut starts = vec![0u32; num + 1];
        for (o, _) in &items {
            starts[*o as usize + 1] += 1;
        }
        for o in 0..num {
            starts[o + 1] += starts[o];
        }
        let mut fill = starts.clone();
        let mut out: Vec<(u32, T)> = Vec::with_capacity(items.len());
        // SAFETY-free version: place through an index vector, then gather.
        let mut place: Vec<u32> = vec![0; items.len()];
        for (i, (o, _)) in items.iter().enumerate() {
            let slot = fill[*o as usize];
            fill[*o as usize] += 1;
            place[slot as usize] = i as u32;
        }
        for &i in &place {
            out.push(items[i as usize].clone());
        }
        drop(items);
        for o in 0..num {
            let (a, b) = (starts[o] as usize, starts[o + 1] as usize);
            let bucket = &mut out[a..b];
            if !bucket.windows(2).all(|w| w[0].1 <= w[1].1) {
                debug_assert!(false, "merge_segments_v3: bucket {o} arrived unsorted");
                bucket.sort_unstable();
            }
        }
        (out, starts)
    }
    let (chunk_post, chunk_starts) = {
        let tagged: Vec<(u32, (u32, u32, u32, u32))> = chunk_post.into_iter()
            .map(|(o, d, t, f, b)| (o, (d, t, f, b))).collect();
        bucket_by_ordinal(tagged, num_tokens)
    };
    let (word_post, word_starts) = bucket_by_ordinal(word_post, num_tokens);

    // Assign final ordinals in text order. Chunk and word-stripped entries keep
    // separate ordinals even when their texts match — that is the whole point.
    // The single-parent FST value holds a 24-bit ordinal. The build would
    // refuse the segment anyway; refuse here, before the derived indexes are
    // computed, and say which merge did it.
    if num_tokens as u64 > crate::suffix_fst::builder_v3::SuffixFstBuilderV3::MAX_ORDINAL {
        return Err(format!(
            "merge_segments_v3: {num_tokens} distinct terms across {} segments exceed the \
             {} ordinals the v3 encoding can address; merge fewer segments",
            segments.len(), crate::suffix_fst::builder_v3::SuffixFstBuilderV3::MAX_ORDINAL + 1));
    }
    let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
    sorted_indices.sort_unstable_by(|&a, &b| {
        text_of(a as usize).cmp(text_of(b as usize))
            .then(token_meta[a as usize].is_word_stripped.cmp(&token_meta[b as usize].is_word_stripped))
    });

    let mut intern_to_final = vec![0u32; num_tokens];
    for (final_ord, &io) in sorted_indices.iter().enumerate() {
        intern_to_final[io as usize] = final_ord as u32;
    }
    let final_count = num_tokens as u32;

    let mut token_texts: Vec<String> = Vec::with_capacity(num_tokens);
    for i in 0..num_tokens {
        token_texts.push(text_of(i).to_string());
    }
    let mut tokens: Vec<String> = Vec::with_capacity(num_tokens);
    let mut own_lens: Vec<u16> = Vec::with_capacity(num_tokens);
    let mut content_postings: Vec<Vec<(u32, u32, u32, u32)>> = Vec::with_capacity(num_tokens);
    let mut wsp_writer = crate::suffix_fst::word_sfxpost::WordSfxPostWriter::new(num_tokens);
    let mut word_stripped: Vec<WordStrippedEntry> = Vec::new();

    for &io in &sorted_indices {
        let i = io as usize;
        let fo = intern_to_final[i];
        tokens.push(token_texts[i].clone());
        own_lens.push(token_meta[i].own_len);

        let (cs, ce) = (chunk_starts[i] as usize, chunk_starts[i + 1] as usize);
        let mut p: Vec<(u32, u32, u32, u32)> = chunk_post[cs..ce].iter().map(|&(_, e)| e).collect();
        p.dedup();
        content_postings.push(p);

        let (ws, we) = (word_starts[i] as usize, word_starts[i + 1] as usize);
        let mut prev: Option<&crate::suffix_fst::word_sfxpost::WordPostingEntry> = None;
        for (_, e) in &word_post[ws..we] {
            if prev == Some(e) { continue; }
            prev = Some(e);
            wpm_writer.add_word(e.doc_id, e.first_position, e.last_position, fo);
            wsp_writer.add(fo, e.clone());
        }

        if token_meta[i].is_word_stripped {
            // The 0x02 key is word_content + content_overlap; overlap_len says
            // where to cut. Nothing else is needed to re-emit the FST entry.
            let text = &token_texts[i];
            let ovl = (token_meta[i].overlap_len as usize).min(text.len());
            let split = text.len() - ovl;
            let split = (0..=split).rev().find(|&b| text.is_char_boundary(b)).unwrap_or(0);
            word_stripped.push(WordStrippedEntry {
                word_content: text[..split].to_string(),
                content_overlap: text[split..].to_string(),
                first_intern_ord: io,
                first_chunk_intern_ord: io,
                last_chunk_intern_ord: io,
                first_own_len: token_meta[i].own_len,
                last_sep_len: token_meta[i].sep_len,
                is_word_start: token_meta[i].is_word_start,
                num_chunks: 1,
            });
        }
    }

    let mut sibling_writer =
        crate::suffix_fst::sibling_table::SiblingTableWriter::new(final_count);
    for &(a, b) in &sibling_pairs {
        sibling_writer.add(intern_to_final[a as usize], intern_to_final[b as usize], 0);
    }

    if prof {
        let ms = |n: u128| n as f64 / 1e6;
        eprintln!(
            "  [merge] {} segments, {} tokens | seg loop {:.0}ms (intern+postings {:.0}, sibling {:.0}) | finalize {:.0}ms | writers {:.0}ms",
            segments.len(), num_tokens,
            ms(ns_seg_loop), ms(ns_postings + ns_intern), ms(ns_sibling),
            ms(t_final.elapsed().as_nanos()),
            ms(t_start.elapsed().as_nanos() - ns_seg_loop),
        );
    }

    let total_docs = segments.iter()
        .flat_map(|s| s.doc_remap.values().copied())
        .max().map(|m| m + 1).unwrap_or(0);

    Ok(SfxCollectorDataV3 {
        tokens,
        sorted_indices,
        intern_to_final,
        token_texts,
        content_postings,
        own_lens,
        num_content_ords: final_count as usize,
        token_meta,
        num_docs: total_docs,
        min_suffix_len: 1,
        word_stripped,
        word_sfxpost: wsp_writer.finish(),
        word_pos_map: wpm_writer.serialize(),
        sibling_v3: sibling_writer.serialize(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suffix_fst::collector_v3::SfxCollectorV3;
    use crate::suffix_fst::file_v3::SfxFileReaderV3;
    use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;

    #[test]
    fn test_initial_build_dag() {
        let mut collector = SfxCollectorV3::new();
        collector.begin_doc();
        collector.add_value("mutex_lock_init");
        collector.end_doc();
        collector.begin_doc();
        collector.add_value("hello_world");
        collector.end_doc();

        let data = collector.into_data();
        let mut dag = build_initial_sfx_dag_v3(data);

        let mut result = luciole::execute_dag(&mut dag, None)
            .expect("DAG execution should succeed");

        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output")
            .expect("should have output");

        // Verify .sfx is readable
        let reader = SfxFileReaderV3::open(&output.sfx)
            .expect("should open sfx v3");
        assert!(reader.num_suffix_terms() > 0);

        // Verify cross-boundary trigram "x_l": key `x_`, overlap `lo` in its record
        let parents = reader.resolve_suffix("x_");
        assert!(parents.iter().any(|p| &p.overlap[..2] == b"lo"), "x_ + lo should be in the FST");

        // Verify termtexts
        let tt = TermTextsReaderV3::open(&output.termtexts)
            .expect("should open termtexts v3");
        assert!(tt.num_terms() > 0);
        // All entries should have text + metadata
        for ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(ord).expect("entry should exist");
            assert!(!text.is_empty());
            assert!(meta.own_len > 0 || meta.sep_len > 0);
        }

        // Verify sfxpost exists
        assert!(output.sfxpost.is_some());
    }

    #[test]
    fn test_dag_empty_doc() {
        let mut collector = SfxCollectorV3::new();
        collector.begin_doc();
        collector.add_value("test");
        collector.end_doc();
        collector.begin_doc();
        collector.end_doc_empty();

        let data = collector.into_data();
        let mut dag = build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();
        assert!(reader.num_suffix_terms() > 0);
    }

    #[test]
    fn test_dag_multi_value() {
        let mut collector = SfxCollectorV3::new();
        collector.begin_doc();
        collector.add_value("mutex_lock");
        collector.add_value("hello_world");
        collector.end_doc();

        let data = collector.into_data();
        let mut dag = build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();
        // Both values should be indexed
        assert!(!reader.resolve_suffix("mutex_").is_empty());
        assert!(!reader.resolve_suffix("hello_").is_empty());
    }

    #[test]
    fn test_termtexts_metadata_matches_builder() {
        let mut collector = SfxCollectorV3::new();
        collector.begin_doc();
        collector.add_value("mutex_lock");
        collector.end_doc();

        let data = collector.into_data();
        let mut dag = build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let tt = TermTextsReaderV3::open(&output.termtexts).unwrap();
        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();

        // For each term in termtexts, resolving the suffix should work
        for ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(ord).unwrap();
            // The key stops at the token boundary: own bytes for a chunk, the
            // content for a word entry; the overlap is in the record.
            let key_text = if meta.is_word_stripped {
                &text[..text.len() - meta.overlap_len as usize]
            } else {
                &text[..meta.own_len as usize]
            };
            let parents = reader.resolve_suffix(key_text);
            // At least SI=0 should exist for this token
            assert!(
                parents.iter().any(|p| p.sti == 0),
                "ordinal {ord} text '{text}' should have SI=0 entry"
            );
            // Metadata should match
            let p = parents.iter().find(|p| p.sti == 0).unwrap();
            assert_eq!(p.own_len, meta.own_len, "own_len mismatch for '{text}'");
            assert_eq!(p.sep_len, meta.sep_len, "sep_len mismatch for '{text}'");
            assert_eq!(p.overlap_len, meta.overlap_len, "overlap_len mismatch for '{text}'");
            assert_eq!(p.is_word_start, meta.is_word_start, "is_word_start mismatch for '{text}'");
        }
    }

    // ── Merge tests ──

    /// Helper: build a segment's outputs (termtexts + sfxpost bytes) from text values.
    fn build_segment(texts: &[&str]) -> SfxBuildOutputV3 {
        let mut collector = SfxCollectorV3::new();
        for text in texts {
            collector.begin_doc();
            collector.add_value(text);
            collector.end_doc();
        }
        let data = collector.into_data();
        let mut dag = build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap()
    }

    /// View a build output as a merge input, pulling the registry files by name.
    fn as_merge_input<'a>(
        out: &'a SfxBuildOutputV3,
        remap: &'a std::collections::HashMap<u32, u32>,
    ) -> SegmentSfxV3<'a> {
        let f = |ext: &str| out.registry_files.iter()
            .find(|(e, _)| e == ext).map(|(_, d)| d.as_slice());
        SegmentSfxV3 {
            termtexts: &out.termtexts,
            sfxpost: out.sfxpost.as_deref(),
            word_sfxpost: f("word_sfxpost"),
            sibling_v3: f("sibling_v3"),
            doc_remap: remap,
        }
    }

    #[test]
    fn test_merge_two_segments() {
        let seg_a = build_segment(&["mutex_lock", "hello_world"]);
        let seg_b = build_segment(&["mutex_core", "foo_bar"]);

        // Doc remapping: seg_a docs 0,1 → 0,1; seg_b docs 0,1 → 2,3
        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0), (1, 1)].into();
        let remap_b: std::collections::HashMap<u32, u32> = [(0, 2), (1, 3)].into();

        let merged_data = merge_segments_v3(&[
            as_merge_input(&seg_a, &remap_a),
            as_merge_input(&seg_b, &remap_b),
        ]).unwrap();

        // Rebuild from merged data
        let mut dag = build_initial_sfx_dag_v3(merged_data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();

        // All tokens from both segments should be present
        let overlaps = |key: &str| -> Vec<[u8; 2]> {
            reader.resolve_suffix(key).iter().map(|p| [p.overlap[0], p.overlap[1]]).collect()
        };
        assert!(overlaps("mutex_").contains(b"lo"), "mutex_lo from seg_a");
        assert!(overlaps("mutex_").contains(b"co"), "mutex_co from seg_b");
        assert!(overlaps("hello_").contains(b"wo"), "hello_wo from seg_a");
        assert!(overlaps("foo_").contains(b"ba"), "foo_ba from seg_b");
    }

    #[test]
    fn test_merge_shared_tokens() {
        // Both segments have "mutex_lock" → same extended tokens
        let seg_a = build_segment(&["mutex_lock"]);
        let seg_b = build_segment(&["mutex_lock"]);

        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0)].into();
        let remap_b: std::collections::HashMap<u32, u32> = [(0, 1)].into();

        let merged_data = merge_segments_v3(&[
            as_merge_input(&seg_a, &remap_a),
            as_merge_input(&seg_b, &remap_b),
        ]).unwrap();

        // Shared tokens should have merged postings
        // "mutex_lo" should have postings from both doc 0 and doc 1
        let intern_ord = merged_data.token_texts.iter()
            .position(|t| t == "mutex_lo")
            .expect("mutex_lo should exist");
        let content_ord = merged_data.intern_to_final[intern_ord] as usize;
        let postings = &merged_data.content_postings[content_ord];
        let doc_ids: std::collections::HashSet<u32> = postings.iter().map(|p| p.0).collect();
        assert!(doc_ids.contains(&0), "should have doc 0");
        assert!(doc_ids.contains(&1), "should have doc 1");
    }

    #[test]
    fn test_merge_with_deleted_docs() {
        let seg_a = build_segment(&["mutex_lock", "hello_world", "foo_bar"]);

        // Only remap docs 0 and 2, doc 1 is deleted
        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0), (2, 1)].into();

        let merged_data = merge_segments_v3(&[as_merge_input(&seg_a, &remap_a)]).unwrap();

        // "hello_wo" was in doc 1 which is deleted → its postings should only
        // contain docs that are in the remap
        let has_hello = merged_data.token_texts.iter().any(|t| t == "hello_wo");
        if has_hello {
            let intern_ord = merged_data.token_texts.iter().position(|t| t == "hello_wo").unwrap();
            let content_ord = merged_data.intern_to_final[intern_ord] as usize;
            let postings = &merged_data.content_postings[content_ord];
            // Doc 1 was deleted, so no postings should reference the deleted doc
            for p in postings {
                assert_ne!(p.0, 1, "deleted doc should not be in postings");
            }
        }
    }

    #[test]
    fn test_merge_preserves_metadata() {
        let seg_a = build_segment(&["mutex_lock"]);

        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0)].into();

        let merged_data = merge_segments_v3(&[as_merge_input(&seg_a, &remap_a)]).unwrap();

        // Rebuild and check metadata survives the round-trip
        let mut dag = build_initial_sfx_dag_v3(merged_data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let tt = TermTextsReaderV3::open(&output.termtexts).unwrap();
        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();

        for ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(ord).unwrap();
            // Word-stripped entries (partition 0x02) are deliberately absent from
            // the chunk partitions — BuildFstV3Node skips them. Before the tag was
            // persisted in TTX3, the merge path marked every entry as chunk, so
            // they were wrongly fed to add_token and did resolve here. They must
            // not any more.
            if meta.is_word_stripped { continue; }
            let parents = reader.resolve_suffix(&text[..meta.own_len as usize]);
            let p = parents.iter().find(|p| p.sti == 0 && p.overlap[..meta.overlap_len as usize] == text.as_bytes()[meta.own_len as usize..])
                .unwrap_or_else(|| panic!("no sti=0 parent for chunk entry '{text}'"));
            assert_eq!(p.own_len, meta.own_len, "own_len roundtrip for '{text}'");
            assert_eq!(p.sep_len, meta.sep_len, "sep_len roundtrip for '{text}'");
            assert_eq!(p.overlap_len, meta.overlap_len, "overlap roundtrip for '{text}'");
        }
    }
}
