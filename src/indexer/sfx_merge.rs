//! Standalone functions for suffix FST merge — each step is independent
//! and can be wired as a DAG node for observability.
//!
//! These functions are extracted from `IndexMerger::merge_sfx` so that
//! each step can be individually timed, tapped, and debugged.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};

use crate::docset::{DocSet, TERMINATED};
use crate::index::SegmentReader;
use crate::schema::{Field, IndexRecordOption};

// ---------------------------------------------------------------------------
// Validation flag — enabled by default, disable via LUCIVY_SKIP_VALIDATION=1
// ---------------------------------------------------------------------------

static VALIDATION_FLAG: AtomicU8 = AtomicU8::new(0); // 0=unset, 1=enabled, 2=disabled

fn should_validate() -> bool {
    let flag = VALIDATION_FLAG.load(Ordering::Relaxed);
    if flag != 0 {
        return flag == 1;
    }
    let enabled = std::env::var("LUCIVY_SKIP_VALIDATION")
        .map(|v| v != "1")
        .unwrap_or(true);
    VALIDATION_FLAG.store(if enabled { 1 } else { 2 }, Ordering::Relaxed);
    enabled
}
use crate::suffix_fst::builder::SuffixFstBuilder;
use crate::suffix_fst::encode_vint;
use crate::suffix_fst::file::{SfxFileReader, SfxFileWriter};
use crate::suffix_fst::sfxpost_v2::SfxPostReaderV2;
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

    let sfxpost_readers: Vec<Option<SfxPostReaderV2>> = segment_sfxpost
        .iter()
        .map(|opt| opt.as_ref().and_then(|b| SfxPostReaderV2::open_slice(b)))
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

    // Merge entries per token → V2 format
    let mut sfxpost_writer = crate::suffix_fst::sfxpost_v2::SfxPostWriterV2::new(tokens.len());

    for (new_ord, token) in tokens.iter().enumerate() {
        for (seg_ord, sfxpost_reader) in sfxpost_readers.iter().enumerate() {
            if let Some(reader) = sfxpost_reader {
                if let Some(&old_ord) = token_to_ordinal[seg_ord].get(token.as_str()) {
                    for e in reader.entries(old_ord) {
                        if let Some(&new_doc) = reverse_doc_map[seg_ord].get(&e.doc_id) {
                            sfxpost_writer.add_entry(new_ord as u32, new_doc, e.token_index, e.byte_from, e.byte_to);
                        }
                    }
                }
            } else if token_to_ordinal[seg_ord].contains_key(token.as_str()) {
                let seg_id = readers[seg_ord].segment_id().uuid_string();
                let ndocs = readers[seg_ord].num_docs();
                return Err(crate::LucivyError::SystemError(format!(
                    "merge_sfxpost: segment {} ({} docs) has term {:?} but NO sfxpost — \
                     every segment must have sfxpost",
                    &seg_id[..8], ndocs, token,
                )));
            }
        }
    }

    Ok(Some(sfxpost_writer.finish()))
}

// ---------------------------------------------------------------------------
// Step 4b: Merge sibling links from source segments
// ---------------------------------------------------------------------------

/// Merge sibling links from source segments with ordinal remapping.
/// Returns serialized sibling table bytes.
pub(crate) fn merge_sibling_links(
    sfx_data: &[Option<Vec<u8>>],
    readers: &[SegmentReader],
    field: Field,
    tokens: &BTreeSet<String>,
) -> crate::Result<Vec<u8>> {
    use crate::suffix_fst::sibling_table::SiblingTableWriter;

    let num_tokens = tokens.len() as u32;
    let mut writer = SiblingTableWriter::new(num_tokens);

    // Build old_ordinal → token text maps per segment (for reverse lookup)
    // and token text → new_ordinal map (from merged BTreeSet)
    let token_to_new: HashMap<&str, u32> = tokens.iter().enumerate()
        .map(|(i, t)| (t.as_str(), i as u32))
        .collect();

    for (seg_ord, reader) in readers.iter().enumerate() {
        // Read old sibling table from the segment's .sfx
        let sfx_bytes = match &sfx_data[seg_ord] {
            Some(b) => b,
            None => continue,
        };
        let sfx_reader = match SfxFileReader::open(sfx_bytes) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let sibling_table = match sfx_reader.sibling_table() {
            Some(t) => t,
            None => continue,
        };

        // Build old_ordinal → token text for this segment
        let mut old_ord_to_text: Vec<String> = Vec::new();
        if let Ok(inv_idx) = reader.inverted_index(field) {
            let term_dict = inv_idx.terms();
            let mut stream = term_dict.stream()?;
            while stream.advance() {
                if let Ok(s) = std::str::from_utf8(stream.key()) {
                    old_ord_to_text.push(s.to_string());
                }
            }
        }

        // Remap sibling links
        for old_ord in 0..sibling_table.num_ordinals() {
            let old_text = match old_ord_to_text.get(old_ord as usize) {
                Some(t) => t.as_str(),
                None => continue,
            };
            let new_ord = match token_to_new.get(old_text) {
                Some(&n) => n,
                None => continue, // token was deleted
            };

            for entry in sibling_table.siblings(old_ord) {
                let next_old_text = match old_ord_to_text.get(entry.next_ordinal as usize) {
                    Some(t) => t.as_str(),
                    None => continue,
                };
                let next_new_ord = match token_to_new.get(next_old_text) {
                    Some(&n) => n,
                    None => continue, // next token was deleted
                };
                writer.add(new_ord, next_new_ord, entry.gap_len);
            }
        }
    }

    Ok(writer.serialize())
}

// ---------------------------------------------------------------------------
// Step 5: Validate gapmap
// ---------------------------------------------------------------------------

/// Validate gapmap data. Returns errors (empty = valid).
pub(crate) fn validate_gapmap(gapmap_data: &[u8]) -> Vec<GapMapError> {
    if !should_validate() { return vec![]; }
    let reader = GapMapReader::open(gapmap_data);
    reader.validate()
}

/// Validate sfxpost data against the term dict.
/// Checks:
/// - All doc_ids < num_docs
/// - Number of ordinals matches num_tokens
/// Returns error description or None if valid.
pub(crate) fn validate_sfxpost(
    sfxpost_data: &[u8],
    num_docs: u32,
    num_tokens: u32,
) -> Option<String> {
    if !should_validate() { return None; }
    if sfxpost_data.len() < 8 { return Some("sfxpost too short".into()); }

    // V2 format: "SFP2" magic + u32 num_terms + offset table + entry data
    if &sfxpost_data[0..4] != b"SFP2" {
        return Some("sfxpost: missing SFP2 magic".into());
    }

    let stored_num_tokens = u32::from_le_bytes(
        sfxpost_data[4..8].try_into().unwrap()
    );
    if stored_num_tokens != num_tokens {
        return Some(format!(
            "sfxpost num_tokens mismatch: stored={} expected={}",
            stored_num_tokens, num_tokens,
        ));
    }

    // Validate via V2 reader: try to open and read all entries
    let reader = match SfxPostReaderV2::open_slice(sfxpost_data) {
        Some(r) => r,
        None => return Some("sfxpost: cannot open V2 reader".into()),
    };

    for ord in 0..num_tokens {
        let entries = reader.entries(ord);
        for e in &entries {
            if e.doc_id >= num_docs {
                return Some(format!(
                    "sfxpost doc_id {} >= num_docs {} at ordinal {}",
                    e.doc_id, num_docs, ord,
                ));
            }
        }
    }

    None // valid
}

fn decode_vint(data: &[u8]) -> (u32, usize) {
    let mut result = 0u32;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        shift += 7;
        if shift >= 35 { break; } // overflow protection
    }
    (result, data.len())
}

// ---------------------------------------------------------------------------
// Step 6: Assemble and write .sfx + .sfxpost
// ---------------------------------------------------------------------------

/// Assemble SFX file from components and write via serializer.
/// Legacy — replaced by WriteSfxNode in sfx_dag.rs.
#[allow(dead_code)]
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
