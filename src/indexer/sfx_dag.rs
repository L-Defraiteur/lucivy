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

        // Write primary index files (from DAG nodes)
        if let Some(ref sfxpost) = sfxpost_data {
            write_file(segment, "sfxpost", sfxpost)?;
        }
        if !gapmap_data_clone.is_empty() {
            write_file(segment, "gapmap", &gapmap_data_clone)?;
        }
        if !sibling_data_clone.is_empty() {
            write_file(segment, "sibling", &sibling_data_clone)?;
        }

        // Build all derived indexes via registry single-pass
        // (posmap, bytemap, termtexts, sepmap — in one loop over tokens+sfxpost)
        let derived_files = crate::suffix_fst::index_registry::build_derived_indexes(
            tokens,
            sfxpost_data.as_deref(),
            &gapmap_data_clone,
            self.num_docs,
        );
        for (ext, data) in &derived_files {
            write_file(segment, ext, data)?;
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

// ===========================================================================
// Initial segment build DAG
// ===========================================================================
//
// ```text
// prepare_data ──┬── build_fst ──────────┐
//                ├── build_sfxpost ───────┼── write_sfx
//                └── build_sibling ───────┘
// ```

// ---------------------------------------------------------------------------
// PrepareDataNode — sort tokens, extract data from SfxCollector
// ---------------------------------------------------------------------------

struct PrepareDataNode {
    data: Option<crate::suffix_fst::SfxCollectorData>,
}

impl Node for PrepareDataNode {
    fn node_type(&self) -> &'static str { "sfx_prepare_data" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("tokens", PortType::of::<BTreeSet<String>>()),
            PortDef::required("collector_data", PortType::of::<crate::suffix_fst::SfxCollectorData>()),
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
// BuildSfxPostNode — build sfxpost from raw collector data
// ---------------------------------------------------------------------------

struct BuildSfxPostNode;

impl Node for BuildSfxPostNode {
    fn node_type(&self) -> &'static str { "sfx_build_sfxpost" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("collector_data", PortType::of::<crate::suffix_fst::SfxCollectorData>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = ctx.input("collector_data")
            .ok_or("missing collector_data")?
            .downcast::<crate::suffix_fst::SfxCollectorData>()
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
// BuildSiblingNode — build sibling table from collector sibling_pairs
// ---------------------------------------------------------------------------

struct BuildSiblingNode;

impl Node for BuildSiblingNode {
    fn node_type(&self) -> &'static str { "sfx_build_sibling" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("collector_data", PortType::of::<crate::suffix_fst::SfxCollectorData>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("siblings", PortType::of::<Vec<u8>>())]
    }
    fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
        let data = ctx.input("collector_data")
            .ok_or("missing collector_data")?
            .downcast::<crate::suffix_fst::SfxCollectorData>()
            .ok_or("wrong type")?;

        let num_terms = data.tokens.len() as u32;
        let mut writer = crate::suffix_fst::sibling_table::SiblingTableWriter::new(num_terms);
        for ((intern_a, intern_b), gap_lens) in &data.sibling_pairs {
            let final_a = data.intern_to_final[*intern_a as usize];
            let final_b = data.intern_to_final[*intern_b as usize];
            for &gap_len in gap_lens {
                writer.add(final_a, final_b, gap_len);
            }
        }
        let sibling_data = writer.serialize();
        ctx.metric("sibling_bytes", sibling_data.len() as f64);
        ctx.set_output("siblings", PortValue::new(sibling_data));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_initial_sfx_dag — factory for initial segment build
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// AssembleSfxNode — collect all outputs into SfxBuildOutput (no I/O)
// ---------------------------------------------------------------------------

struct AssembleSfxNode {
    num_docs: u32,
}

impl Node for AssembleSfxNode {
    fn node_type(&self) -> &'static str { "sfx_assemble" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>()),
            PortDef::required("gapmap", PortType::of::<Vec<u8>>()),
            PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>()),
            PortDef::required("siblings", PortType::of::<Vec<u8>>()),
            PortDef::required("tokens", PortType::of::<BTreeSet<String>>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("output", PortType::of::<crate::suffix_fst::SfxBuildOutput>())]
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

        // Build .sfx file bytes
        let sfx_file = crate::suffix_fst::file::SfxFileWriter::new(
            fst_data, parent_data, gapmap_data.clone(),
            self.num_docs, num_tokens,
        ).with_sibling_data(sibling_data.clone());
        let sfx_bytes = sfx_file.to_bytes();

        // Primary registry files
        let mut registry_files = Vec::new();
        if let Some(ref data) = sfxpost_data {
            registry_files.push(("sfxpost".to_string(), data.clone()));
        }
        if !gapmap_data.is_empty() {
            registry_files.push(("gapmap".to_string(), gapmap_data.clone()));
        }
        if !sibling_data.is_empty() {
            registry_files.push(("sibling".to_string(), sibling_data));
        }

        // Derived indexes via single-pass registry
        let derived = crate::suffix_fst::index_registry::build_derived_indexes(
            tokens,
            sfxpost_data.as_deref(),
            &gapmap_data,
            self.num_docs,
        );
        registry_files.extend(derived);

        ctx.set_output("output", PortValue::new(crate::suffix_fst::SfxBuildOutput {
            sfx: sfx_bytes,
            registry_files,
        }));
        Ok(())
    }
}

/// Build a DAG for initial SFX index creation from SfxCollector data.
///
/// ```text
/// prepare_data ──┬── build_fst ──────────┐
///                ├── build_sfxpost ───────┼── assemble → SfxBuildOutput
///                └── build_sibling ───────┘
/// ```
///
/// build_fst, build_sfxpost, and build_sibling run in PARALLEL.
/// Returns a DAG whose "assemble" node outputs `SfxBuildOutput`.
pub(crate) fn build_initial_sfx_dag(
    data: crate::suffix_fst::SfxCollectorData,
) -> Dag {
    let num_docs = data.num_docs;
    let gapmap_data = data.gapmap_data.clone();

    let mut dag = Dag::new();

    // prepare_data (source node)
    dag.add_node("prepare", PrepareDataNode { data: Some(data) });

    // build_fst (parallel — reuses the merge DAG node)
    dag.add_node("build_fst", BuildFstNode);
    dag.connect("prepare", "tokens", "build_fst", "tokens").unwrap();

    // build_sfxpost (parallel)
    dag.add_node("build_sfxpost", BuildSfxPostNode);
    dag.connect("prepare", "collector_data", "build_sfxpost", "collector_data").unwrap();

    // build_sibling (parallel)
    dag.add_node("build_sibling", BuildSiblingNode);
    dag.connect("prepare", "collector_data", "build_sibling", "collector_data").unwrap();

    // gapmap as constant source
    struct GapmapSourceNode(Option<Vec<u8>>);
    impl Node for GapmapSourceNode {
        fn node_type(&self) -> &'static str { "gapmap_source" }
        fn outputs(&self) -> Vec<PortDef> {
            vec![PortDef::required("gapmap", PortType::of::<Vec<u8>>())]
        }
        fn execute(&mut self, ctx: &mut NodeContext) -> Result<(), String> {
            let data = self.0.take().unwrap_or_default();
            ctx.set_output("gapmap", PortValue::new(data));
            Ok(())
        }
    }
    dag.add_node("gapmap_source", GapmapSourceNode(Some(gapmap_data)));

    // assemble (collect all → SfxBuildOutput)
    dag.add_node("assemble", AssembleSfxNode { num_docs });
    dag.connect("build_fst", "fst", "assemble", "fst").unwrap();
    dag.connect("gapmap_source", "gapmap", "assemble", "gapmap").unwrap();
    dag.connect("build_sfxpost", "sfxpost", "assemble", "sfxpost").unwrap();
    dag.connect("build_sibling", "siblings", "assemble", "siblings").unwrap();
    dag.connect("prepare", "tokens", "assemble", "tokens").unwrap();

    dag
}
