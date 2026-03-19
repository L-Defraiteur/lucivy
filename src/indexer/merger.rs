use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use columnar::{
    ColumnType, ColumnarReader, MergeRowOrder, RowAddr, ShuffleMergeOrder, StackMergeOrder,
};
use common::ReadOnlyBitSet;
use itertools::Itertools;
use measure_time::debug_time;

use crate::directory::WritePtr;
use crate::docset::{DocSet, TERMINATED};
use crate::error::DataCorruption;
use crate::fastfield::AliveBitSet;
use crate::fieldnorm::{FieldNormReader, FieldNormReaders, FieldNormsSerializer, FieldNormsWriter};
use crate::index::{Segment, SegmentComponent, SegmentReader};
use crate::indexer::doc_id_mapping::{MappingType, SegmentDocIdMapping};
use crate::indexer::SegmentSerializer;
use crate::postings::{InvertedIndexSerializer, Postings, SegmentPostings};
use crate::schema::{value_type_to_column_type, Field, FieldType, IndexRecordOption, Schema};
use crate::store::StoreWriter;
use crate::suffix_fst::builder::SuffixFstBuilder;
use crate::suffix_fst::encode_vint;
use crate::suffix_fst::file::{SfxFileReader, SfxFileWriter, SfxPostingsReader};
use crate::suffix_fst::gapmap::GapMapWriter;
use crate::termdict::{TermMerger, TermOrdinal};
use crate::{DocAddress, DocId, InvertedIndexReader};

/// Segment's max doc must be `< MAX_DOC_LIMIT`.
///
/// We do not allow segments with more than
pub const MAX_DOC_LIMIT: u32 = 1 << 31;

fn estimate_total_num_tokens_in_single_segment(
    reader: &SegmentReader,
    field: Field,
) -> crate::Result<u64> {
    // There are no deletes. We can simply use the exact value saved into the posting list.
    // Note that this value is not necessarily exact as it could have been the result of a merge
    // between segments themselves containing deletes.
    if !reader.has_deletes() {
        return Ok(reader.inverted_index(field)?.total_num_tokens());
    }

    // When there are deletes, we use an approximation either
    // by using the fieldnorm.
    if let Some(fieldnorm_reader) = reader.fieldnorms_readers().get_field(field)? {
        let mut count: [usize; 256] = [0; 256];
        for doc in reader.doc_ids_alive() {
            let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc);
            count[fieldnorm_id as usize] += 1;
        }
        let total_num_tokens = count
            .iter()
            .cloned()
            .enumerate()
            .map(|(fieldnorm_ord, count)| {
                count as u64 * u64::from(FieldNormReader::id_to_fieldnorm(fieldnorm_ord as u8))
            })
            .sum::<u64>();
        return Ok(total_num_tokens);
    }

    // There are no fieldnorms available.
    // Here we just do a pro-rata with the overall number of tokens an the ratio of
    // documents alive.
    let segment_num_tokens = reader.inverted_index(field)?.total_num_tokens();
    if reader.max_doc() == 0 {
        // That supposedly never happens, but let's be a bit defensive here.
        return Ok(0u64);
    }
    let ratio = reader.num_docs() as f64 / reader.max_doc() as f64;
    Ok((segment_num_tokens as f64 * ratio) as u64)
}

fn estimate_total_num_tokens(readers: &[SegmentReader], field: Field) -> crate::Result<u64> {
    let mut total_num_tokens: u64 = 0;
    for reader in readers {
        total_num_tokens += estimate_total_num_tokens_in_single_segment(reader, field)?;
    }
    Ok(total_num_tokens)
}

pub struct IndexMerger {
    pub(crate) schema: Schema,
    pub(crate) readers: Vec<SegmentReader>,
    pub(crate) max_doc: u32,
}

struct DeltaComputer {
    buffer: Vec<u32>,
}

impl DeltaComputer {
    fn new() -> DeltaComputer {
        DeltaComputer {
            buffer: vec![0u32; 512],
        }
    }

    fn compute_delta(&mut self, positions: &[u32]) -> &[u32] {
        if positions.len() > self.buffer.len() {
            self.buffer.resize(positions.len(), 0u32);
        }
        let mut last_pos = 0u32;
        for (cur_pos, dest) in positions.iter().cloned().zip(self.buffer.iter_mut()) {
            *dest = cur_pos - last_pos;
            last_pos = cur_pos;
        }
        &self.buffer[..positions.len()]
    }
}

fn convert_to_merge_order(
    columnars: &[&ColumnarReader],
    doc_id_mapping: SegmentDocIdMapping,
) -> MergeRowOrder {
    match doc_id_mapping.mapping_type() {
        MappingType::Stacked => MergeRowOrder::Stack(StackMergeOrder::stack(columnars)),
        MappingType::StackedWithDeletes => {
            // RUST/LLVM is amazing. The following conversion is actually a no-op:
            // no allocation, no copy.
            let new_row_id_to_old_row_id: Vec<RowAddr> = doc_id_mapping
                .new_doc_id_to_old_doc_addr
                .into_iter()
                .map(|doc_addr| RowAddr {
                    segment_ord: doc_addr.segment_ord,
                    row_id: doc_addr.doc_id,
                })
                .collect();
            MergeRowOrder::Shuffled(ShuffleMergeOrder {
                new_row_id_to_old_row_id,
                alive_bitsets: doc_id_mapping.alive_bitsets,
            })
        }
    }
}

fn extract_fast_field_required_columns(schema: &Schema) -> Vec<(String, ColumnType)> {
    schema
        .fields()
        .map(|(_, field_entry)| field_entry)
        .filter(|field_entry| field_entry.is_fast())
        .filter_map(|field_entry| {
            let column_name = field_entry.name().to_string();
            let column_type = value_type_to_column_type(field_entry.field_type().value_type())?;
            Some((column_name, column_type))
        })
        .collect()
}

impl IndexMerger {
    pub fn open(schema: Schema, segments: &[Segment]) -> crate::Result<IndexMerger> {
        let alive_bitset = segments.iter().map(|_| None).collect_vec();
        Self::open_with_custom_alive_set(schema, segments, alive_bitset)
    }

    // Create merge with a custom delete set.
    // For every Segment, a delete bitset can be provided, which
    // will be merged with the existing bit set. Make sure the index
    // corresponds to the segment index.
    //
    // If `None` is provided for custom alive set, the regular alive set will be used.
    // If a alive_bitset is provided, the union between the provided and regular
    // alive set will be used.
    //
    // This can be used to merge but also apply an additional filter.
    // One use case is demux, which is basically taking a list of
    // segments and partitions them e.g. by a value in a field.
    pub fn open_with_custom_alive_set(
        schema: Schema,
        segments: &[Segment],
        alive_bitset_opt: Vec<Option<AliveBitSet>>,
    ) -> crate::Result<IndexMerger> {
        let mut readers = vec![];
        for (segment, new_alive_bitset_opt) in segments.iter().zip(alive_bitset_opt) {
            if segment.meta().num_docs() > 0 {
                let reader =
                    SegmentReader::open_with_custom_alive_set(segment, new_alive_bitset_opt)?;
                readers.push(reader);
            }
        }

        let max_doc = readers.iter().map(|reader| reader.num_docs()).sum();
        // sort segments by their natural sort setting
        if max_doc >= MAX_DOC_LIMIT {
            let err_msg = format!(
                "The segment resulting from this merge would have {max_doc} docs,which exceeds \
                 the limit {MAX_DOC_LIMIT}."
            );
            return Err(crate::LucivyError::InvalidArgument(err_msg));
        }
        Ok(IndexMerger {
            schema,
            readers,
            max_doc,
        })
    }

    pub(crate) fn write_fieldnorms(
        &self,
        mut fieldnorms_serializer: FieldNormsSerializer,
        doc_id_mapping: &SegmentDocIdMapping,
    ) -> crate::Result<()> {
        let fields = FieldNormsWriter::fields_with_fieldnorm(&self.schema);
        let mut fieldnorms_data = Vec::with_capacity(self.max_doc as usize);
        for field in fields {
            fieldnorms_data.clear();
            let fieldnorms_readers: Vec<FieldNormReader> = self
                .readers
                .iter()
                .map(|reader| reader.get_fieldnorms_reader(field))
                .collect::<Result<_, _>>()?;
            for old_doc_addr in doc_id_mapping.iter_old_doc_addrs() {
                let fieldnorms_reader = &fieldnorms_readers[old_doc_addr.segment_ord as usize];
                let fieldnorm_id = fieldnorms_reader.fieldnorm_id(old_doc_addr.doc_id);
                fieldnorms_data.push(fieldnorm_id);
            }
            fieldnorms_serializer.serialize_field(field, &fieldnorms_data[..])?;
        }
        fieldnorms_serializer.close()?;
        Ok(())
    }

    pub(crate) fn write_fast_fields(
        &self,
        fast_field_wrt: &mut WritePtr,
        doc_id_mapping: SegmentDocIdMapping,
    ) -> crate::Result<()> {
        debug_time!("write-fast-fields");
        let required_columns = extract_fast_field_required_columns(&self.schema);
        let columnars: Vec<&ColumnarReader> = self
            .readers
            .iter()
            .map(|reader| reader.fast_fields().columnar())
            .collect();
        let merge_row_order = convert_to_merge_order(&columnars[..], doc_id_mapping);
        columnar::merge_columnar(
            &columnars[..],
            &required_columns,
            merge_row_order,
            fast_field_wrt,
        )?;
        Ok(())
    }

    /// Creates a mapping if the segments are stacked. this is helpful to merge codelines between
    /// index sorting and the others
    pub(crate) fn get_doc_id_from_concatenated_data(&self) -> crate::Result<SegmentDocIdMapping> {
        let total_num_new_docs = self
            .readers
            .iter()
            .map(|reader| reader.num_docs() as usize)
            .sum();

        let mut mapping: Vec<DocAddress> = Vec::with_capacity(total_num_new_docs);

        mapping.extend(
            self.readers
                .iter()
                .enumerate()
                .flat_map(|(segment_ord, reader)| {
                    reader.doc_ids_alive().map(move |doc_id| DocAddress {
                        segment_ord: segment_ord as u32,
                        doc_id,
                    })
                }),
        );

        let has_deletes: bool = self.readers.iter().any(SegmentReader::has_deletes);
        let mapping_type = if has_deletes {
            MappingType::StackedWithDeletes
        } else {
            MappingType::Stacked
        };
        let alive_bitsets: Vec<Option<ReadOnlyBitSet>> = self
            .readers
            .iter()
            .map(|reader| {
                let alive_bitset = reader.alive_bitset()?;
                Some(alive_bitset.bitset().clone())
            })
            .collect();
        Ok(SegmentDocIdMapping::new(
            mapping,
            mapping_type,
            alive_bitsets,
        ))
    }

    pub(crate) fn write_postings_for_field(
        &self,
        indexed_field: Field,
        _field_type: &FieldType,
        serializer: &mut InvertedIndexSerializer,
        fieldnorm_reader: Option<FieldNormReader>,
        doc_id_mapping: &SegmentDocIdMapping,
    ) -> crate::Result<()> {
        debug_time!("write-postings-for-field");
        let mut positions_buffer: Vec<u32> = Vec::with_capacity(1_000);
        let mut offsets_buffer: Vec<(u32, u32)> = Vec::new();
        let mut delta_computer = DeltaComputer::new();

        let mut max_term_ords: Vec<TermOrdinal> = Vec::new();

        let field_readers: Vec<Arc<InvertedIndexReader>> = self
            .readers
            .iter()
            .map(|reader| reader.inverted_index(indexed_field))
            .collect::<crate::Result<Vec<_>>>()?;

        let mut field_term_streams = Vec::new();
        for field_reader in &field_readers {
            let terms = field_reader.terms();
            field_term_streams.push(terms.stream()?);
            max_term_ords.push(terms.num_terms() as u64);
        }

        let mut merged_terms = TermMerger::new(field_term_streams);

        // map from segment doc ids to the resulting merged segment doc id.

        let mut merged_doc_id_map: Vec<Vec<Option<DocId>>> = self
            .readers
            .iter()
            .map(|reader| {
                let mut segment_local_map = vec![];
                segment_local_map.resize(reader.max_doc() as usize, None);
                segment_local_map
            })
            .collect();
        for (new_doc_id, old_doc_addr) in doc_id_mapping.iter_old_doc_addrs().enumerate() {
            let segment_map = &mut merged_doc_id_map[old_doc_addr.segment_ord as usize];
            segment_map[old_doc_addr.doc_id as usize] = Some(new_doc_id as DocId);
        }

        // Note that the total number of tokens is not exact.
        // It is only used as a parameter in the BM25 formula.
        let total_num_tokens: u64 = estimate_total_num_tokens(&self.readers, indexed_field)?;

        // Create the total list of doc ids
        // by stacking the doc ids from the different segment.
        //
        // In the new segments, the doc id from the different
        // segment are stacked so that :
        // - Segment 0's doc ids become doc id [0, seg.max_doc]
        // - Segment 1's doc ids become  [seg0.max_doc, seg0.max_doc + seg.max_doc]
        // - Segment 2's doc ids become  [seg0.max_doc + seg1.max_doc, seg0.max_doc + seg1.max_doc +
        //   seg2.max_doc]
        //
        // This stacking applies only when the index is not sorted, in that case the
        // doc_ids are kmerged by their sort property
        let mut field_serializer =
            serializer.new_field(indexed_field, total_num_tokens, fieldnorm_reader)?;

        let field_entry = self.schema.get_field_entry(indexed_field);

        // ... set segment postings option the new field.
        let segment_postings_option = field_entry.field_type().get_index_record_option().expect(
            "Encountered a field that is not supposed to be
                         indexed. Have you modified the schema?",
        );

        let mut segment_postings_containing_the_term: Vec<(usize, SegmentPostings)> = vec![];

        while merged_terms.advance() {
            segment_postings_containing_the_term.clear();
            let term_bytes: &[u8] = merged_terms.key();

            let mut total_doc_freq = 0;

            // Let's compute the list of non-empty posting lists
            for (segment_ord, term_info) in merged_terms.current_segment_ords_and_term_infos() {
                let segment_reader = &self.readers[segment_ord];
                let inverted_index: &InvertedIndexReader = &field_readers[segment_ord];
                let segment_postings = inverted_index
                    .read_postings_from_terminfo(&term_info, segment_postings_option)?;
                let alive_bitset_opt = segment_reader.alive_bitset();
                let doc_freq = if let Some(alive_bitset) = alive_bitset_opt {
                    segment_postings.doc_freq_given_deletes(alive_bitset)
                } else {
                    segment_postings.doc_freq()
                };
                if doc_freq > 0u32 {
                    total_doc_freq += doc_freq;
                    segment_postings_containing_the_term.push((segment_ord, segment_postings));
                }
            }

            // At this point, `segment_postings` contains the posting list
            // of all of the segments containing the given term (and that are non-empty)
            //
            // These segments are non-empty and advance has already been called.
            if total_doc_freq == 0u32 {
                // All docs that used to contain the term have been deleted. The `term` will be
                // entirely removed.
                continue;
            }

            // This should never happen as we early exited for total_doc_freq == 0.
            assert!(!segment_postings_containing_the_term.is_empty());

            let has_term_freq = {
                let has_term_freq = !segment_postings_containing_the_term[0]
                    .1
                    .block_cursor
                    .freqs()
                    .is_empty();
                for (_, postings) in &segment_postings_containing_the_term[1..] {
                    // This may look at a strange way to test whether we have term freq or not.
                    // With JSON object, the schema is not sufficient to know whether a term
                    // has its term frequency encoded or not:
                    // strings may have term frequencies, while number terms never have one.
                    //
                    // Ideally, we should have burnt one bit of two in the `TermInfo`.
                    // However, we preferred not changing the codec too much and detect this
                    // instead by
                    // - looking at the size of the skip data for bitpacked blocks
                    // - observing the absence of remaining data after reading the docs for vint
                    // blocks.
                    //
                    // Overall the reliable way to know if we have actual frequencies loaded or not
                    // is to check whether the actual decoded array is empty or not.
                    if has_term_freq == postings.block_cursor.freqs().is_empty() {
                        return Err(DataCorruption::comment_only(
                            "Term freqs are inconsistent across segments",
                        )
                        .into());
                    }
                }
                has_term_freq
            };

            field_serializer.new_term(term_bytes, total_doc_freq, has_term_freq)?;

            // We can now serialize this postings, by pushing each document to the
            // postings serializer.
            for (segment_ord, mut segment_postings) in
                segment_postings_containing_the_term.drain(..)
            {
                let old_to_new_doc_id = &merged_doc_id_map[segment_ord];

                let mut doc = segment_postings.doc();
                while doc != TERMINATED {
                    // deleted doc are skipped as they do not have a `remapped_doc_id`.
                    if let Some(remapped_doc_id) = old_to_new_doc_id[doc as usize] {
                        // we make sure to only write the term if
                        // there is at least one document.
                        let term_freq = if has_term_freq {
                            segment_postings.positions(&mut positions_buffer);
                            segment_postings.term_freq()
                        } else {
                            // The positions_buffer may contain positions from the previous term
                            // Existence of positions depend on the value type in JSON fields.
                            // https://github.com/quickwit-oss/tantivy/issues/2283
                            positions_buffer.clear();
                            0u32
                        };

                        let delta_positions = delta_computer.compute_delta(&positions_buffer);

                        if segment_postings_option.has_offsets() && has_term_freq {
                            segment_postings.offsets(&mut offsets_buffer);
                            // Convert absolute offsets to deltas for serialization.
                            let tf = term_freq as usize;
                            let mut offset_from_deltas = Vec::with_capacity(tf);
                            let mut offset_to_deltas = Vec::with_capacity(tf);
                            let mut prev_from = 0u32;
                            let mut prev_to = 0u32;
                            for &(from, to) in &offsets_buffer {
                                offset_from_deltas.push(from - prev_from);
                                offset_to_deltas.push(to - prev_to);
                                prev_from = from;
                                prev_to = to;
                            }
                            field_serializer.write_doc_with_offsets(
                                remapped_doc_id,
                                term_freq,
                                delta_positions,
                                &offset_from_deltas,
                                &offset_to_deltas,
                            );
                        } else {
                            field_serializer.write_doc(remapped_doc_id, term_freq, delta_positions);
                        }
                    }

                    doc = segment_postings.advance();
                }
            }
            // closing the term.
            field_serializer.close_term()?;
        }
        field_serializer.close()?;
        Ok(())
    }

    pub(crate) fn write_postings(
        &self,
        serializer: &mut InvertedIndexSerializer,
        fieldnorm_readers: FieldNormReaders,
        doc_id_mapping: &SegmentDocIdMapping,
    ) -> crate::Result<()> {
        for (field, field_entry) in self.schema.fields() {
            let fieldnorm_reader = fieldnorm_readers.get_field(field)?;
            if field_entry.is_indexed() {
                self.write_postings_for_field(
                    field,
                    field_entry.field_type(),
                    serializer,
                    fieldnorm_reader,
                    doc_id_mapping,
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn write_storable_fields(&self, store_writer: &mut StoreWriter) -> crate::Result<()> {
        debug_time!("write-storable-fields");
        debug!("write-storable-field");

        for reader in &self.readers {
            let store_reader = reader.get_store_reader(1)?;
            if reader.has_deletes()
                    // If there is not enough data in the store, we avoid stacking in order to
                    // avoid creating many small blocks in the doc store. Once we have 5 full blocks,
                    // we start stacking. In the worst case 2/7 of the blocks would be very small.
                    // [segment 1 - {1 doc}][segment 2 - {fullblock * 5}{1doc}]
                    // => 5 * full blocks, 2 * 1 document blocks
                    //
                    // In a more realistic scenario the segments are of the same size, so 1/6 of
                    // the doc stores would be on average half full, given total randomness (which
                    // is not the case here, but not sure how it behaves exactly).
                    //
                    // https://github.com/quickwit-oss/tantivy/issues/1053
                    //
                    // take 7 in order to not walk over all checkpoints.
                    || store_reader.block_checkpoints().take(7).count() < 6
                    || store_reader.decompressor() != store_writer.compressor().into()
            {
                for doc_bytes_res in store_reader.iter_raw(reader.alive_bitset()) {
                    let doc_bytes = doc_bytes_res?;
                    store_writer.store_bytes(&doc_bytes)?;
                }
            } else {
                store_writer.stack(store_reader)?;
            }
        }
        Ok(())
    }

    /// Writes the merged segment by pushing information
    /// to the `SegmentSerializer`.
    ///
    /// # Returns
    /// The number of documents in the resulting segment.
    pub fn write(&self, mut serializer: SegmentSerializer) -> crate::Result<u32> {
        use std::time::Instant;
        let merge_start = Instant::now();
        let num_readers = self.readers.len();
        let total_docs: u32 = self.readers.iter().map(|r| r.num_docs()).sum();
        lucivy_trace!("[merge] start: {} segments, {} docs total", num_readers, total_docs);

        let doc_id_mapping = self.get_doc_id_from_concatenated_data()?;

        let t = Instant::now();
        if let Some(fieldnorms_serializer) = serializer.extract_fieldnorms_serializer() {
            self.write_fieldnorms(fieldnorms_serializer, &doc_id_mapping)?;
        }
        lucivy_trace!("[merge]   fieldnorms: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let fieldnorm_data = serializer
            .segment()
            .open_read(SegmentComponent::FieldNorms)?;
        let fieldnorm_readers = FieldNormReaders::open(fieldnorm_data)?;
        self.write_postings(
            serializer.get_postings_serializer(),
            fieldnorm_readers,
            &doc_id_mapping,
        )?;
        lucivy_trace!("[merge]   postings: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        self.write_storable_fields(serializer.get_store_writer())?;
        lucivy_trace!("[merge]   stored_fields: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let sfx_doc_mapping: Vec<DocAddress> =
            doc_id_mapping.iter_old_doc_addrs().collect();
        self.write_fast_fields(serializer.get_fast_field_write(), doc_id_mapping)?;
        lucivy_trace!("[merge]   fast_fields: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        self.merge_sfx_deferred(&mut serializer, &sfx_doc_mapping)?;
        lucivy_trace!("[merge]   sfx_deferred: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        serializer.close()?;
        lucivy_trace!("[merge]   close: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

        lucivy_trace!("[merge] done: {} docs in {:.1}ms", self.max_doc, merge_start.elapsed().as_secs_f64() * 1000.0);
        Ok(self.max_doc)
    }

    /// Deferred sfx merge: copy gapmap + sfxpost (fast) but skip FST rebuild.
    ///
    /// The merged segment gets .sfxpost and gapmap data (via a partial .sfx file
    /// with empty FST) but no suffix FST. The FST is rebuilt at commit time from
    /// the term dictionary. Queries skip segments without a valid .sfx FST.
    ///
    /// This eliminates the expensive O(E log E) SuffixFstBuilder.build() during
    /// merges — the main scalability bottleneck on large indexes.
    pub(crate) fn merge_sfx_deferred(
        &self,
        serializer: &mut SegmentSerializer,
        doc_mapping: &[DocAddress],
    ) -> crate::Result<()> {
        let sfx_fields: Vec<Field> = self
            .schema
            .fields()
            .filter(|(field, _)| self.readers.iter().any(|r| r.sfx_file(*field).is_some()))
            .map(|(field, _)| field)
            .collect();

        if sfx_fields.is_empty() {
            return Ok(());
        }

        let mut reverse_doc_map: Vec<HashMap<DocId, DocId>> =
            vec![HashMap::new(); self.readers.len()];
        for (new_doc, old_addr) in doc_mapping.iter().enumerate() {
            reverse_doc_map[old_addr.segment_ord as usize]
                .insert(old_addr.doc_id, new_doc as DocId);
        }

        let mut sfx_field_ids = Vec::new();

        for &field in &sfx_fields {
            // Load source .sfx files
            let mut segment_sfx: Vec<Option<Vec<u8>>> = Vec::with_capacity(self.readers.len());
            let mut any_has_sfx = false;
            for reader in &self.readers {
                if let Some(file_slice) = reader.sfx_file(field) {
                    match file_slice.read_bytes() {
                        Ok(bytes) => { segment_sfx.push(Some(bytes.to_vec())); any_has_sfx = true; }
                        Err(_) => segment_sfx.push(None),
                    }
                } else {
                    segment_sfx.push(None);
                }
            }
            if !any_has_sfx { continue; }

            let sfx_readers: Vec<Option<SfxFileReader<'_>>> = segment_sfx
                .iter()
                .map(|opt| opt.as_ref().and_then(|bytes| SfxFileReader::open(bytes).ok()))
                .collect();

            // Step 1: collect unique tokens (fast path, no alive check needed for ordering)
            let has_deletes = self.readers.iter().any(|r| r.alive_bitset().is_some());
            let mut unique_tokens = BTreeSet::new();
            if has_deletes {
                for (seg_ord, reader) in self.readers.iter().enumerate() {
                    if let Ok(inv_idx) = reader.inverted_index(field) {
                        let term_dict = inv_idx.terms();
                        let alive = reader.alive_bitset();
                        let mut stream = term_dict.stream()?;
                        while stream.advance() {
                            if let Ok(s) = std::str::from_utf8(stream.key()) {
                                let ti = stream.value().clone();
                                let mut postings = inv_idx.read_postings_from_terminfo(
                                    &ti, IndexRecordOption::Basic)?;
                                let has_alive_doc = loop {
                                    let doc = postings.doc();
                                    if doc == TERMINATED { break false; }
                                    let is_alive = alive.map_or(true, |bs| bs.is_alive(doc));
                                    if is_alive && reverse_doc_map[seg_ord].contains_key(&doc) {
                                        break true;
                                    }
                                    postings.advance();
                                };
                                if has_alive_doc {
                                    unique_tokens.insert(s.to_string());
                                }
                            }
                        }
                    }
                }
            } else {
                for reader in &self.readers {
                    if let Ok(inv_idx) = reader.inverted_index(field) {
                        let mut stream = inv_idx.terms().stream()?;
                        while stream.advance() {
                            if let Ok(s) = std::str::from_utf8(stream.key()) {
                                unique_tokens.insert(s.to_string());
                            }
                        }
                    }
                }
            }

            // Step 2: SKIP FST rebuild (deferred to commit time)

            // Step 3: Copy GapMap in merge order
            let mut gapmap_writer = GapMapWriter::new();
            for &doc_addr in doc_mapping {
                let seg_ord = doc_addr.segment_ord as usize;
                let old_doc_id = doc_addr.doc_id;
                if let Some(Some(sfx_reader)) = sfx_readers.get(seg_ord) {
                    let doc_data = sfx_reader.gapmap().doc_data(old_doc_id);
                    gapmap_writer.add_doc_raw(doc_data);
                } else {
                    gapmap_writer.add_empty_doc();
                }
            }

            // Step 4: Merge sfxpost with doc_id remapping
            let mut sfxpost_data: Option<Vec<u8>> = None;
            {
                let mut segment_sfxpost: Vec<Option<Vec<u8>>> = Vec::with_capacity(self.readers.len());
                let mut any_has_sfxpost = false;
                for reader in &self.readers {
                    if let Some(file_slice) = reader.sfxpost_file(field) {
                        match file_slice.read_bytes() {
                            Ok(bytes) => { segment_sfxpost.push(Some(bytes.to_vec())); any_has_sfxpost = true; }
                            Err(_) => segment_sfxpost.push(None),
                        }
                    } else {
                        segment_sfxpost.push(None);
                    }
                }

                if any_has_sfxpost {
                    let sfxpost_readers: Vec<Option<SfxPostingsReader<'_>>> = segment_sfxpost
                        .iter()
                        .map(|opt| opt.as_ref().and_then(|b| SfxPostingsReader::open(b).ok()))
                        .collect();

                    let mut token_to_ordinal: Vec<HashMap<String, u32>> = Vec::with_capacity(self.readers.len());
                    for reader in &self.readers {
                        let mut map = HashMap::new();
                        if let Ok(inv_idx) = reader.inverted_index(field) {
                            let term_dict = inv_idx.terms();
                            let mut stream = term_dict.stream()?;
                            let mut ord = 0u32;
                            while stream.advance() {
                                if let Ok(s) = std::str::from_utf8(stream.key()) {
                                    map.insert(s.to_string(), ord);
                                }
                                ord += 1;
                            }
                        }
                        token_to_ordinal.push(map);
                    }

                    let mut posting_offsets: Vec<u32> = Vec::with_capacity(unique_tokens.len() + 1);
                    let mut posting_bytes: Vec<u8> = Vec::new();

                    for token in &unique_tokens {
                        posting_offsets.push(posting_bytes.len() as u32);
                        let mut merged: Vec<(u32, u32, u32, u32)> = Vec::new();
                        for (seg_ord, sfxpost_reader) in sfxpost_readers.iter().enumerate() {
                            if let Some(reader) = sfxpost_reader {
                                if let Some(&old_ord) = token_to_ordinal[seg_ord].get(token.as_str()) {
                                    for e in reader.entries(old_ord) {
                                        if let Some(&new_doc) = reverse_doc_map[seg_ord].get(&e.doc_id) {
                                            merged.push((new_doc, e.token_index, e.byte_from, e.byte_to));
                                        }
                                    }
                                }
                            }
                        }
                        merged.sort_unstable();
                        for &(doc_id, ti, byte_from, byte_to) in &merged {
                            encode_vint(doc_id, &mut posting_bytes);
                            encode_vint(ti, &mut posting_bytes);
                            encode_vint(byte_from, &mut posting_bytes);
                            encode_vint(byte_to, &mut posting_bytes);
                        }
                    }
                    posting_offsets.push(posting_bytes.len() as u32);

                    let mut data = Vec::new();
                    data.extend_from_slice(&(unique_tokens.len() as u32).to_le_bytes());
                    for &off in &posting_offsets {
                        data.extend_from_slice(&off.to_le_bytes());
                    }
                    data.extend_from_slice(&posting_bytes);
                    sfxpost_data = Some(data);
                }
            }

            // Write gapmap as a partial .sfx file (empty FST, gapmap only).
            // The FST will be rebuilt at commit time.
            let gapmap_data = gapmap_writer.serialize();
            let sfx_file = SfxFileWriter::new(
                Vec::new(),  // empty FST — rebuilt at commit
                Vec::new(),  // empty parent list
                gapmap_data,
                doc_mapping.len() as u32,
                0,           // no suffix terms yet
            );
            let sfx_bytes = sfx_file.to_bytes();
            serializer.write_sfx(field.field_id(), &sfx_bytes)?;
            if let Some(ref sfxpost) = sfxpost_data {
                serializer.write_sfxpost(field.field_id(), sfxpost)?;
            }
            sfx_field_ids.push(field.field_id());
        }

        if !sfx_field_ids.is_empty() {
            serializer.write_sfx_manifest(&sfx_field_ids)?;
        }

        Ok(())
    }

    /// Merge .sfx files from source segments into the merged segment.
    ///
    /// For each `._raw` field that has .sfx in at least one source segment:
    /// 1. Collect all unique tokens from source term dictionaries
    /// 2. Rebuild suffix FST with SuffixFstBuilder
    /// 3. Copy GapMap data per-doc in merge order
    pub(crate) fn merge_sfx(
        &self,
        serializer: &mut SegmentSerializer,
        doc_mapping: &[DocAddress],
    ) -> crate::Result<()> {
        // Find fields that have .sfx in at least one source segment.
        let sfx_fields: Vec<Field> = self
            .schema
            .fields()
            .filter(|(field, _)| self.readers.iter().any(|r| r.sfx_file(*field).is_some()))
            .map(|(field, _)| field)
            .collect();

        // Log per-reader sfx status to diagnose segments without sfx
        for (i, reader) in self.readers.iter().enumerate() {
            let seg_id = reader.segment_id().uuid_string();
            let has_any_sfx = self.schema.fields().any(|(f, _)| reader.sfx_file(f).is_some());
            if !has_any_sfx {
                eprintln!("[merge_sfx] reader[{}] seg={} ({} docs): NO SFX FILES",
                    i, &seg_id[..8], reader.num_docs());
            }
        }

        eprintln!("[merge_sfx] sfx_fields: {:?} ({} readers, {} docs)",
            sfx_fields.iter().map(|f| f.field_id()).collect::<Vec<_>>(),
            self.readers.len(),
            doc_mapping.len());
        if sfx_fields.is_empty() {
            eprintln!("[merge_sfx] NO sfx fields found — ALL {} readers missing sfx, producing segment WITHOUT sfx!",
                self.readers.len());
            return Ok(());
        }

        // Reverse doc mapping for .sfxpost merge: (seg_ord, old_doc) → new_doc
        // Computed once, shared across all fields.
        let mut reverse_doc_map: Vec<HashMap<DocId, DocId>> =
            vec![HashMap::new(); self.readers.len()];
        for (new_doc, old_addr) in doc_mapping.iter().enumerate() {
            reverse_doc_map[old_addr.segment_ord as usize]
                .insert(old_addr.doc_id, new_doc as DocId);
        }

        let mut sfx_field_ids = Vec::new();

        for &field in &sfx_fields {
            // Load .sfx bytes from each source segment
            let mut segment_sfx: Vec<Option<Vec<u8>>> = Vec::with_capacity(self.readers.len());
            let mut any_has_sfx = false;

            for reader in &self.readers {
                if let Some(file_slice) = reader.sfx_file(field) {
                    match file_slice.read_bytes() {
                        Ok(bytes) => {
                            segment_sfx.push(Some(bytes.to_vec()));
                            any_has_sfx = true;
                        }
                        Err(_) => segment_sfx.push(None),
                    }
                } else {
                    segment_sfx.push(None);
                }
            }

            if !any_has_sfx {
                continue;
            }

            // Parse the SfxFileReaders
            let sfx_readers: Vec<Option<SfxFileReader<'_>>> = segment_sfx
                .iter()
                .map(|opt| {
                    opt.as_ref()
                        .and_then(|bytes| SfxFileReader::open(bytes).ok())
                })
                .collect();

            // 1. Collect unique tokens that have at least one alive document.
            let has_deletes = self.readers.iter().any(|r| r.alive_bitset().is_some());
            let mut unique_tokens = BTreeSet::new();

            if has_deletes {
                // Slow path: must check each term's postings for alive docs.
                // Terms from deleted documents get purged in the merged TermDictionary,
                // causing ordinal mismatches if we include them in .sfx.
                for (seg_ord, reader) in self.readers.iter().enumerate() {
                    if let Ok(inv_idx) = reader.inverted_index(field) {
                        let term_dict = inv_idx.terms();
                        let alive = reader.alive_bitset();
                        let mut stream = term_dict.stream()?;
                        while stream.advance() {
                            if let Ok(s) = std::str::from_utf8(stream.key()) {
                                let ti = stream.value().clone();
                                let mut postings = inv_idx.read_postings_from_terminfo(
                                    &ti, IndexRecordOption::Basic)?;
                                let has_alive_doc = loop {
                                    let doc = postings.doc();
                                    if doc == TERMINATED { break false; }
                                    let is_alive = alive.map_or(true, |bs| bs.is_alive(doc));
                                    if is_alive && reverse_doc_map[seg_ord].contains_key(&doc) {
                                        break true;
                                    }
                                    postings.advance();
                                };
                                if has_alive_doc {
                                    unique_tokens.insert(s.to_string());
                                }
                            }
                        }
                    }
                }
            } else {
                // Fast path: no deletes — all terms are alive, skip postings check.
                for reader in &self.readers {
                    if let Ok(inv_idx) = reader.inverted_index(field) {
                        let mut stream = inv_idx.terms().stream()?;
                        while stream.advance() {
                            if let Ok(s) = std::str::from_utf8(stream.key()) {
                                unique_tokens.insert(s.to_string());
                            }
                        }
                    }
                }
            }

            // 2. Build suffix FST
            eprintln!("[merge_sfx] field {} : {} unique tokens collected, has_deletes={}, {} readers",
                field.field_id(), unique_tokens.len(), has_deletes, self.readers.len());
            if unique_tokens.len() < 5 {
                eprintln!("[merge_sfx]   tokens: {:?}", unique_tokens.iter().take(10).collect::<Vec<_>>());
            }
            let mut sfx_builder = SuffixFstBuilder::new();
            for (ordinal, token) in unique_tokens.iter().enumerate() {
                sfx_builder.add_token(token, ordinal as u64);
            }
            let (fst_data, parent_list_data) = sfx_builder.build().map_err(|e| {
                crate::LucivyError::SystemError(format!("merge sfx build: {e}"))
            })?;

            // 3. Build GapMap by copying doc data in merge order
            let mut gapmap_writer = GapMapWriter::new();
            for &doc_addr in doc_mapping {
                let seg_ord = doc_addr.segment_ord as usize;
                let old_doc_id = doc_addr.doc_id;

                if let Some(Some(sfx_reader)) = sfx_readers.get(seg_ord) {
                    let doc_data = sfx_reader.gapmap().doc_data(old_doc_id);
                    gapmap_writer.add_doc_raw(doc_data);
                } else {
                    // Source segment had no .sfx — add empty doc
                    gapmap_writer.add_empty_doc();
                }
            }

            // 4. Reconstruct .sfxpost by merging posting entries with doc_id remapping
            let mut sfxpost_data: Option<Vec<u8>> = None;
            {
                let mut segment_sfxpost: Vec<Option<Vec<u8>>> = Vec::with_capacity(self.readers.len());
                let mut any_has_sfxpost = false;
                let mut missing_sfxpost = Vec::new();
                for (seg_ord, reader) in self.readers.iter().enumerate() {
                    if let Some(file_slice) = reader.sfxpost_file(field) {
                        match file_slice.read_bytes() {
                            Ok(bytes) => {
                                segment_sfxpost.push(Some(bytes.to_vec()));
                                any_has_sfxpost = true;
                            }
                            Err(_) => {
                                segment_sfxpost.push(None);
                                missing_sfxpost.push((seg_ord, reader.num_docs(), "read_err"));
                            }
                        }
                    } else {
                        segment_sfxpost.push(None);
                        missing_sfxpost.push((seg_ord, reader.num_docs(), "no_file"));
                    }
                }
                if !missing_sfxpost.is_empty() {
                    for &(seg_ord, ndocs, reason) in &missing_sfxpost {
                        let has_sfx = self.readers[seg_ord].sfx_file(field).is_some();
                        let seg_id = self.readers[seg_ord].segment_id().uuid_string();
                        eprintln!("[merge_sfx] WARNING: seg={} ({} docs) missing sfxpost ({}), has_sfx={}",
                            &seg_id[..8], ndocs, reason, has_sfx);
                    }
                }

                if any_has_sfxpost {
                    let sfxpost_readers: Vec<Option<SfxPostingsReader<'_>>> = segment_sfxpost
                        .iter()
                        .map(|opt| opt.as_ref().and_then(|b| SfxPostingsReader::open(b).ok()))
                        .collect();

                    // token → old ordinal for each source segment
                    let mut token_to_ordinal: Vec<HashMap<String, u32>> = Vec::with_capacity(self.readers.len());
                    for reader in &self.readers {
                        let mut map = HashMap::new();
                        if let Ok(inv_idx) = reader.inverted_index(field) {
                            let term_dict = inv_idx.terms();
                            let mut stream = term_dict.stream()?;
                            let mut ord = 0u32;
                            while stream.advance() {
                                if let Ok(s) = std::str::from_utf8(stream.key()) {
                                    map.insert(s.to_string(), ord);
                                }
                                ord += 1;
                            }
                        }
                        token_to_ordinal.push(map);
                    }

                    // Merge entries per token in BTreeSet order (= new ordinal order)
                    let mut posting_offsets: Vec<u32> = Vec::with_capacity(unique_tokens.len() + 1);
                    let mut posting_bytes: Vec<u8> = Vec::new();

                    for token in &unique_tokens {
                        posting_offsets.push(posting_bytes.len() as u32);
                        let mut merged: Vec<(u32, u32, u32, u32)> = Vec::new();

                        for (seg_ord, sfxpost_reader) in sfxpost_readers.iter().enumerate() {
                            if let Some(reader) = sfxpost_reader {
                                if let Some(&old_ord) = token_to_ordinal[seg_ord].get(token.as_str()) {
                                    for e in reader.entries(old_ord) {
                                        if let Some(&new_doc) = reverse_doc_map[seg_ord].get(&e.doc_id) {
                                            merged.push((new_doc, e.token_index, e.byte_from, e.byte_to));
                                        }
                                    }
                                }
                            }
                        }

                        merged.sort_unstable();
                        for &(doc_id, ti, byte_from, byte_to) in &merged {
                            encode_vint(doc_id, &mut posting_bytes);
                            encode_vint(ti, &mut posting_bytes);
                            encode_vint(byte_from, &mut posting_bytes);
                            encode_vint(byte_to, &mut posting_bytes);
                        }
                    }
                    posting_offsets.push(posting_bytes.len() as u32);

                    let mut data = Vec::new();
                    data.extend_from_slice(&(unique_tokens.len() as u32).to_le_bytes());
                    for &off in &posting_offsets {
                        data.extend_from_slice(&off.to_le_bytes());
                    }
                    data.extend_from_slice(&posting_bytes);
                    sfxpost_data = Some(data);
                }
            }

            // 5. Validate gapmap before writing
            let gapmap_data = gapmap_writer.serialize();
            {
                use crate::suffix_fst::gapmap::GapMapReader;
                let validation_reader = GapMapReader::open(&gapmap_data);
                let errors = validation_reader.validate();
                if !errors.is_empty() {
                    eprintln!("[merge_sfx] GAPMAP VALIDATION FAILED for field {}: {} errors in {} docs",
                        field.field_id(), errors.len(), doc_mapping.len());
                    for (i, err) in errors.iter().enumerate().take(10) {
                        eprintln!("[merge_sfx]   error {}: {}", i, err);
                    }
                    // Log source segment details for diagnosis
                    for (seg_ord, reader) in self.readers.iter().enumerate() {
                        let has_sfx = reader.sfx_file(field).is_some();
                        let seg_id = reader.segment_id().uuid_string();
                        eprintln!("[merge_sfx]   source seg[{}] {} ({} docs) has_sfx={}",
                            seg_ord, &seg_id[..8], reader.num_docs(), has_sfx);
                    }
                }
            }

            // 6. Assemble and write .sfx + .sfxpost
            let sfx_file = SfxFileWriter::new(
                fst_data,
                parent_list_data,
                gapmap_data,
                doc_mapping.len() as u32,
                unique_tokens.len() as u32,
            );
            let sfx_bytes = sfx_file.to_bytes();
            serializer.write_sfx(field.field_id(), &sfx_bytes)?;
            if let Some(ref sfxpost) = sfxpost_data {
                serializer.write_sfxpost(field.field_id(), sfxpost)?;
            }
            sfx_field_ids.push(field.field_id());
        }

        if !sfx_field_ids.is_empty() {
            serializer.write_sfx_manifest(&sfx_field_ids)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use columnar::Column;
    use proptest::prop_oneof;
    use proptest::strategy::Strategy;
    use schema::FAST;

    use crate::collector::tests::{
        BytesFastFieldTestCollector, FastFieldTestCollector, TEST_COLLECTOR_WITH_SCORE,
    };
    use crate::collector::{Count, FacetCollector};
    use crate::index::{Index, SegmentId};
    use crate::indexer::NoMergePolicy;
    use crate::query::{AllQuery, BooleanQuery, EnableScoring, Scorer, TermQuery};
    use crate::schema::{
        Facet, FacetOptions, IndexRecordOption, NumericOptions, LucivyDocument, Term,
        TextFieldIndexing, Value, INDEXED, TEXT,
    };
    use crate::time::OffsetDateTime;
    use crate::{
        assert_nearly_equals, schema, DateTime, DocAddress, DocId, DocSet, IndexSettings,
        IndexWriter, Searcher,
    };

    #[test]
    fn test_index_merger_no_deletes() -> crate::Result<()> {
        let mut schema_builder = schema::Schema::builder();
        let text_fieldtype = schema::TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default().set_index_option(IndexRecordOption::WithFreqs),
            )
            .set_stored();
        let text_field = schema_builder.add_text_field("text", text_fieldtype);
        let date_field = schema_builder.add_date_field("date", INDEXED);
        let score_fieldtype = schema::NumericOptions::default().set_fast();
        let score_field = schema_builder.add_u64_field("score", score_fieldtype);
        let bytes_score_field = schema_builder.add_bytes_field("score_bytes", FAST);
        let index = Index::create_in_ram(schema_builder.build());
        let reader = index.reader()?;
        let curr_time = OffsetDateTime::now_utc();
        {
            let mut index_writer = index.writer_for_tests()?;
            // writing the segment
            index_writer.add_document(doc!(
                text_field => "af b",
                score_field => 3u64,
                date_field => DateTime::from_utc(curr_time),
                bytes_score_field => 3u32.to_be_bytes().as_ref()
            ))?;
            index_writer.add_document(doc!(
                text_field => "a b c",
                score_field => 5u64,
                bytes_score_field => 5u32.to_be_bytes().as_ref()
            ))?;
            index_writer.add_document(doc!(
                text_field => "a b c d",
                score_field => 7u64,
                bytes_score_field => 7u32.to_be_bytes().as_ref()
            ))?;
            index_writer.commit()?;
            // writing the segment
            index_writer.add_document(doc!(
                text_field => "af b",
                date_field => DateTime::from_utc(curr_time),
                score_field => 11u64,
                bytes_score_field => 11u32.to_be_bytes().as_ref()
            ))?;
            index_writer.add_document(doc!(
                text_field => "a b c g",
                score_field => 13u64,
                bytes_score_field => 13u32.to_be_bytes().as_ref()
            ))?;
            index_writer.commit()?;
        }
        {
            let segment_ids = index
                .searchable_segment_ids()
                .expect("Searchable segments failed.");
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.merge(&segment_ids)?;
            index_writer.wait_merging_threads()?;
        }
        {
            reader.reload()?;
            let searcher = reader.searcher();
            let get_doc_ids = |terms: Vec<Term>| {
                let query = BooleanQuery::new_multiterms_query(terms);
                searcher
                    .search(&query, &TEST_COLLECTOR_WITH_SCORE)
                    .map(|top_docs| top_docs.docs().to_vec())
            };
            {
                assert_eq!(
                    get_doc_ids(vec![Term::from_field_text(text_field, "a")])?,
                    vec![
                        DocAddress::new(0, 1),
                        DocAddress::new(0, 2),
                        DocAddress::new(0, 4)
                    ]
                );
                assert_eq!(
                    get_doc_ids(vec![Term::from_field_text(text_field, "af")])?,
                    vec![DocAddress::new(0, 0), DocAddress::new(0, 3)]
                );
                assert_eq!(
                    get_doc_ids(vec![Term::from_field_text(text_field, "g")])?,
                    vec![DocAddress::new(0, 4)]
                );
                assert_eq!(
                    get_doc_ids(vec![Term::from_field_text(text_field, "b")])?,
                    vec![
                        DocAddress::new(0, 0),
                        DocAddress::new(0, 1),
                        DocAddress::new(0, 2),
                        DocAddress::new(0, 3),
                        DocAddress::new(0, 4)
                    ]
                );
                assert_eq!(
                    get_doc_ids(vec![Term::from_field_date_for_search(
                        date_field,
                        DateTime::from_utc(curr_time)
                    )])?,
                    vec![DocAddress::new(0, 0), DocAddress::new(0, 3)]
                );
            }
            {
                let doc = searcher.doc::<LucivyDocument>(DocAddress::new(0, 0))?;
                assert_eq!(
                    doc.get_first(text_field).unwrap().as_value().as_str(),
                    Some("af b")
                );
            }
            {
                let doc = searcher.doc::<LucivyDocument>(DocAddress::new(0, 1))?;
                assert_eq!(
                    doc.get_first(text_field).unwrap().as_value().as_str(),
                    Some("a b c")
                );
            }
            {
                let doc = searcher.doc::<LucivyDocument>(DocAddress::new(0, 2))?;
                assert_eq!(
                    doc.get_first(text_field).unwrap().as_value().as_str(),
                    Some("a b c d")
                );
            }
            {
                let doc = searcher.doc::<LucivyDocument>(DocAddress::new(0, 3))?;
                assert_eq!(doc.get_first(text_field).unwrap().as_str(), Some("af b"));
            }
            {
                let doc = searcher.doc::<LucivyDocument>(DocAddress::new(0, 4))?;
                assert_eq!(doc.get_first(text_field).unwrap().as_str(), Some("a b c g"));
            }

            {
                let get_fast_vals = |terms: Vec<Term>| {
                    let query = BooleanQuery::new_multiterms_query(terms);
                    searcher.search(&query, &FastFieldTestCollector::for_field("score"))
                };
                let get_fast_vals_bytes = |terms: Vec<Term>| {
                    let query = BooleanQuery::new_multiterms_query(terms);
                    searcher.search(
                        &query,
                        &BytesFastFieldTestCollector::for_field("score_bytes"),
                    )
                };
                assert_eq!(
                    get_fast_vals(vec![Term::from_field_text(text_field, "a")])?,
                    vec![5, 7, 13]
                );
                assert_eq!(
                    get_fast_vals_bytes(vec![Term::from_field_text(text_field, "a")])?,
                    vec![0, 0, 0, 5, 0, 0, 0, 7, 0, 0, 0, 13]
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_index_merger_with_deletes() -> crate::Result<()> {
        let mut schema_builder = schema::Schema::builder();
        let text_fieldtype = schema::TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default().set_index_option(IndexRecordOption::WithFreqs),
            )
            .set_stored();
        let text_field = schema_builder.add_text_field("text", text_fieldtype);
        let score_fieldtype = schema::NumericOptions::default().set_fast();
        let score_field = schema_builder.add_u64_field("score", score_fieldtype);
        let bytes_score_field = schema_builder.add_bytes_field("score_bytes", FAST);
        let index = Index::create_in_ram(schema_builder.build());
        let mut index_writer = index.writer_for_tests()?;
        let reader = index.reader().unwrap();
        let search_term = |searcher: &Searcher, term: Term| {
            let collector = FastFieldTestCollector::for_field("score");
            // let bytes_collector = BytesFastFieldTestCollector::for_field(bytes_score_field);
            let term_query = TermQuery::new(term, IndexRecordOption::Basic);
            // searcher
            //     .search(&term_query, &(collector, bytes_collector))
            //     .map(|(scores, bytes)| {
            //         let mut score_bytes = &bytes[..];
            //         for &score in &scores {
            //             assert_eq!(score as u32, score_bytes.read_u32::<BigEndian>().unwrap());
            //         }
            //         scores
            //     })
            searcher.search(&term_query, &collector)
        };

        let empty_vec = Vec::<u64>::new();
        {
            // a first commit
            index_writer.add_document(doc!(
                text_field => "a b d",
                score_field => 1u64,
                bytes_score_field => vec![0u8, 0, 0, 1],
            ))?;
            index_writer.add_document(doc!(
                text_field => "b c",
                score_field => 2u64,
                bytes_score_field => vec![0u8, 0, 0, 2],
            ))?;
            index_writer.delete_term(Term::from_field_text(text_field, "c"));
            index_writer.add_document(doc!(
                text_field => "c d",
                score_field => 3u64,
                bytes_score_field => vec![0u8, 0, 0, 3],
            ))?;
            index_writer.commit()?;
            reader.reload()?;
            let searcher = reader.searcher();
            assert_eq!(searcher.num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].max_doc(), 3);
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "a"))?,
                vec![1]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "b"))?,
                vec![1]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "c"))?,
                vec![3]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "d"))?,
                vec![1, 3]
            );
        }
        {
            // a second commit
            index_writer.add_document(doc!(
                text_field => "a d e",
                score_field => 4_000u64,
                bytes_score_field => vec![0u8, 0, 0, 4],
            ))?;
            index_writer.add_document(doc!(
                text_field => "e f",
                score_field => 5_000u64,
                bytes_score_field => vec![0u8, 0, 0, 5],
            ))?;
            index_writer.delete_term(Term::from_field_text(text_field, "a"));
            index_writer.delete_term(Term::from_field_text(text_field, "f"));
            index_writer.add_document(doc!(
                text_field => "f g",
                score_field => 6_000u64,
                bytes_score_field => vec![0u8, 0, 23, 112],
            ))?;
            index_writer.add_document(doc!(
                text_field => "g h",
                score_field => 7_000u64,
                bytes_score_field => vec![0u8, 0, 27, 88],
            ))?;
            index_writer.commit()?;
            reader.reload()?;
            let searcher = reader.searcher();

            assert_eq!(searcher.segment_readers().len(), 2);
            assert_eq!(searcher.num_docs(), 3);
            assert_eq!(searcher.segment_readers()[0].num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].max_doc(), 4);
            assert_eq!(searcher.segment_readers()[1].num_docs(), 1);
            assert_eq!(searcher.segment_readers()[1].max_doc(), 3);
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "a"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "b"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "c"))?,
                vec![3]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "d"))?,
                vec![3]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "e"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "f"))?,
                vec![6_000]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "g"))?,
                vec![6_000, 7_000]
            );

            let score_field_reader = searcher
                .segment_reader(0)
                .fast_fields()
                .u64("score")
                .unwrap();
            assert_eq!(score_field_reader.min_value(), 4000);
            assert_eq!(score_field_reader.max_value(), 7000);

            let score_field_reader = searcher
                .segment_reader(1)
                .fast_fields()
                .u64("score")
                .unwrap();
            assert_eq!(score_field_reader.min_value(), 1);
            assert_eq!(score_field_reader.max_value(), 3);
        }
        {
            // merging the segments
            let segment_ids = index.searchable_segment_ids()?;
            index_writer.merge(&segment_ids)?;
            reader.reload()?;
            let searcher = reader.searcher();
            assert_eq!(searcher.segment_readers().len(), 1);
            assert_eq!(searcher.num_docs(), 3);
            assert_eq!(searcher.segment_readers()[0].num_docs(), 3);
            assert_eq!(searcher.segment_readers()[0].max_doc(), 3);
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "a"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "b"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "c"))?,
                vec![3]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "d"))?,
                vec![3]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "e"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "f"))?,
                vec![6_000]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "g"))?,
                vec![6_000, 7_000]
            );
            let score_field_reader = searcher
                .segment_reader(0)
                .fast_fields()
                .u64("score")
                .unwrap();
            assert_eq!(score_field_reader.min_value(), 3);
            assert_eq!(score_field_reader.max_value(), 7000);
        }
        {
            // test a commit with only deletes
            index_writer.delete_term(Term::from_field_text(text_field, "c"));
            index_writer.commit()?;

            reader.reload()?;
            let searcher = reader.searcher();
            assert_eq!(searcher.segment_readers().len(), 1);
            assert_eq!(searcher.num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].max_doc(), 3);
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "a"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "b"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "c"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "d"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "e"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "f"))?,
                vec![6_000]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "g"))?,
                vec![6_000, 7_000]
            );
            let score_field_reader = searcher
                .segment_reader(0)
                .fast_fields()
                .u64("score")
                .unwrap();
            assert_eq!(score_field_reader.min_value(), 3);
            assert_eq!(score_field_reader.max_value(), 7000);
        }
        {
            // Test merging a single segment in order to remove deletes.
            let segment_ids = index.searchable_segment_ids()?;
            index_writer.merge(&segment_ids)?;
            reader.reload()?;

            let searcher = reader.searcher();
            assert_eq!(searcher.segment_readers().len(), 1);
            assert_eq!(searcher.num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].num_docs(), 2);
            assert_eq!(searcher.segment_readers()[0].max_doc(), 2);
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "a"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "b"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "c"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "d"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "e"))?,
                empty_vec
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "f"))?,
                vec![6_000]
            );
            assert_eq!(
                search_term(&searcher, Term::from_field_text(text_field, "g"))?,
                vec![6_000, 7_000]
            );
            let score_field_reader = searcher
                .segment_reader(0)
                .fast_fields()
                .u64("score")
                .unwrap();
            assert_eq!(score_field_reader.min_value(), 6000);
            assert_eq!(score_field_reader.max_value(), 7000);
        }

        {
            // Test removing all docs
            index_writer.delete_term(Term::from_field_text(text_field, "g"));
            index_writer.commit()?;
            let segment_ids = index.searchable_segment_ids()?;
            reader.reload()?;

            let searcher = reader.searcher();
            assert!(segment_ids.is_empty());
            assert!(searcher.segment_readers().is_empty());
            assert_eq!(searcher.num_docs(), 0);
        }
        Ok(())
    }

    #[test]
    fn test_merge_facets_sort_none() {
        test_merge_facets(None, true)
    }

    // force_segment_value_overlap forces the int value for sorting to have overlapping min and max
    // ranges between segments so that merge algorithm can't apply certain optimizations
    fn test_merge_facets(index_settings: Option<IndexSettings>, force_segment_value_overlap: bool) {
        let mut schema_builder = schema::Schema::builder();
        let facet_field = schema_builder.add_facet_field("facet", FacetOptions::default());
        let int_options = NumericOptions::default().set_fast().set_indexed();
        let int_field = schema_builder.add_u64_field("intval", int_options);
        let mut index_builder = Index::builder().schema(schema_builder.build());
        if let Some(settings) = index_settings {
            index_builder = index_builder.settings(settings);
        }
        let index = index_builder.create_in_ram().unwrap();
        // let index = Index::create_in_ram(schema_builder.build());
        let reader = index.reader().unwrap();
        let mut int_val = 0;
        {
            let mut index_writer: IndexWriter = index.writer_for_tests().unwrap();
            let index_doc =
                |index_writer: &mut IndexWriter, doc_facets: &[&str], int_val: &mut u64| {
                    let mut doc = LucivyDocument::default();
                    for facet in doc_facets {
                        doc.add_facet(facet_field, Facet::from(facet));
                    }
                    doc.add_u64(int_field, *int_val);
                    *int_val += 1;
                    index_writer.add_document(doc).unwrap();
                };

            index_doc(
                &mut index_writer,
                &["/top/a/firstdoc", "/top/b"],
                &mut int_val,
            );
            index_doc(
                &mut index_writer,
                &["/top/a/firstdoc", "/top/b", "/top/c"],
                &mut int_val,
            );
            index_doc(&mut index_writer, &["/top/a", "/top/b"], &mut int_val);
            index_doc(&mut index_writer, &["/top/a"], &mut int_val);

            index_doc(&mut index_writer, &["/top/b", "/top/d"], &mut int_val);
            if force_segment_value_overlap {
                index_doc(&mut index_writer, &["/top/d"], &mut 0);
                index_doc(&mut index_writer, &["/top/e"], &mut 10);
                index_writer.commit().expect("committed");
                index_doc(&mut index_writer, &["/top/a"], &mut 5); // 5 is between 0 - 10 so the
                                                                   // segments don' have disjunct
                                                                   // ranges
            } else {
                index_doc(&mut index_writer, &["/top/d"], &mut int_val);
                index_doc(&mut index_writer, &["/top/e"], &mut int_val);
                index_writer.commit().expect("committed");
                index_doc(&mut index_writer, &["/top/a"], &mut int_val);
            }
            index_doc(&mut index_writer, &["/top/b"], &mut int_val);
            index_doc(&mut index_writer, &["/top/c"], &mut int_val);
            index_writer.commit().expect("committed");

            index_doc(&mut index_writer, &["/top/e", "/top/f"], &mut int_val);
            index_writer.commit().expect("committed");
        }

        reader.reload().unwrap();
        let test_searcher = |expected_num_docs: usize, expected: &[(&str, u64)]| {
            let searcher = reader.searcher();
            let mut facet_collector = FacetCollector::for_field("facet");
            facet_collector.add_facet(Facet::from("/top"));
            let (count, facet_counts) = searcher
                .search(&AllQuery, &(Count, facet_collector))
                .unwrap();
            assert_eq!(count, expected_num_docs);
            let facets: Vec<(String, u64)> = facet_counts
                .get("/top")
                .map(|(facet, count)| (facet.to_string(), count))
                .collect();
            assert_eq!(
                facets,
                expected
                    .iter()
                    .map(|&(facet_str, count)| (String::from(facet_str), count))
                    .collect::<Vec<_>>()
            );
        };
        test_searcher(
            11,
            &[
                ("/top/a", 5),
                ("/top/b", 5),
                ("/top/c", 2),
                ("/top/d", 2),
                ("/top/e", 2),
                ("/top/f", 1),
            ],
        );
        // Merging the segments
        {
            let segment_ids = index
                .searchable_segment_ids()
                .expect("Searchable segments failed.");
            let mut index_writer: IndexWriter = index.writer_for_tests().unwrap();
            index_writer
                .merge(&segment_ids)
                .expect("Merging failed");
            index_writer.wait_merging_threads().unwrap();
            reader.reload().unwrap();
            test_searcher(
                11,
                &[
                    ("/top/a", 5),
                    ("/top/b", 5),
                    ("/top/c", 2),
                    ("/top/d", 2),
                    ("/top/e", 2),
                    ("/top/f", 1),
                ],
            );
        }

        // Deleting one term
        {
            let mut index_writer: IndexWriter = index.writer_for_tests().unwrap();
            let facet = Facet::from_path(vec!["top", "a", "firstdoc"]);
            let facet_term = Term::from_facet(facet_field, &facet);
            index_writer.delete_term(facet_term);
            index_writer.commit().unwrap();
            reader.reload().unwrap();
            test_searcher(
                9,
                &[
                    ("/top/a", 3),
                    ("/top/b", 3),
                    ("/top/c", 1),
                    ("/top/d", 2),
                    ("/top/e", 2),
                    ("/top/f", 1),
                ],
            );
        }
    }

    #[test]
    fn test_bug_merge() -> crate::Result<()> {
        let mut schema_builder = schema::Schema::builder();
        let int_field = schema_builder.add_u64_field("intvals", INDEXED);
        let index = Index::create_in_ram(schema_builder.build());
        let mut index_writer: IndexWriter = index.writer_for_tests().unwrap();
        index_writer.add_document(doc!(int_field => 1u64))?;
        index_writer.commit().expect("commit failed");
        index_writer.add_document(doc!(int_field => 1u64))?;
        index_writer.commit().expect("commit failed");
        let reader = index.reader()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 2);
        index_writer.delete_term(Term::from_field_u64(int_field, 1));
        let segment_ids = index
            .searchable_segment_ids()
            .expect("Searchable segments failed.");
        index_writer.merge(&segment_ids)?;
        reader.reload()?;
        // commit has not been called yet. The document should still be
        // there.
        assert_eq!(reader.searcher().num_docs(), 2);
        Ok(())
    }

    #[test]
    fn test_merge_multivalued_int_fields_all_deleted() -> crate::Result<()> {
        let mut schema_builder = schema::Schema::builder();
        let int_options = NumericOptions::default().set_fast().set_indexed();
        let int_field = schema_builder.add_u64_field("intvals", int_options);
        let index = Index::create_in_ram(schema_builder.build());
        let reader = index.reader()?;
        {
            let mut index_writer = index.writer_for_tests()?;
            let mut doc = LucivyDocument::default();
            doc.add_u64(int_field, 1);
            index_writer.add_document(doc.clone())?;
            index_writer.commit()?;
            index_writer.add_document(doc)?;
            index_writer.commit()?;
            index_writer.delete_term(Term::from_field_u64(int_field, 1));
            let segment_ids = index.searchable_segment_ids()?;
            index_writer.merge(&segment_ids)?;

            // assert delete has not been committed
            reader.reload()?;
            let searcher = reader.searcher();
            assert_eq!(searcher.num_docs(), 2);

            index_writer.commit()?;

            index_writer.wait_merging_threads()?;
        }

        reader.reload()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 0);
        Ok(())
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    enum IndexingOp {
        ZeroVal,
        OneVal { val: u64 },
        TwoVal { val: u64 },
        Commit,
    }

    fn balanced_operation_strategy() -> impl Strategy<Value = IndexingOp> {
        prop_oneof![
            (0u64..1u64).prop_map(|_| IndexingOp::ZeroVal),
            (0u64..1u64).prop_map(|val| IndexingOp::OneVal { val }),
            (0u64..1u64).prop_map(|val| IndexingOp::TwoVal { val }),
            (0u64..1u64).prop_map(|_| IndexingOp::Commit),
        ]
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn test_merge_columnar_int_proptest(ops in proptest::collection::vec(balanced_operation_strategy(), 1..20)) {
            assert!(test_merge_int_fields(&ops[..]).is_ok());
        }
    }
    fn test_merge_int_fields(ops: &[IndexingOp]) -> crate::Result<()> {
        if ops.iter().all(|op| *op == IndexingOp::Commit) {
            return Ok(());
        }
        let expected_doc_and_vals: Vec<(u32, Vec<u64>)> = ops
            .iter()
            .filter(|op| *op != &IndexingOp::Commit)
            .map(|op| match op {
                IndexingOp::ZeroVal => vec![],
                IndexingOp::OneVal { val } => vec![*val],
                IndexingOp::TwoVal { val } => vec![*val, *val],
                IndexingOp::Commit => unreachable!(),
            })
            .enumerate()
            .map(|(id, val)| (id as u32, val))
            .collect();

        let mut schema_builder = schema::Schema::builder();
        let int_options = NumericOptions::default().set_fast().set_indexed();
        let int_field = schema_builder.add_u64_field("intvals", int_options);
        let index = Index::create_in_ram(schema_builder.build());
        {
            let mut index_writer = index.writer_for_tests()?;
            index_writer.set_merge_policy(Box::new(NoMergePolicy));
            let index_doc = |index_writer: &mut IndexWriter, int_vals: &[u64]| {
                let mut doc = LucivyDocument::default();
                for &val in int_vals {
                    doc.add_u64(int_field, val);
                }
                index_writer.add_document(doc).unwrap();
            };

            for op in ops {
                match op {
                    IndexingOp::ZeroVal => index_doc(&mut index_writer, &[]),
                    IndexingOp::OneVal { val } => index_doc(&mut index_writer, &[*val]),
                    IndexingOp::TwoVal { val } => index_doc(&mut index_writer, &[*val, *val]),
                    IndexingOp::Commit => {
                        index_writer.commit().expect("commit failed");
                    }
                }
            }
            index_writer.commit().expect("commit failed");
        }
        {
            let mut segment_ids = index.searchable_segment_ids()?;
            segment_ids.sort();
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.merge(&segment_ids)?;
            index_writer.wait_merging_threads()?;
        }
        let reader = index.reader()?;
        reader.reload()?;

        let mut vals: Vec<u64> = Vec::new();
        let mut test_vals = move |col: &Column<u64>, doc: DocId, expected: &[u64]| {
            vals.clear();
            vals.extend(col.values_for_doc(doc));
            assert_eq!(&vals[..], expected);
        };

        let mut test_col = move |col: &Column<u64>, column_expected: &[(u32, Vec<u64>)]| {
            for (doc_id, vals) in column_expected.iter() {
                test_vals(col, *doc_id, vals);
            }
        };

        {
            let searcher = reader.searcher();
            let segment = searcher.segment_reader(0u32);
            let col = segment
                .fast_fields()
                .column_opt::<u64>("intvals")
                .unwrap()
                .unwrap();

            test_col(&col, &expected_doc_and_vals);
        }

        Ok(())
    }

    #[test]
    fn test_merge_multivalued_int_fields_simple() -> crate::Result<()> {
        let mut schema_builder = schema::Schema::builder();
        let int_options = NumericOptions::default().set_fast().set_indexed();
        let int_field = schema_builder.add_u64_field("intvals", int_options);
        let index = Index::create_in_ram(schema_builder.build());

        let mut vals: Vec<u64> = Vec::new();
        let mut test_vals = move |col: &Column<u64>, doc: DocId, expected: &[u64]| {
            vals.clear();
            vals.extend(col.values_for_doc(doc));
            assert_eq!(&vals[..], expected);
        };

        {
            let mut index_writer = index.writer_for_tests()?;
            let index_doc = |index_writer: &mut IndexWriter, int_vals: &[u64]| {
                let mut doc = LucivyDocument::default();
                for &val in int_vals {
                    doc.add_u64(int_field, val);
                }
                index_writer.add_document(doc).unwrap();
            };
            index_doc(&mut index_writer, &[1, 2]);
            index_doc(&mut index_writer, &[1, 2, 3]);
            index_doc(&mut index_writer, &[4, 5]);
            index_doc(&mut index_writer, &[1, 2]);
            index_doc(&mut index_writer, &[1, 5]);
            index_doc(&mut index_writer, &[3]);
            index_doc(&mut index_writer, &[17]);
            assert!(index_writer.commit().is_ok());
            index_doc(&mut index_writer, &[20]);
            assert!(index_writer.commit().is_ok());
            index_doc(&mut index_writer, &[28, 27]);
            index_doc(&mut index_writer, &[1_000]);
            assert!(index_writer.commit().is_ok());
        }
        let reader = index.reader()?;
        let searcher = reader.searcher();

        {
            let segment = searcher.segment_reader(0u32);
            let column = segment
                .fast_fields()
                .column_opt::<u64>("intvals")
                .unwrap()
                .unwrap();
            test_vals(&column, 0, &[1, 2]);
            test_vals(&column, 1, &[1, 2, 3]);
            test_vals(&column, 2, &[4, 5]);
            test_vals(&column, 3, &[1, 2]);
            test_vals(&column, 4, &[1, 5]);
            test_vals(&column, 5, &[3]);
            test_vals(&column, 6, &[17]);
        }

        {
            let segment = searcher.segment_reader(1u32);
            let col = segment
                .fast_fields()
                .column_opt::<u64>("intvals")
                .unwrap()
                .unwrap();
            test_vals(&col, 0, &[28, 27]);
            test_vals(&col, 1, &[1000]);
        }

        {
            let segment = searcher.segment_reader(2u32);
            let col = segment
                .fast_fields()
                .column_opt::<u64>("intvals")
                .unwrap()
                .unwrap();
            test_vals(&col, 0, &[20]);
        }

        // Merging the segments
        {
            let segment_ids = index.searchable_segment_ids()?;
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.merge(&segment_ids)?;
            index_writer.wait_merging_threads()?;
        }
        reader.reload()?;

        {
            let searcher = reader.searcher();
            let segment = searcher.segment_reader(0u32);
            let col = segment
                .fast_fields()
                .column_opt::<u64>("intvals")
                .unwrap()
                .unwrap();
            test_vals(&col, 0, &[1, 2]);
            test_vals(&col, 1, &[1, 2, 3]);
            test_vals(&col, 2, &[4, 5]);
            test_vals(&col, 3, &[1, 2]);
            test_vals(&col, 4, &[1, 5]);
            test_vals(&col, 5, &[3]);
            test_vals(&col, 6, &[17]);
            test_vals(&col, 7, &[28, 27]);
            test_vals(&col, 8, &[1000]);
            test_vals(&col, 9, &[20]);
        }
        Ok(())
    }

    #[test]
    fn merges_f64_fast_fields_correctly() -> crate::Result<()> {
        let mut builder = schema::SchemaBuilder::new();

        let fast_multi = NumericOptions::default().set_fast();

        let field = builder.add_f64_field("f64", schema::FAST);
        let multi_field = builder.add_f64_field("f64s", fast_multi);

        let index = Index::create_in_ram(builder.build());

        let mut writer = index.writer_for_tests()?;

        // Make sure we'll attempt to merge every created segment
        let mut policy = crate::indexer::LogMergePolicy::default();
        policy.set_min_num_segments(2);
        writer.set_merge_policy(Box::new(policy));

        for i in 0..100 {
            let mut doc = LucivyDocument::new();
            doc.add_f64(field, 42.0);
            doc.add_f64(multi_field, 0.24);
            doc.add_f64(multi_field, 0.27);
            writer.add_document(doc)?;
            if i % 5 == 0 {
                writer.commit()?;
            }
        }

        writer.commit()?;
        writer.wait_merging_threads()?;

        // If a merging thread fails, we should end up with more
        // than one segment here
        assert_eq!(1, index.searchable_segments()?.len());
        Ok(())
    }

    #[test]
    fn test_merged_index_has_blockwand() -> crate::Result<()> {
        let mut builder = schema::SchemaBuilder::new();
        let text = builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(builder.build());
        let mut writer = index.writer_for_tests()?;
        let happy_term = Term::from_field_text(text, "happy");
        let term_query = TermQuery::new(happy_term, IndexRecordOption::WithFreqs);
        for _ in 0..62 {
            writer.add_document(doc!(text=>"hello happy tax payer"))?;
        }
        writer.commit()?;
        let reader = index.reader()?;
        let searcher = reader.searcher();
        let mut term_scorer = term_query
            .specialized_weight(EnableScoring::enabled_from_searcher(&searcher))?
            .term_scorer_for_test(searcher.segment_reader(0u32), 1.0)?
            .unwrap();
        assert_eq!(term_scorer.doc(), 0);
        assert_nearly_equals!(term_scorer.block_max_score(), 0.0079681855);
        assert_nearly_equals!(term_scorer.score(), 0.0079681855);
        for _ in 0..81 {
            writer.add_document(doc!(text=>"hello happy tax payer"))?;
        }
        writer.commit()?;
        reader.reload()?;
        let searcher = reader.searcher();

        assert_eq!(searcher.segment_readers().len(), 2);
        for segment_reader in searcher.segment_readers() {
            let mut term_scorer = term_query
                .specialized_weight(EnableScoring::enabled_from_searcher(&searcher))?
                .term_scorer_for_test(segment_reader, 1.0)?
                .unwrap();
            // the difference compared to before is intrinsic to the bm25 formula. no worries
            // there.
            for doc in segment_reader.doc_ids_alive() {
                assert_eq!(term_scorer.doc(), doc);
                assert_nearly_equals!(term_scorer.block_max_score(), 0.003478312);
                assert_nearly_equals!(term_scorer.score(), 0.003478312);
                term_scorer.advance();
            }
        }

        let segment_ids: Vec<SegmentId> = searcher
            .segment_readers()
            .iter()
            .map(|reader| reader.segment_id())
            .collect();
        writer.merge(&segment_ids[..])?;

        reader.reload()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1);

        let segment_reader = searcher.segment_reader(0u32);
        let mut term_scorer = term_query
            .specialized_weight(EnableScoring::enabled_from_searcher(&searcher))?
            .term_scorer_for_test(segment_reader, 1.0)?
            .unwrap();
        // the difference compared to before is intrinsic to the bm25 formula. no worries there.
        for doc in segment_reader.doc_ids_alive() {
            assert_eq!(term_scorer.doc(), doc);
            assert_nearly_equals!(term_scorer.block_max_score(), 0.003478312);
            assert_nearly_equals!(term_scorer.score(), 0.003478312);
            term_scorer.advance();
        }

        Ok(())
    }

    #[test]
    fn test_max_doc() {
        // this is the first time I write a unit test for a constant.
        assert!(((super::MAX_DOC_LIMIT - 1) as i32) >= 0);
        assert!((super::MAX_DOC_LIMIT as i32) < 0);
    }

    #[test]
    fn test_merge_preserves_offsets() -> crate::Result<()> {
        use crate::postings::Postings;
        use crate::schema::TextOptions;

        // Create a field with offsets enabled.
        let mut schema_builder = schema::Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_index_option(IndexRecordOption::WithFreqsAndPositionsAndOffsets);
        let text_opts = TextOptions::default()
            .set_indexing_options(indexing)
            .set_stored();
        let text_field = schema_builder.add_text_field("text", text_opts);
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);

        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            index_writer.set_merge_policy(Box::new(NoMergePolicy));

            // Segment 1
            index_writer.add_document(doc!(text_field => "hello world foo"))?;
            index_writer.commit()?;

            // Segment 2
            index_writer.add_document(doc!(text_field => "hello bar baz"))?;
            index_writer.commit()?;
        }

        // Force merge into a single segment.
        {
            let mut index_writer: IndexWriter = index.writer_for_tests()?;
            let segment_ids: Vec<_> = index
                .searchable_segment_ids()
                .unwrap()
                .into_iter()
                .collect();
            assert!(segment_ids.len() >= 2, "expected multiple segments before merge");
            index_writer.merge(&segment_ids)?;
            index_writer.wait_merging_threads()?;
        }

        // After merge, read postings with offsets for a term that exists in both docs.
        let reader = index.reader()?;
        let searcher = reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1, "expected single merged segment");

        let seg_reader = searcher.segment_reader(0);
        let inverted_index = seg_reader.inverted_index(text_field)?;
        let term = Term::from_field_text(text_field, "hello");
        let term_info = inverted_index.get_term_info(&term)?.expect("term not found");

        // This is the line that panicked before the fix: reading offsets from merged segment.
        let mut postings = inverted_index.read_postings_from_terminfo(
            &term_info,
            IndexRecordOption::WithFreqsAndPositionsAndOffsets,
        )?;

        // Doc 0: "hello world foo" → "hello" at byte offsets [0, 5)
        assert_ne!(postings.doc(), crate::TERMINATED);
        let mut offsets = Vec::new();
        postings.offsets(&mut offsets);
        assert_eq!(offsets.len(), 1, "expected 1 occurrence in doc 0");
        assert_eq!(offsets[0], (0, 5), "expected 'hello' at bytes 0..5");

        // Doc 1: "hello bar baz" → "hello" at byte offsets [0, 5)
        postings.advance();
        assert_ne!(postings.doc(), crate::TERMINATED);
        offsets.clear();
        postings.offsets(&mut offsets);
        assert_eq!(offsets.len(), 1, "expected 1 occurrence in doc 1");
        assert_eq!(offsets[0], (0, 5), "expected 'hello' at bytes 0..5");

        Ok(())
    }
}
