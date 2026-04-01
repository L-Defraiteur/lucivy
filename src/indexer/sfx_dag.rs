//! DAG for suffix FST merge — each step is a node with observability.
//!
//! ```text
//! collect_tokens ──┬── build_fst ──────────────┐
//!                  ├── copy_gapmap ── validate ─┼── write_sfx
//!                  └── merge_sfxpost ───────────┘
//! ```
//!
//! build_fst, copy_gapmap, and merge_sfxpost are INDEPENDENT and run
//! in parallel on the scheduler pool.

use std::collections::BTreeSet;
use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::index::SegmentReader;
use crate::indexer::sfx_merge;
use crate::schema::Field;
use crate::DocAddress;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Shared context (read-only, Arc'd for DAG nodes)
// ---------------------------------------------------------------------------

struct SfxContext {
    readers: Arc<Vec<SegmentReader>>,
    field: Field,
    doc_mapping: Vec<DocAddress>,
    reverse_doc_map: Vec<HashMap<u32, u32>>,
    sfx_data: Vec<Option<Vec<u8>>>,
}

// ---------------------------------------------------------------------------
// CollectTokensNode
// ---------------------------------------------------------------------------

struct CollectTokensNode {
    ctx: Arc<SfxContext>,
}

impl Node for CollectTokensNode {
    fn node_type(&self) -> &'static str { "sfx_collect_tokens" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("tokens", PortType::of::<BTreeSet<String>>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let tokens = sfx_merge::collect_tokens(
            &self.ctx.readers, self.ctx.field, &self.ctx.reverse_doc_map,
        ).map_err(|e| format!("collect_tokens: {e}"))?;
        nctx.metric("unique_tokens", tokens.len() as f64);
        nctx.set_output("tokens", PortValue::new(tokens));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BuildFstNode
// ---------------------------------------------------------------------------

struct BuildFstNode;

impl Node for BuildFstNode {
    fn node_type(&self) -> &'static str { "sfx_build_fst" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("tokens", PortType::of::<BTreeSet<String>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let tokens = ctx.input("tokens")
            .ok_or("missing tokens")?
            .downcast::<BTreeSet<String>>()
            .ok_or("wrong type")?;
        let (fst_data, parent_data) = sfx_merge::build_fst(tokens)
            .map_err(|e| format!("build_fst: {e}"))?;
        ctx.metric("fst_bytes", fst_data.len() as f64);
        ctx.set_output("fst", PortValue::new((fst_data, parent_data)));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CopyGapmapNode
// ---------------------------------------------------------------------------

struct CopyGapmapNode {
    ctx: Arc<SfxContext>,
}

impl Node for CopyGapmapNode {
    fn node_type(&self) -> &'static str { "sfx_copy_gapmap" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("gapmap", PortType::of::<Vec<u8>>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let gapmap_data = sfx_merge::copy_gapmap(
            &self.ctx.sfx_data, &self.ctx.doc_mapping,
        );
        nctx.metric("gapmap_bytes", gapmap_data.len() as f64);
        nctx.set_output("gapmap", PortValue::new(gapmap_data));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ValidateGapmapNode
// ---------------------------------------------------------------------------

struct ValidateGapmapNode;

impl Node for ValidateGapmapNode {
    fn node_type(&self) -> &'static str { "sfx_validate_gapmap" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("gapmap", PortType::of::<Vec<u8>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("gapmap", PortType::of::<Vec<u8>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let gapmap_data = ctx.input("gapmap")
            .ok_or("missing gapmap")?
            .downcast::<Vec<u8>>()
            .ok_or("wrong type")?;

        let errors = sfx_merge::validate_gapmap(gapmap_data);
        ctx.metric("errors", errors.len() as f64);
        if !errors.is_empty() {
            for (i, err) in errors.iter().enumerate().take(10) {
                ctx.warn(&format!("gapmap error {}: {}", i, err));
            }
        }

        // Passthrough
        let gapmap_clone = ctx.take_input("gapmap").unwrap();
        ctx.set_output("gapmap", gapmap_clone);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergeSfxpostNode
// ---------------------------------------------------------------------------

struct MergeSfxpostNode {
    ctx: Arc<SfxContext>,
}

impl Node for MergeSfxpostNode {
    fn node_type(&self) -> &'static str { "sfx_merge_sfxpost" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("tokens", PortType::of::<BTreeSet<String>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let tokens = nctx.input("tokens")
            .ok_or("missing tokens")?
            .downcast::<BTreeSet<String>>()
            .ok_or("wrong type")?;

        let sfxpost = sfx_merge::merge_sfxpost(
            &self.ctx.readers, self.ctx.field, tokens, &self.ctx.reverse_doc_map,
        ).map_err(|e| format!("merge_sfxpost: {e}"))?;

        nctx.metric("sfxpost_bytes", sfxpost.as_ref().map(|d| d.len()).unwrap_or(0) as f64);
        nctx.set_output("sfxpost", PortValue::new(sfxpost));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MergeSiblingLinksNode
// ---------------------------------------------------------------------------

struct MergeSiblingLinksNode {
    ctx: Arc<SfxContext>,
}

impl Node for MergeSiblingLinksNode {
    fn node_type(&self) -> &'static str { "sfx_merge_siblings" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("tokens", PortType::of::<BTreeSet<String>>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("siblings", PortType::of::<Vec<u8>>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let tokens = nctx.input("tokens")
            .ok_or("missing tokens")?
            .downcast::<BTreeSet<String>>()
            .ok_or("wrong type")?;

        let sfx_data: Vec<Option<Vec<u8>>> = self.ctx.readers.iter().map(|r| {
            r.sfx_file(self.ctx.field)
                .and_then(|f| f.read_bytes().ok())
                .map(|b| b.to_vec())
        }).collect();

        let sibling_data = sfx_merge::merge_sibling_links(
            &sfx_data, &self.ctx.readers, self.ctx.field, tokens,
        ).map_err(|e| format!("merge_sibling_links: {e}"))?;

        nctx.metric("sibling_bytes", sibling_data.len() as f64);
        nctx.set_output("siblings", PortValue::new(sibling_data));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ValidateSfxpostNode
// ---------------------------------------------------------------------------

struct ValidateSfxpostNode {
    num_docs: u32,
}

impl Node for ValidateSfxpostNode {
    fn node_type(&self) -> &'static str { "sfx_validate_sfxpost" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>()),
            PortDef::required("tokens", PortType::of::<BTreeSet<String>>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let sfxpost = ctx.input("sfxpost")
            .ok_or("missing sfxpost")?
            .downcast::<Option<Vec<u8>>>()
            .ok_or("wrong type")?;
        let tokens = ctx.input("tokens")
            .ok_or("missing tokens")?
            .downcast::<BTreeSet<String>>()
            .ok_or("wrong type")?;

        if let Some(data) = &sfxpost {
            if let Some(err) = sfx_merge::validate_sfxpost(
                data, self.num_docs, tokens.len() as u32,
            ) {
                return Err(format!("sfxpost validation: {err}"));
            }
        }

        let sfxpost_pass = ctx.take_input("sfxpost").unwrap();
        ctx.set_output("sfxpost", sfxpost_pass);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WriteSfxNode — writes .sfx/.sfxpost directly via Segment
// ---------------------------------------------------------------------------

struct WriteSfxNode {
    segment: Option<crate::index::Segment>,
    field: Field,
    num_docs: u32,
    ctx: Arc<SfxContext>,
}

impl Node for WriteSfxNode {
    fn node_type(&self) -> &'static str { "sfx_write" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>()),
            PortDef::required("gapmap", PortType::of::<Vec<u8>>()),
            PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>()),
            PortDef::required("siblings", PortType::of::<Vec<u8>>()),
            PortDef::required("tokens", PortType::of::<BTreeSet<String>>()),
        ]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let (fst_data, parent_data) = ctx.take_input("fst")
            .ok_or("missing fst")?.take::<(Vec<u8>, Vec<u8>)>().ok_or("fst type")?;
        let gapmap_data = ctx.take_input("gapmap")
            .ok_or("missing gapmap")?.take::<Vec<u8>>().ok_or("gapmap type")?;
        let sfxpost_data = ctx.take_input("sfxpost")
            .ok_or("missing sfxpost")?.take::<Option<Vec<u8>>>().ok_or("sfxpost type")?;
        let sibling_data = ctx.take_input("siblings")
            .and_then(|v| v.take::<Vec<u8>>())
            .unwrap_or_default();
        let tokens = ctx.input("tokens")
            .ok_or("missing tokens")?.downcast::<BTreeSet<String>>().ok_or("tokens type")?;

        let num_tokens = tokens.len() as u32;
        let segment = self.segment.as_mut().ok_or("segment missing")?;
        let field_id = self.field.field_id();

        // Clone gapmap/sibling before passing to SfxFileWriter (which takes ownership)
        let gapmap_data_clone = gapmap_data.clone();
        let sibling_data_clone = sibling_data.clone();

        // Build .sfx file
        let sfx_file = crate::suffix_fst::file::SfxFileWriter::new(
            fst_data, parent_data, gapmap_data,
            self.num_docs, num_tokens,
        ).with_sibling_data(sibling_data);
        let sfx_bytes = sfx_file.to_bytes();

        // Write .sfx
        use common::TerminatingWrite;
        let mut writer = segment.open_write_custom(&format!("{field_id}.sfx"))
            .map_err(|e| format!("open sfx: {e}"))?;
        std::io::Write::write_all(&mut writer, &sfx_bytes)
            .map_err(|e| format!("write sfx: {e}"))?;
        writer.terminate().map_err(|e| format!("close sfx: {e}"))?;

        // Write all registry files via write_custom_index pattern
        let write_file = |seg: &mut crate::index::Segment, ext: &str, data: &[u8]| -> Result<(), String> {
            let mut w = seg.open_write_custom(&format!("{field_id}.{ext}"))
                .map_err(|e| format!("open {ext}: {e}"))?;
            std::io::Write::write_all(&mut w, data)
                .map_err(|e| format!("write {ext}: {e}"))?;
            use common::TerminatingWrite;
            w.terminate().map_err(|e| format!("close {ext}: {e}"))?;
            Ok(())
        };

        if let Some(ref sfxpost) = sfxpost_data {
            write_file(segment, "sfxpost", sfxpost)?;

            // Build posmap + bytemap + termtexts from sfxpost data + tokens
            if let Some(reader) = crate::suffix_fst::sfxpost_v2::SfxPostReaderV2::open_slice(sfxpost) {
                let mut posmap_writer = crate::suffix_fst::PosMapWriter::new();
                let mut bytemap_writer = crate::suffix_fst::ByteBitmapWriter::new();
                let mut termtexts_writer = crate::suffix_fst::TermTextsWriter::new();
                bytemap_writer.ensure_capacity(num_tokens);

                for (ord, token) in tokens.iter().enumerate() {
                    let ord = ord as u32;
                    bytemap_writer.record_token(ord, token.as_bytes());
                    termtexts_writer.add(ord, token);
                    for e in reader.entries(ord) {
                        posmap_writer.add(e.doc_id, e.token_index, ord);
                    }
                }

                write_file(segment, "posmap", &posmap_writer.serialize())?;
                write_file(segment, "bytemap", &bytemap_writer.serialize())?;
                write_file(segment, "termtexts", &termtexts_writer.serialize())?;
            }
        }

        // Write gapmap + sibling as separate registry files
        if !gapmap_data_clone.is_empty() {
            write_file(segment, "gapmap", &gapmap_data_clone)?;
        }
        if !sibling_data_clone.is_empty() {
            write_file(segment, "sibling", &sibling_data_clone)?;
        }

        // Merge sepmap from source segments (OR-merge bitmaps per ordinal).
        // Uses TermTextsReader (SFX ordinals) to map token text → old SFX ordinal,
        // NOT the term dict (which has different ordinals).
        {
            use crate::suffix_fst::sepmap::{SepMapReader, SepMapWriter};
            use crate::suffix_fst::TermTextsReader;

            let source_sepmaps: Vec<Option<Vec<u8>>> = self.ctx.readers.iter().map(|r| {
                r.sfx_index_file("sepmap", self.field)
                    .and_then(|f| f.read_bytes().ok())
                    .map(|b| b.to_vec())
            }).collect();
            let source_termtexts: Vec<Option<Vec<u8>>> = self.ctx.readers.iter().map(|r| {
                r.sfx_index_file("termtexts", self.field)
                    .and_then(|f| f.read_bytes().ok())
                    .map(|b| b.to_vec())
            }).collect();

            let sepmap_readers: Vec<Option<SepMapReader>> = source_sepmaps.iter()
                .map(|opt| opt.as_deref().and_then(SepMapReader::open))
                .collect();

            if sepmap_readers.iter().any(|r| r.is_some()) {
                let num_terms = tokens.len() as u32;
                let mut sepmap_writer = SepMapWriter::new();
                sepmap_writer.ensure_capacity(num_terms);

                for (seg_idx, sepmap_opt) in sepmap_readers.iter().enumerate() {
                    let sepmap = match sepmap_opt {
                        Some(r) => r,
                        None => continue,
                    };

                    // Build reverse map: token text → old SFX ordinal via TermTextsReader
                    let reverse_map: std::collections::HashMap<&str, u32> =
                        if let Some(tt_bytes) = &source_termtexts[seg_idx] {
                            if let Some(tt_reader) = TermTextsReader::open(tt_bytes) {
                                (0..tt_reader.num_terms())
                                    .filter_map(|ord| tt_reader.text(ord).map(|t| (t, ord)))
                                    .collect()
                            } else {
                                std::collections::HashMap::new()
                            }
                        } else {
                            std::collections::HashMap::new()
                        };

                    // For each merged token, find its old SFX ordinal and OR-merge bitmap
                    for (new_ord, token) in tokens.iter().enumerate() {
                        if let Some(&old_ord) = reverse_map.get(token.as_str()) {
                            if let Some(bitmap) = sepmap.bitmap(old_ord) {
                                sepmap_writer.or_bitmap(new_ord as u32, bitmap);
                            }
                        }
                    }
                }

                let sepmap_data = sepmap_writer.serialize();
                if !sepmap_data.is_empty() {
                    write_file(segment, "sepmap", &sepmap_data)?;
                }
            }
        }

        ctx.metric("written", 1.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_sfx_dag — factory
// ---------------------------------------------------------------------------

/// Build a DAG for merging suffix FST data of a single field.
///
/// ```text
/// collect_tokens ──┬── build_fst ─────────────────────────┐
///                  ├── copy_gapmap ── validate_gapmap ─────┼── write_sfx
///                  └── merge_sfxpost ── validate_sfxpost ──┘
/// ```
///
/// build_fst, copy_gapmap, and merge_sfxpost run in PARALLEL.
pub(crate) fn build_sfx_dag(
    readers: Arc<Vec<SegmentReader>>,
    field: Field,
    doc_mapping: Vec<DocAddress>,
    reverse_doc_map: Vec<HashMap<u32, u32>>,
    sfx_data: Vec<Option<Vec<u8>>>,
    segment: crate::index::Segment,
) -> Dag {
    let num_docs = doc_mapping.len() as u32;

    let ctx = Arc::new(SfxContext {
        readers,
        field,
        doc_mapping,
        reverse_doc_map,
        sfx_data,
    });

    let mut dag = Dag::new();

    // collect_tokens (source node — sequential, needs readers)
    dag.add_node("collect", CollectTokensNode { ctx: ctx.clone() });

    // copy_gapmap (independent — parallel with build_fst and merge_sfxpost)
    dag.add_node("copy_gapmap", CopyGapmapNode { ctx: ctx.clone() });

    // build_fst (depends on tokens)
    dag.add_node("build_fst", BuildFstNode);
    dag.connect("collect", "tokens", "build_fst", "tokens").unwrap();

    // validate_gapmap (depends on gapmap)
    dag.add_node("validate_gapmap", ValidateGapmapNode);
    dag.connect("copy_gapmap", "gapmap", "validate_gapmap", "gapmap").unwrap();

    // merge_sfxpost (depends on tokens, parallel with fst+gapmap)
    dag.add_node("merge_sfxpost", MergeSfxpostNode { ctx: ctx.clone() });
    dag.connect("collect", "tokens", "merge_sfxpost", "tokens").unwrap();

    // validate_sfxpost (depends on sfxpost + tokens)
    dag.add_node("validate_sfxpost", ValidateSfxpostNode { num_docs });
    dag.connect("merge_sfxpost", "sfxpost", "validate_sfxpost", "sfxpost").unwrap();
    dag.connect("collect", "tokens", "validate_sfxpost", "tokens").unwrap();

    // merge_sibling_links (depends on tokens, parallel with fst+gapmap+sfxpost)
    dag.add_node("merge_siblings", MergeSiblingLinksNode { ctx: ctx.clone() });
    dag.connect("collect", "tokens", "merge_siblings", "tokens").unwrap();

    // write (depends on all: fst, validated gapmap, validated sfxpost, siblings, tokens)
    dag.add_node("write", WriteSfxNode {
        segment: Some(segment),
        field,
        num_docs,
        ctx: ctx.clone(),
    });
    dag.connect("build_fst", "fst", "write", "fst").unwrap();
    dag.connect("validate_gapmap", "gapmap", "write", "gapmap").unwrap();
    dag.connect("validate_sfxpost", "sfxpost", "write", "sfxpost").unwrap();
    dag.connect("merge_siblings", "siblings", "write", "siblings").unwrap();
    dag.connect("collect", "tokens", "write", "tokens").unwrap();

    dag
}
