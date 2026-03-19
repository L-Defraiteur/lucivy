//! Standalone functions for suffix FST merge — each step is independent
//! and can be wired as a DAG node for observability.
//!
//! These functions are extracted from `IndexMerger::merge_sfx` so that
//! each step can be individually timed, tapped, and debugged.

use std::collections::{BTreeSet, HashMap};

use crate::docset::{DocSet, TERMINATED};
use crate::index::SegmentReader;
use crate::postings::Postings;
use crate::schema::{Field, IndexRecordOption};
use crate::suffix_fst::builder::SuffixFstBuilder;
use crate::suffix_fst::encode_vint;
use crate::suffix_fst::file::{SfxFileReader, SfxFileWriter, SfxPostingsReader};
use crate::suffix_fst::gapmap::{GapMapReader, GapMapWriter, GapMapError};
use crate::DocAddress;

// ---------------------------------------------------------------------------
// Reverse doc map (shared by multiple steps)
// ---------------------------------------------------------------------------

/// Build reverse doc mapping: for each segment, (old_doc_id → new_doc_id).
pub(crate) fn build_reverse_doc_map(
    doc_mapping: &[DocAddress],
    num_segments: usize,
) -> Vec<HashMap<u32, u32>> {
    let mut reverse: Vec<HashMap<u32, u32>> = vec![HashMap::new(); num_segments];
    for (new_doc, old_addr) in doc_mapping.iter().enumerate() {
        reverse[old_addr.segment_ord as usize]
            .insert(old_addr.doc_id, new_doc as u32);
    }
    reverse
}

// ---------------------------------------------------------------------------
// Step 0: Load sfx data from source segments
// ---------------------------------------------------------------------------

/// Load .sfx bytes from each source segment for a given field.
/// Returns (sfx_bytes_per_segment, any_has_sfx).
pub(crate) fn load_sfx_data(
    readers: &[SegmentReader],
    field: Field,
) -> (Vec<Option<Vec<u8>>>, bool) {
    let mut segment_sfx = Vec::with_capacity(readers.len());
    let mut any_has_sfx = false;

    for reader in readers {
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

    (segment_sfx, any_has_sfx)
}

// ---------------------------------------------------------------------------
// Step 1: Collect unique tokens
// ---------------------------------------------------------------------------

/// Collect all unique tokens from the source segments' term dictionaries.
/// If any segment has deletes, check that each term has at least one alive doc.
pub(crate) fn collect_tokens(
    readers: &[SegmentReader],
    field: Field,
    reverse_doc_map: &[HashMap<u32, u32>],
) -> crate::Result<BTreeSet<String>> {
    let has_deletes = readers.iter().any(|r| r.alive_bitset().is_some());
    let mut unique_tokens = BTreeSet::new();

    if has_deletes {
        for (seg_ord, reader) in readers.iter().enumerate() {
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
        for reader in readers {
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

    Ok(unique_tokens)
}

// ---------------------------------------------------------------------------
// Step 2: Build FST from collected tokens
// ---------------------------------------------------------------------------

/// Build suffix FST from sorted unique tokens.
/// Returns (fst_data, parent_list_data).
pub(crate) fn build_fst(
    tokens: &BTreeSet<String>,
) -> crate::Result<(Vec<u8>, Vec<u8>)> {
    let mut sfx_builder = SuffixFstBuilder::new();
    for (ordinal, token) in tokens.iter().enumerate() {
        sfx_builder.add_token(token, ordinal as u64);
    }
    sfx_builder.build().map_err(|e| {
        crate::LucivyError::SystemError(format!("sfx fst build: {e}"))
    })
}

// ---------------------------------------------------------------------------
// Step 3: Copy gapmap data per doc in merge order
// ---------------------------------------------------------------------------

/// Copy gapmap data from source segments in merge order.
/// Returns serialized gapmap bytes.
pub(crate) fn copy_gapmap(
    sfx_data: &[Option<Vec<u8>>],
    doc_mapping: &[DocAddress],
) -> Vec<u8> {
    let sfx_readers: Vec<Option<SfxFileReader<'_>>> = sfx_data
        .iter()
        .map(|opt| opt.as_ref().and_then(|bytes| SfxFileReader::open(bytes).ok()))
        .collect();

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

    gapmap_writer.serialize()
}

// ---------------------------------------------------------------------------
// Step 4: Merge sfxpost (posting entries with doc_id remapping)
// ---------------------------------------------------------------------------

/// Merge sfxpost data from source segments with doc_id remapping.
/// Returns serialized sfxpost bytes (or None if no source had sfxpost).
pub(crate) fn merge_sfxpost(
    readers: &[SegmentReader],
    field: Field,
    tokens: &BTreeSet<String>,
    reverse_doc_map: &[HashMap<u32, u32>],
) -> crate::Result<Option<Vec<u8>>> {
    let mut segment_sfxpost: Vec<Option<Vec<u8>>> = Vec::with_capacity(readers.len());
    let mut any_has_sfxpost = false;

    for reader in readers {
        if let Some(file_slice) = reader.sfxpost_file(field) {
            match file_slice.read_bytes() {
                Ok(bytes) => {
                    segment_sfxpost.push(Some(bytes.to_vec()));
                    any_has_sfxpost = true;
                }
                Err(_) => segment_sfxpost.push(None),
            }
        } else {
            segment_sfxpost.push(None);
        }
    }

    if !any_has_sfxpost {
        return Ok(None);
    }

    let sfxpost_readers: Vec<Option<SfxPostingsReader<'_>>> = segment_sfxpost
        .iter()
        .map(|opt| opt.as_ref().and_then(|b| SfxPostingsReader::open(b).ok()))
        .collect();

    // Build token → old ordinal maps
    let mut token_to_ordinal: Vec<HashMap<String, u32>> = Vec::with_capacity(readers.len());
    for reader in readers {
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

    // Merge entries per token
    let mut posting_offsets: Vec<u32> = Vec::with_capacity(tokens.len() + 1);
    let mut posting_bytes: Vec<u8> = Vec::new();

    for token in tokens {
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
            // No sfxpost for this segment — its docs will be MISSING from search.
            // This should not happen if all segments were properly merged via commit().
            if token_to_ordinal[seg_ord].contains_key(token.as_str()) {
                let ndocs = readers[seg_ord].num_docs();
                eprintln!("[merge_sfxpost] ERROR: segment {} ({} docs) has term {:?} but NO sfxpost file — docs will be missing from contains search!",
                    readers[seg_ord].segment_id().uuid_string()[..8].to_string(), ndocs, token);
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
    data.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
    for &off in &posting_offsets {
        data.extend_from_slice(&off.to_le_bytes());
    }
    data.extend_from_slice(&posting_bytes);

    Ok(Some(data))
}

// ---------------------------------------------------------------------------
// Step 5: Validate gapmap
// ---------------------------------------------------------------------------

/// Validate gapmap data. Returns errors (empty = valid).
pub(crate) fn validate_gapmap(gapmap_data: &[u8]) -> Vec<GapMapError> {
    let reader = GapMapReader::open(gapmap_data);
    reader.validate()
}

// ---------------------------------------------------------------------------
// Step 6: Assemble and write .sfx + .sfxpost
// ---------------------------------------------------------------------------

/// Assemble SFX file from components and write via serializer.
pub(crate) fn write_sfx(
    serializer: &mut crate::indexer::SegmentSerializer,
    field: Field,
    fst_data: Vec<u8>,
    parent_list_data: Vec<u8>,
    gapmap_data: Vec<u8>,
    num_docs: u32,
    num_tokens: u32,
    sfxpost_data: Option<Vec<u8>>,
) -> crate::Result<()> {
    let sfx_file = SfxFileWriter::new(
        fst_data,
        parent_list_data,
        gapmap_data,
        num_docs,
        num_tokens,
    );
    let sfx_bytes = sfx_file.to_bytes();
    serializer.write_sfx(field.field_id(), &sfx_bytes)?;
    if let Some(ref sfxpost) = sfxpost_data {
        serializer.write_sfxpost(field.field_id(), sfxpost)?;
    }
    Ok(())
}
