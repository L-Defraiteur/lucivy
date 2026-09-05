//! DAG-based merge: each phase is a node, parallel where possible.
//!
//! ```text
//! init ──┬── postings ──────────┐
//!        ├── store ─────────────┼── sfx ── close
//!        └── fast_fields ───────┘
//! ```
//!
//! Postings, Store, and FastFields run in PARALLEL after Init.
//! Sfx runs after all three (needs doc_mapping from FastFields).
//! Close reassembles and finalizes.

use std::sync::Arc;

use luciole::node::{Node, NodeContext, PortDef};
use luciole::port::{PortType, PortValue};
use luciole::Dag;

use crate::directory::WritePtr;
use crate::fieldnorm::FieldNormReaders;
use crate::index::{Index, Segment, SegmentComponent};
use crate::indexer::doc_id_mapping::SegmentDocIdMapping;
use crate::indexer::merger::IndexMerger;
use crate::indexer::segment_entry::SegmentEntry;
use crate::indexer::segment_serializer::SegmentSerializer;
use crate::indexer::delete_queue::DeleteCursor;
use crate::postings::InvertedIndexSerializer;
use crate::schema::Field;
use crate::store::StoreWriter;
use crate::{DocAddress, Opstamp, SegmentReader};

// ---------------------------------------------------------------------------
// Shared merge context (read-only, Arc'd)
// ---------------------------------------------------------------------------

pub(crate) struct MergeContext {
    pub merger: IndexMerger,
    pub index: Index,
    pub merged_segment: Segment,
    pub delete_cursor: DeleteCursor,
    pub indexed_fields: Vec<Field>,
}

// ---------------------------------------------------------------------------
// InitNode
// ---------------------------------------------------------------------------

struct InitNode {
    ctx: Arc<MergeContext>,
    serializer: Option<SegmentSerializer>,
}

impl Node for InitNode {
    fn node_type(&self) -> &'static str { "init" }
    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>()),
            PortDef::required("store_writer", PortType::of::<StoreWriter>()),
            PortDef::required("ff_write", PortType::of::<WritePtr>()),
            PortDef::required("segment", PortType::of::<Segment>()),
            PortDef::required("doc_id_mapping_postings", PortType::of::<SegmentDocIdMapping>()),
            PortDef::required("doc_id_mapping_ff", PortType::of::<SegmentDocIdMapping>()),
            PortDef::required("fieldnorm_readers", PortType::of::<FieldNormReaders>()),
            PortDef::required("sfx_doc_mapping", PortType::of::<Vec<DocAddress>>()),
        ]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let doc_id_mapping = self.ctx.merger.get_doc_id_from_concatenated_data()
            .map_err(|e| format!("doc_id_mapping: {e}"))?;

        let mut serializer = self.serializer.take().unwrap();

        // Write fieldnorms (must happen before decompose — needs &mut serializer)
        if let Some(fieldnorms_serializer) = serializer.extract_fieldnorms_serializer() {
            self.ctx.merger.write_fieldnorms(fieldnorms_serializer, &doc_id_mapping)
                .map_err(|e| format!("write_fieldnorms: {e}"))?;
        }

        // Read fieldnorm data back for postings scoring
        let fieldnorm_data = serializer.segment()
            .open_read(SegmentComponent::FieldNorms)
            .map_err(|e| format!("open fieldnorms: {e}"))?;
        let fieldnorm_readers = FieldNormReaders::open(fieldnorm_data)
            .map_err(|e| format!("fieldnorm readers: {e}"))?;

        // Extract sfx_doc_mapping before decompose
        let sfx_doc_mapping: Vec<DocAddress> = doc_id_mapping.iter_old_doc_addrs().collect();

        // Decompose serializer into independent writers
        let (postings_ser, store_writer, ff_write, segment, _fieldnorms) = serializer.decompose();

        nctx.set_output("postings_ser", PortValue::new(postings_ser));
        nctx.set_output("store_writer", PortValue::new(store_writer));
        nctx.set_output("ff_write", PortValue::new(ff_write));
        nctx.set_output("segment", PortValue::new(segment));
        let doc_id_mapping_ff = doc_id_mapping.clone();
        nctx.set_output("doc_id_mapping_postings", PortValue::new(doc_id_mapping));
        nctx.set_output("doc_id_mapping_ff", PortValue::new(doc_id_mapping_ff));
        nctx.set_output("fieldnorm_readers", PortValue::new(fieldnorm_readers));
        nctx.set_output("sfx_doc_mapping", PortValue::new(sfx_doc_mapping));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PostingsNode — writes all postings (per-field, sequential within)
// ---------------------------------------------------------------------------

struct PostingsNode {
    ctx: Arc<MergeContext>,
}

impl Node for PostingsNode {
    fn node_type(&self) -> &'static str { "postings" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>()),
            PortDef::required("doc_id_mapping_postings", PortType::of::<SegmentDocIdMapping>()),
            PortDef::required("fieldnorm_readers", PortType::of::<FieldNormReaders>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let mut postings_ser = nctx.take_input("postings_ser")
            .ok_or("missing")?.take::<InvertedIndexSerializer>().ok_or("type")?;
        let doc_id_mapping = nctx.take_input("doc_id_mapping_postings")
            .ok_or("missing")?.take::<SegmentDocIdMapping>().ok_or("type")?;
        let fieldnorm_readers = nctx.take_input("fieldnorm_readers")
            .ok_or("missing")?.take::<FieldNormReaders>().ok_or("type")?;

        for (i, &field) in self.ctx.indexed_fields.iter().enumerate() {
            let field_entry = self.ctx.merger.schema.get_field_entry(field);
            let fieldnorm_reader = fieldnorm_readers.get_field(field)
                .map_err(|e| format!("fieldnorm field {i}: {e}"))?;
            self.ctx.merger.write_postings_for_field(
                field,
                field_entry.field_type(),
                &mut postings_ser,
                fieldnorm_reader,
                &doc_id_mapping,
            ).map_err(|e| format!("postings field {i}: {e}"))?;
        }

        nctx.metric("fields", self.ctx.indexed_fields.len() as f64);
        nctx.set_output("postings_ser", PortValue::new(postings_ser));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StoreNode — writes the document store
// ---------------------------------------------------------------------------

struct StoreNode {
    ctx: Arc<MergeContext>,
}

impl Node for StoreNode {
    fn node_type(&self) -> &'static str { "store" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("store_writer", PortType::of::<StoreWriter>())]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("store_writer", PortType::of::<StoreWriter>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let mut store_writer = nctx.take_input("store_writer")
            .ok_or("missing")?.take::<StoreWriter>().ok_or("type")?;
        self.ctx.merger.write_storable_fields(&mut store_writer)
            .map_err(|e| format!("store: {e}"))?;
        nctx.set_output("store_writer", PortValue::new(store_writer));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FastFieldsNode — writes columnar data
// ---------------------------------------------------------------------------

struct FastFieldsNode {
    ctx: Arc<MergeContext>,
}

impl Node for FastFieldsNode {
    fn node_type(&self) -> &'static str { "fast_fields" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("ff_write", PortType::of::<WritePtr>()),
            PortDef::required("doc_id_mapping_ff", PortType::of::<SegmentDocIdMapping>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("ff_write", PortType::of::<WritePtr>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let mut ff_write = nctx.take_input("ff_write")
            .ok_or("missing")?.take::<WritePtr>().ok_or("type")?;
        let doc_id_mapping = nctx.take_input("doc_id_mapping_ff")
            .ok_or("missing")?.take::<SegmentDocIdMapping>().ok_or("type")?;
        self.ctx.merger.write_fast_fields(&mut ff_write, doc_id_mapping)
            .map_err(|e| format!("fast_fields: {e}"))?;
        nctx.set_output("ff_write", PortValue::new(ff_write));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SfxNode — runs sfx sub-DAG per field
// ---------------------------------------------------------------------------

struct SfxNode {
    ctx: Arc<MergeContext>,
}


impl SfxNode {
    /// Merge the v3 SFX files of the source segments by remapping.
    ///
    /// The first implementation re-indexed from the docstore: one code path, but it
    /// rebuilt every intern table and posting list in RAM, measured at ~18 GB to
    /// fuse 50k kernel documents into a single segment. Remapping walks the
    /// persisted files instead — the partition tag in TTX3 is what makes the
    /// word-stripped half recoverable without re-tokenising.
    fn rebuild_sfx_v3(
        readers: &[SegmentReader],
        reverse_doc_map: &[std::collections::HashMap<u32, u32>],
        field: Field,
    ) -> crate::Result<super::sfx_dag_v3::SfxBuildOutputV3> {
        use super::sfx_dag_v3::SegmentSfxV3;

        // No copy: the SegmentReader already holds these FileSlices, and
        // read_bytes on a RAM- or mmap-backed handle only slices an Arc. The
        // `.to_vec()` that used to close this materialised all seven sidecars of
        // every source segment — around 280 MB for a round merging eight
        // 500-document segments — to read them sequentially once.
        let read_ext = |r: &SegmentReader, ext: &str| -> Option<common::OwnedBytes> {
            r.sfx_index_file(ext, field)
                .and_then(|f| f.read_bytes().ok())
        };

        // Hold the bytes alive for the duration of the merge.
        let owned: Vec<_> = readers.iter().map(|r| (
            read_ext(r, "termtexts"),
            read_ext(r, "sfxpost"),
            read_ext(r, "word_sfxpost"),
            read_ext(r, "sibling_v3"),
            read_ext(r, "posmap"),
        )).collect();

        let mut segs: Vec<SegmentSfxV3<'_>> = Vec::new();
        for (i, o) in owned.iter().enumerate() {
            fn sl(b: &Option<common::OwnedBytes>) -> Option<&[u8]> {
                b.as_ref().map(|x| x.as_slice())
            }
            let Some(tt) = sl(&o.0) else { continue };
            segs.push(SegmentSfxV3 {
                termtexts: tt,
                sfxpost: sl(&o.1),
                word_sfxpost: sl(&o.2),
                sibling_v3: sl(&o.3),
                posmap: sl(&o.4),
                doc_remap: &reverse_doc_map[i],
            });
        }
        if segs.is_empty() {
            return Err(crate::LucivyError::SystemError(
                "sfx v3 merge: no segment carries termtexts".into()));
        }

        let data = super::sfx_dag_v3::merge_segments_v3(&segs)
            .map_err(crate::LucivyError::SystemError)?;

        let mut dag = super::sfx_dag_v3::build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("sfx v3 merge DAG: {e}")))?;
        result.take_output::<super::sfx_dag_v3::SfxBuildOutputV3>("assemble", "output")
            .ok_or_else(|| crate::LucivyError::SystemError(
                "sfx v3 merge DAG: missing output".into()))
    }

    /// Merge dictionary segments (`sfx_version` 4): a remap through the
    /// union of their `.gmap`s, no text and no FST (`merge_segments_dict`).
    fn rebuild_sfx_dict(
        readers: &[SegmentReader],
        reverse_doc_map: &[std::collections::HashMap<u32, u32>],
        field: Field,
    ) -> crate::Result<super::sfx_dag_v3::SfxBuildOutputV3> {
        use super::sfx_dag_v3::SegmentSfxDict;
        let read_ext = |r: &SegmentReader, ext: &str| -> Option<common::OwnedBytes> {
            r.sfx_index_file(ext, field).and_then(|f| f.read_bytes().ok())
        };
        let owned: Vec<_> = readers.iter().map(|r| (
            read_ext(r, "gmap"),
            read_ext(r, "sfxpost"),
            read_ext(r, "word_sfxpost"),
            read_ext(r, "sibling_v3"),
            read_ext(r, "posmap"),
        )).collect();
        let mut segs: Vec<SegmentSfxDict<'_>> = Vec::new();
        for (i, o) in owned.iter().enumerate() {
            fn sl(b: &Option<common::OwnedBytes>) -> Option<&[u8]> {
                b.as_ref().map(|x| x.as_slice())
            }
            let Some(gmap) = sl(&o.0) else { continue };
            segs.push(SegmentSfxDict {
                gmap,
                sfxpost: sl(&o.1),
                word_sfxpost: sl(&o.2),
                sibling_v3: sl(&o.3),
                posmap: sl(&o.4),
                doc_remap: &reverse_doc_map[i],
            });
        }
        if segs.is_empty() {
            return Err(crate::LucivyError::SystemError(
                "sfx dictionary merge: no segment carries a .gmap".into()));
        }
        // The shard dictionary's META gives every id its `own_len`, for the
        // merged `.posmap`'s byte checkpoints.
        let dict = readers.iter().find_map(|r| r.sfx_dictionary_field(field));
        let dict_texts = dict.as_ref().and_then(|d| d.termtexts_reader());
        let own_len_of = |g: u32| dict_texts.as_ref().and_then(|t| t.meta(g)).map(|m| m.own_len);
        let data = super::sfx_dag_v3::merge_segments_dict(&segs, &own_len_of)
            .map_err(crate::LucivyError::SystemError)?;
        let mut dag = super::sfx_dag_v3::build_initial_sfx_dag_v3(data);
        let mut result = luciole::execute_dag(&mut dag, None)
            .map_err(|e| crate::LucivyError::SystemError(format!("sfx dictionary merge DAG: {e}")))?;
        result.take_output::<super::sfx_dag_v3::SfxBuildOutputV3>("assemble", "output")
            .ok_or_else(|| crate::LucivyError::SystemError(
                "sfx dictionary merge DAG: missing output".into()))
    }

    /// Write a v3 build output into the merged segment.
    ///
    /// Same file set as the initial build path in segment_writer, so a merged
    /// segment is indistinguishable from a freshly indexed one.
    fn write_sfx_v3(
        segment: &mut crate::index::Segment,
        field: Field,
        output: &super::sfx_dag_v3::SfxBuildOutputV3,
    ) -> crate::Result<()> {
        use common::TerminatingWrite;
        let field_id = field.field_id();
        let derived_in_ram = segment.index().settings().derived_in_ram;

        let mut write_file = |ext: &str, data: &[u8]| -> crate::Result<()> {
            let mut w = segment.open_write_custom(&format!("{field_id}.{ext}"))?;
            std::io::Write::write_all(&mut w, data)?;
            w.terminate()?;
            Ok(())
        };

        if !output.dictionary_mode {
            write_file("sfx", &output.sfx)?;
            write_file("termtexts", &output.termtexts)?;
        }
        if let Some(ref sfxpost) = output.sfxpost {
            write_file("sfxpost", sfxpost)?;
        }
        for (ext, data) in &output.registry_files {
            if derived_in_ram && crate::suffix_fst::derived::DERIVED_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            write_file(ext, data)?;
        }
        Ok(())
    }
}

impl Node for SfxNode {
    fn node_type(&self) -> &'static str { "sfx" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("segment", PortType::of::<Segment>()),
            PortDef::required("sfx_doc_mapping", PortType::of::<Vec<DocAddress>>()),
            // Wait for parallel phases to complete
            PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>()),
            PortDef::required("store_writer", PortType::of::<StoreWriter>()),
            PortDef::required("ff_write", PortType::of::<WritePtr>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("segment", PortType::of::<Segment>()),
            PortDef::required("sfx_field_ids", PortType::of::<Vec<u32>>()),
            PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>()),
            PortDef::required("store_writer", PortType::of::<StoreWriter>()),
            PortDef::required("ff_write", PortType::of::<WritePtr>()),
        ]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let segment = nctx.take_input("segment")
            .ok_or("missing")?.take::<Segment>().ok_or("type")?;
        let doc_mapping = nctx.take_input("sfx_doc_mapping")
            .ok_or("missing")?.take::<Vec<DocAddress>>().ok_or("type")?;

        // Passthrough writers (just for ordering — they flow to CloseNode)
        let postings_ser = nctx.take_input("postings_ser").unwrap();
        let store_writer = nctx.take_input("store_writer").unwrap();
        let ff_write = nctx.take_input("ff_write").unwrap();

        let readers = Arc::clone(&self.ctx.merger.readers);
        let schema = self.ctx.merger.schema.clone();

        let sfx_fields: Vec<Field> = schema
            .fields()
            .filter(|(_, entry)| {
                matches!(entry.field_type(), crate::schema::FieldType::Str(opts)
                    if opts.get_indexing_options().is_some())
            })
            .map(|(field, _)| field)
            .collect();

        let sfx_version = segment.index().settings().sfx_version;

        let reverse_doc_map = super::sfx_merge::build_reverse_doc_map(
            &doc_mapping, readers.len(),
        );

        let mut sfx_field_ids = Vec::new();

        // v3 merges by REMAPPING the persisted files.
        //
        // The v2 sub-DAG below cannot be reused: its CollectTokensNode reads the
        // inverted index term dictionary, which in v3 holds a different alphabet
        // than the SFX ordinal space (analyzer tokens vs extended and word-stripped
        // texts). Running it on v3 segments yields a well-formed v2 segment whose
        // postings point at the wrong terms, silently, because every
        // self-consistency check still passes.
        //
        // Re-indexing from the docstore was tried first: a single code path, and the
        // invariants come for free from the collector. But it rebuilds every intern
        // table and posting list in RAM — ~18 GB to fuse 50k kernel documents into
        // one segment — so it does not survive contact with a real corpus.
        if sfx_version >= 3 {
            let mut segment = segment;
            for &field in &sfx_fields {
                // Existence check only. This used to call load_sfx_data, which
                // copies every source segment's whole .sfx — 43 MB each once the
                // sources are themselves merged segments — and then discarded the
                // data, keeping just the boolean.
                if !readers.iter().any(|r| r.sfx_file(field).is_some()) { continue; }

                let output = (if sfx_version == crate::suffix_fst::dictionary::DICTIONARY_SFX_VERSION {
                    Self::rebuild_sfx_dict(&readers, &reverse_doc_map, field)
                } else {
                    Self::rebuild_sfx_v3(&readers, &reverse_doc_map, field)
                })
                    .map_err(|e| format!("sfx v3 merge field {}: {e}", field.field_id()))?;
                Self::write_sfx_v3(&mut segment, field, &output)
                    .map_err(|e| format!("sfx v3 write field {}: {e}", field.field_id()))?;
                sfx_field_ids.push(field.field_id());
            }

            nctx.metric("sfx_fields", sfx_field_ids.len() as f64);
            nctx.metric("sfx_v3_merged_docs", doc_mapping.len() as f64);
            nctx.set_output("segment", PortValue::new(segment));
            nctx.set_output("sfx_field_ids", PortValue::new(sfx_field_ids));
            nctx.set_output("postings_ser", postings_ser);
            nctx.set_output("store_writer", store_writer);
            nctx.set_output("ff_write", ff_write);
            return Ok(());
        }

        for &field in &sfx_fields {
            let (sfx_data, any_has_sfx) = super::sfx_merge::load_sfx_data(&readers, field);
            if !any_has_sfx { continue; }

            // Build and execute sfx sub-DAG for this field
            let mut sfx_dag = super::sfx_dag::build_sfx_dag(
                Arc::clone(&readers),
                field,
                doc_mapping.clone(),
                reverse_doc_map.clone(),
                sfx_data,
                segment.clone(),
            );

            luciole::execute_dag(&mut sfx_dag, None)
                .map_err(|e| format!("sfx DAG field {}: {e}", field.field_id()))?;

            sfx_field_ids.push(field.field_id());
        }

        nctx.metric("sfx_fields", sfx_field_ids.len() as f64);
        nctx.set_output("segment", PortValue::new(segment));
        nctx.set_output("sfx_field_ids", PortValue::new(sfx_field_ids));
        nctx.set_output("postings_ser", postings_ser);
        nctx.set_output("store_writer", store_writer);
        nctx.set_output("ff_write", ff_write);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CloseNode — close all writers, build SegmentEntry
// ---------------------------------------------------------------------------

struct CloseNode {
    ctx: Arc<MergeContext>,
}

impl Node for CloseNode {
    fn node_type(&self) -> &'static str { "close" }
    fn inputs(&self) -> Vec<PortDef> {
        vec![
            PortDef::required("postings_ser", PortType::of::<InvertedIndexSerializer>()),
            PortDef::required("store_writer", PortType::of::<StoreWriter>()),
            PortDef::required("ff_write", PortType::of::<WritePtr>()),
            PortDef::required("sfx_field_ids", PortType::of::<Vec<u32>>()),
        ]
    }
    fn outputs(&self) -> Vec<PortDef> {
        vec![PortDef::required("entry", PortType::of::<SegmentEntry>())]
    }
    fn execute(&mut self, nctx: &mut NodeContext) -> Result<(), String> {
        let postings_ser = nctx.take_input("postings_ser")
            .ok_or("missing")?.take::<InvertedIndexSerializer>().ok_or("type")?;
        let store_writer = nctx.take_input("store_writer")
            .ok_or("missing")?.take::<StoreWriter>().ok_or("type")?;
        let ff_write = nctx.take_input("ff_write")
            .ok_or("missing")?.take::<WritePtr>().ok_or("type")?;
        let sfx_field_ids = nctx.take_input("sfx_field_ids")
            .ok_or("missing")?.take::<Vec<u32>>().ok_or("type")?;

        // Close each writer
        postings_ser.close().map_err(|e| format!("close postings: {e}"))?;
        store_writer.close().map_err(|e| format!("close store: {e}"))?;
        use common::TerminatingWrite;
        ff_write.terminate()
            .map_err(|e| format!("close fast_fields: {e}"))?;

        // Build segment meta + entry
        let merged_segment_id = self.ctx.merged_segment.id();
        let num_docs = self.ctx.merger.max_doc;
        let segment_meta = self.ctx.index
            .new_segment_meta(merged_segment_id, num_docs)
            .with_sfx_field_ids(sfx_field_ids);
        let entry = SegmentEntry::new(
            segment_meta,
            self.ctx.delete_cursor.clone(),
            None,
        );

        nctx.metric("num_docs", num_docs as f64);
        nctx.set_output("entry", PortValue::new(entry));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// build_merge_dag — factory
// ---------------------------------------------------------------------------

/// Build a merge DAG for a set of segments.
///
/// ```text
/// init ──┬── postings ──────────┐
///        ├── store ─────────────┼── sfx ── close
///        └── fast_fields ───────┘
/// ```
///
/// Returns None if no alive docs (nothing to merge).
pub(crate) fn build_merge_dag(
    index: &Index,
    segment_entries: Vec<SegmentEntry>,
    target_opstamp: Opstamp,
) -> crate::Result<Option<Dag>> {
    let mut entries = segment_entries;
    let num_docs: u64 = entries.iter().map(|s| s.meta().num_docs() as u64).sum();
    if num_docs == 0 {
        return Ok(None);
    }

    let merged_segment = index.new_segment();

    // Advance deletes
    for entry in &mut entries {
        let segment = index.segment(entry.meta().clone());
        crate::indexer::index_writer::advance_deletes(
            segment, entry, target_opstamp,
        )?;
    }

    let delete_cursor = entries[0].delete_cursor().clone();
    let segments: Vec<Segment> = entries
        .iter()
        .map(|se| index.segment(se.meta().clone()))
        .collect();
    let schema = index.schema();
    let merger = IndexMerger::open(schema.clone(), &segments[..])?;
    let serializer = SegmentSerializer::for_segment(merged_segment.clone())?;

    let indexed_fields: Vec<Field> = schema
        .fields()
        .filter_map(|(field, entry)| if entry.is_indexed() { Some(field) } else { None })
        .collect();

    let ctx = Arc::new(MergeContext {
        merger,
        index: index.clone(),
        merged_segment,
        delete_cursor,
        indexed_fields,
    });

    let mut dag = Dag::new();

    // Init: fieldnorms + decompose serializer
    dag.add_node("init", InitNode { ctx: ctx.clone(), serializer: Some(serializer) });

    // Postings (parallel with store + fast_fields)
    dag.add_node("postings", PostingsNode { ctx: ctx.clone() });
    dag.connect("init", "postings_ser", "postings", "postings_ser").map_err(crate::LucivyError::SystemError)?;
    dag.connect("init", "doc_id_mapping_postings", "postings", "doc_id_mapping_postings").map_err(crate::LucivyError::SystemError)?;
    dag.connect("init", "fieldnorm_readers", "postings", "fieldnorm_readers").map_err(crate::LucivyError::SystemError)?;

    // Store (parallel)
    dag.add_node("store", StoreNode { ctx: ctx.clone() });
    dag.connect("init", "store_writer", "store", "store_writer").map_err(crate::LucivyError::SystemError)?;

    // FastFields (parallel)
    dag.add_node("fast_fields", FastFieldsNode { ctx: ctx.clone() });
    dag.connect("init", "ff_write", "fast_fields", "ff_write").map_err(crate::LucivyError::SystemError)?;
    dag.connect("init", "doc_id_mapping_ff", "fast_fields", "doc_id_mapping_ff").map_err(crate::LucivyError::SystemError)?;

    // Sfx (after all three parallel phases)
    dag.add_node("sfx", SfxNode { ctx: ctx.clone() });
    dag.connect("init", "segment", "sfx", "segment").map_err(crate::LucivyError::SystemError)?;
    dag.connect("init", "sfx_doc_mapping", "sfx", "sfx_doc_mapping").map_err(crate::LucivyError::SystemError)?;
    dag.connect("postings", "postings_ser", "sfx", "postings_ser").map_err(crate::LucivyError::SystemError)?;
    dag.connect("store", "store_writer", "sfx", "store_writer").map_err(crate::LucivyError::SystemError)?;
    dag.connect("fast_fields", "ff_write", "sfx", "ff_write").map_err(crate::LucivyError::SystemError)?;

    // Close (after sfx)
    dag.add_node("close", CloseNode { ctx: ctx.clone() });
    dag.connect("sfx", "postings_ser", "close", "postings_ser").map_err(crate::LucivyError::SystemError)?;
    dag.connect("sfx", "store_writer", "close", "store_writer").map_err(crate::LucivyError::SystemError)?;
    dag.connect("sfx", "ff_write", "close", "ff_write").map_err(crate::LucivyError::SystemError)?;
    dag.connect("sfx", "sfx_field_ids", "close", "sfx_field_ids").map_err(crate::LucivyError::SystemError)?;

    Ok(Some(dag))
}
