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

use std::collections::BTreeSet;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
use crate::suffix_fst::collector_v3::{SfxCollectorDataV3, TokenMetaV3};
use crate::suffix_fst::file_v3::SfxFileWriterV3;
use crate::suffix_fst::termtexts_v3::{TermMetaV3, TermTextsWriterV3};

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
        vec![
            PortDef::required("tokens", PortType::of::<BTreeSet<String>>()),
            PortDef::required("collector_data", PortType::of::<SfxCollectorDataV3>()),
        ]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = self.data.take().ok_or("data already consumed")?;
        ctx.metric("tokens", data.tokens.len() as f64);
        ctx.set_output("tokens", PortValue::new(data.tokens.clone()));
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

        let (fst_data, parent_data) = builder.build()
            .map_err(|e| format!("build_fst_v3: {e}"))?;
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

        let num_terms = data.tokens.len();
        let mut writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(num_terms);
        for (final_ord, &old_ord) in data.sorted_indices.iter().enumerate() {
            for &(doc_id, ti, bf, bt) in &data.token_postings[old_ord as usize] {
                writer.add_entry(final_ord as u32, doc_id, ti, bf, bt);
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

struct AssembleV3Node {
    num_docs: u32,
}

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

        // Build termtexts v3 (extended texts + metadata)
        let mut tt_writer = TermTextsWriterV3::new();
        for (final_ord, &intern_ord) in data.sorted_indices.iter().enumerate() {
            let text = &data.token_texts[intern_ord as usize];
            let meta = &data.token_meta[intern_ord as usize];
            tt_writer.add(final_ord as u32, text, TermMetaV3 {
                own_len: meta.own_len,
                sep_len: meta.sep_len,
                overlap_len: meta.overlap_len,
                is_word_start: meta.is_word_start,
            });
        }
        let termtexts = tt_writer.serialize();

        // Build .sfx v3 file
        let sfx_writer = SfxFileWriterV3::new(fst_data, parent_data, self.num_docs);
        // TODO: word_map and next_word will be added when we wire the collector to track them
        let sfx = sfx_writer.to_bytes();

        // EventDriven registry indexes (bytemap, freqmap, posmap, termtexts-v2-compat)
        // V3: pass own_len per token so ByteMap excludes overlap bytes
        let own_lens: Vec<u16> = data.sorted_indices.iter()
            .map(|&intern_ord| data.token_meta[intern_ord as usize].own_len)
            .collect();
        let derived = crate::suffix_fst::index_registry::build_derived_indexes_v3(
            &data.tokens,
            sfxpost_data.as_deref(),
            Some(&own_lens),
        );

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
    let num_docs = data.num_docs;
    let mut dag = Dag::new();

    dag.add_node("prepare", PrepareDataV3Node { data: Some(data) });

    dag.add_node("build_fst", BuildFstV3Node);
    dag.connect("prepare", "collector_data", "build_fst", "collector_data").unwrap();

    dag.add_node("build_sfxpost", BuildSfxPostV3Node);
    dag.connect("prepare", "collector_data", "build_sfxpost", "collector_data").unwrap();

    dag.add_node("assemble", AssembleV3Node { num_docs });
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

/// Merge data from multiple v3 segments into a single SfxCollectorDataV3.
///
/// Reads termtexts v3 (extended tokens + metadata) and sfxpost (postings)
/// from each source segment, remaps doc_ids, and produces a merged dataset
/// ready for `build_initial_sfx_dag_v3`.
///
/// `doc_id_remaps[seg_idx]` maps old_doc_id → new_doc_id for each segment.
pub fn merge_segments_v3(
    termtexts_per_segment: &[&[u8]],
    sfxpost_per_segment: &[Option<&[u8]>],
    doc_id_remaps: &[&std::collections::HashMap<u32, u32>],
) -> Result<SfxCollectorDataV3, String> {
    use std::collections::{BTreeSet, HashMap};

    // Validate all termtexts are v3
    for (i, tt_bytes) in termtexts_per_segment.iter().enumerate() {
        match detect_termtexts_version(tt_bytes) {
            Some(3) => {}
            Some(v) => return Err(format!("segment {i}: termtexts version {v}, expected 3 — reindex required")),
            None => return Err(format!("segment {i}: invalid termtexts format")),
        }
    }

    // Global intern map: extended_text → intern_id
    let mut global_intern: HashMap<String, u32> = HashMap::new();
    let mut token_texts: Vec<String> = Vec::new();
    let mut token_meta: Vec<TokenMetaV3> = Vec::new();
    let mut token_postings: Vec<Vec<(u32, u32, u32, u32)>> = Vec::new();

    for (seg_idx, tt_bytes) in termtexts_per_segment.iter().enumerate() {
        let tt = TermTextsReaderV3::open(tt_bytes)
            .ok_or_else(|| format!("segment {seg_idx}: failed to open termtexts v3"))?;

        let doc_remap = doc_id_remaps[seg_idx];

        // Read sfxpost for this segment (if present)
        let sfxpost_reader: Option<SfxPostReaderV2> = sfxpost_per_segment[seg_idx]
            .and_then(|data| SfxPostReaderV2::open_slice(data));

        // Build old_ordinal → new_ordinal mapping for this segment
        let mut seg_ord_to_global: Vec<u32> = Vec::with_capacity(tt.num_terms() as usize);

        for old_ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(old_ord)
                .ok_or_else(|| format!("segment {seg_idx}: missing entry at ordinal {old_ord}"))?;

            let global_ord = if let Some(&existing) = global_intern.get(text) {
                existing
            } else {
                let new_ord = token_texts.len() as u32;
                global_intern.insert(text.to_string(), new_ord);
                token_texts.push(text.to_string());
                token_meta.push(TokenMetaV3 {
                    own_len: meta.own_len,
                    sep_len: meta.sep_len,
                    overlap_len: meta.overlap_len,
                    is_word_start: meta.is_word_start,
                    word_id: 0, // word_id is segment-local, not meaningful across merge
                });
                token_postings.push(Vec::new());
                new_ord
            };
            seg_ord_to_global.push(global_ord);
        }

        // Remap postings from this segment
        if let Some(reader) = &sfxpost_reader {
            for old_ord in 0..tt.num_terms() {
                let global_ord = seg_ord_to_global[old_ord as usize];
                let entries = reader.entries(old_ord);
                for entry in entries {
                    if let Some(&new_doc_id) = doc_remap.get(&entry.doc_id) {
                        token_postings[global_ord as usize].push((
                            new_doc_id,
                            entry.token_index,
                            entry.byte_from,
                            entry.byte_to,
                        ));
                    }
                    // If doc_id not in remap → doc was deleted, skip
                }
            }
        }
    }

    // Build sorted output
    let num_tokens = token_texts.len();
    let mut sorted_indices: Vec<u32> = (0..num_tokens as u32).collect();
    sorted_indices.sort_by(|&a, &b| {
        token_texts[a as usize].cmp(&token_texts[b as usize])
    });

    let mut intern_to_final = vec![0u32; num_tokens];
    for (new_ord, &old_ord) in sorted_indices.iter().enumerate() {
        intern_to_final[old_ord as usize] = new_ord as u32;
    }

    let tokens: BTreeSet<String> = sorted_indices.iter()
        .map(|&old_ord| token_texts[old_ord as usize].clone())
        .collect();

    let total_docs = doc_id_remaps.iter()
        .map(|m| m.values().copied().max().unwrap_or(0) + 1)
        .max()
        .unwrap_or(0);

    Ok(SfxCollectorDataV3 {
        tokens,
        sorted_indices,
        intern_to_final,
        token_texts,
        token_postings,
        token_meta,
        num_docs: total_docs,
        min_suffix_len: 1,
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

        // Verify cross-boundary trigram "x_l" exists
        let parents = reader.resolve_suffix("x_lo");
        assert!(!parents.is_empty(), "x_lo should be in FST via overlap");

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
        assert!(!reader.resolve_suffix("mutex_lo").is_empty());
        assert!(!reader.resolve_suffix("hello_wo").is_empty());
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
            let parents = reader.resolve_suffix(text);
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

    #[test]
    fn test_merge_two_segments() {
        let seg_a = build_segment(&["mutex_lock", "hello_world"]);
        let seg_b = build_segment(&["mutex_core", "foo_bar"]);

        // Doc remapping: seg_a docs 0,1 → 0,1; seg_b docs 0,1 → 2,3
        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0), (1, 1)].into();
        let remap_b: std::collections::HashMap<u32, u32> = [(0, 2), (1, 3)].into();

        let merged_data = merge_segments_v3(
            &[&seg_a.termtexts, &seg_b.termtexts],
            &[seg_a.sfxpost.as_deref(), seg_b.sfxpost.as_deref()],
            &[&remap_a, &remap_b],
        ).unwrap();

        // Rebuild from merged data
        let mut dag = build_initial_sfx_dag_v3(merged_data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();

        // All tokens from both segments should be present
        assert!(!reader.resolve_suffix("mutex_lo").is_empty(), "mutex_lo from seg_a");
        assert!(!reader.resolve_suffix("mutex_co").is_empty(), "mutex_co from seg_b");
        assert!(!reader.resolve_suffix("hello_wo").is_empty(), "hello_wo from seg_a");
        assert!(!reader.resolve_suffix("foo_ba").is_empty(), "foo_ba from seg_b");
    }

    #[test]
    fn test_merge_shared_tokens() {
        // Both segments have "mutex_lock" → same extended tokens
        let seg_a = build_segment(&["mutex_lock"]);
        let seg_b = build_segment(&["mutex_lock"]);

        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0)].into();
        let remap_b: std::collections::HashMap<u32, u32> = [(0, 1)].into();

        let merged_data = merge_segments_v3(
            &[&seg_a.termtexts, &seg_b.termtexts],
            &[seg_a.sfxpost.as_deref(), seg_b.sfxpost.as_deref()],
            &[&remap_a, &remap_b],
        ).unwrap();

        // Shared tokens should have merged postings
        // "mutex_lo" should have postings from both doc 0 and doc 1
        let ord = merged_data.token_texts.iter()
            .position(|t| t == "mutex_lo")
            .expect("mutex_lo should exist");
        let postings = &merged_data.token_postings[ord];
        let doc_ids: std::collections::HashSet<u32> = postings.iter().map(|p| p.0).collect();
        assert!(doc_ids.contains(&0), "should have doc 0");
        assert!(doc_ids.contains(&1), "should have doc 1");
    }

    #[test]
    fn test_merge_with_deleted_docs() {
        let seg_a = build_segment(&["mutex_lock", "hello_world", "foo_bar"]);

        // Only remap docs 0 and 2, doc 1 is deleted
        let remap_a: std::collections::HashMap<u32, u32> = [(0, 0), (2, 1)].into();

        let merged_data = merge_segments_v3(
            &[&seg_a.termtexts],
            &[seg_a.sfxpost.as_deref()],
            &[&remap_a],
        ).unwrap();

        // "hello_wo" was in doc 1 which is deleted → its postings should only
        // contain docs that are in the remap
        let has_hello = merged_data.token_texts.iter().any(|t| t == "hello_wo");
        if has_hello {
            let ord = merged_data.token_texts.iter().position(|t| t == "hello_wo").unwrap();
            let postings = &merged_data.token_postings[ord];
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

        let merged_data = merge_segments_v3(
            &[&seg_a.termtexts],
            &[seg_a.sfxpost.as_deref()],
            &[&remap_a],
        ).unwrap();

        // Rebuild and check metadata survives the round-trip
        let mut dag = build_initial_sfx_dag_v3(merged_data);
        let mut result = luciole::execute_dag(&mut dag, None).unwrap();
        let output = result.take_output::<SfxBuildOutputV3>("assemble", "output").unwrap();

        let tt = TermTextsReaderV3::open(&output.termtexts).unwrap();
        let reader = SfxFileReaderV3::open(&output.sfx).unwrap();

        for ord in 0..tt.num_terms() {
            let (text, meta) = tt.entry(ord).unwrap();
            let parents = reader.resolve_suffix(text);
            let p = parents.iter().find(|p| p.sti == 0).unwrap();
            assert_eq!(p.own_len, meta.own_len, "own_len roundtrip for '{text}'");
            assert_eq!(p.sep_len, meta.sep_len, "sep_len roundtrip for '{text}'");
            assert_eq!(p.overlap_len, meta.overlap_len, "overlap roundtrip for '{text}'");
        }
    }
}
