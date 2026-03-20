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
// WriteSfxNode
// ---------------------------------------------------------------------------

struct WriteSfxNode {
    field: Field,
    num_docs: u32,
    serializer: Arc<std::sync::Mutex<Option<crate::indexer::SegmentSerializer>>>,
}

impl Node for WriteSfxNode {
    fn node_type(&self) -> &'static str { "sfx_write" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("fst", PortType::of::<(Vec<u8>, Vec<u8>)>()),
            PortDef::required("gapmap", PortType::of::<Vec<u8>>()),
            PortDef::required("sfxpost", PortType::of::<Option<Vec<u8>>>()),
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
        let tokens = ctx.input("tokens")
            .ok_or("missing tokens")?.downcast::<BTreeSet<String>>().ok_or("tokens type")?;

        let num_tokens = tokens.len() as u32;
        let mut serializer = self.serializer.lock().unwrap();
        let ser = serializer.as_mut().ok_or("serializer already taken")?;

        sfx_merge::write_sfx(
            ser, self.field,
            fst_data, parent_data, gapmap_data,
            self.num_docs, num_tokens, sfxpost_data,
        ).map_err(|e| format!("write_sfx: {e}"))?;

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
    serializer: crate::indexer::SegmentSerializer,
) -> (Dag, Arc<std::sync::Mutex<Option<crate::indexer::SegmentSerializer>>>) {
    let num_docs = doc_mapping.len() as u32;

    let ctx = Arc::new(SfxContext {
        readers,
        field,
        doc_mapping,
        reverse_doc_map,
        sfx_data,
    });

    let serializer = Arc::new(std::sync::Mutex::new(Some(serializer)));

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

    // write (depends on all: fst, validated gapmap, validated sfxpost, tokens)
    dag.add_node("write", WriteSfxNode {
        field,
        num_docs,
        serializer: serializer.clone(),
    });
    dag.connect("build_fst", "fst", "write", "fst").unwrap();
    dag.connect("validate_gapmap", "gapmap", "write", "gapmap").unwrap();
    dag.connect("validate_sfxpost", "sfxpost", "write", "sfxpost").unwrap();
    dag.connect("collect", "tokens", "write", "tokens").unwrap();

    (dag, serializer)
}
