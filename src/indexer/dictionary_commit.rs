//! The shard dictionary's generations, written at commit.
//!
//! A commit on a `sfx_version` 4 index folds the `.newtexts` of the segments
//! it commits — the texts of the ids they minted — into a new generation
//! holding those texts only (one `.sfx` and one `.termtexts` per field),
//! then makes it live on the `Index` so that `save_metas` names it. Each
//! segment also wrote the FST over its own new texts (`.newsfx`, built on
//! its build thread), so the fold is a stream merge of the segments' pairs
//! (`compact_parts`), not an FST build: on 30 000 kernel files the build
//! was 8.8 s of the commit path, serial with the document stream. Past
//! `LUCIVY_DICT_MAX_GENERATIONS` live generations, the smallest ones are
//! merged into one, in streams (`suffix_fst::dictionary_compact`) —
//! nothing of the dictionary is ever held in RAM, and the largest
//! generation only joins once enough others have outgrown it.
//! Nothing new to fold → nothing written.
//!
//! Ids are stable and append-only, so a merge changes no segment.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::directory::Directory;
use crate::index::SfxDictionaryMeta;
use crate::suffix_fst::dictionary::{decode_newtexts, DICTIONARY_SFX_VERSION, SfxDictionary};
use crate::suffix_fst::dictionary_compact::{choose_compaction, compact_generations, compact_parts, generation_bytes, remove_leftovers, write_generation};
use crate::suffix_fst::termtexts_v3::{TermMetaV3, TermTextsReaderV3};
use common::OwnedBytes;

use super::segment_updater::SegmentUpdaterShared;

/// Fold the not-yet-committed segments' `.newtexts` into a new generation
/// and make it the `Index`'s live dictionary. No-op on other indexes.
pub(crate) fn fold_new_texts(shared: &Arc<SegmentUpdaterShared>) -> crate::Result<()> {
    let index = &shared.index;
    if index.settings().sfx_version != DICTIONARY_SFX_VERSION {
        return Ok(());
    }
    let Some(dictionary) = index.sfx_dictionary() else {
        return Err(crate::LucivyError::SystemError(
            "sfx_version 4 index without a shard dictionary".to_string()));
    };
    let verbose = crate::diag::is_verbose();
    let t_fold = std::time::Instant::now();
    let committed: HashSet<_> = shared.segment_manager.committed_segment_metas()
        .iter().map(|m| m.id()).collect();

    // field → the `.newtexts` (and `.newsfx`) of the segments being
    // committed: generation-shaped pairs over the ids they minted. A segment
    // committed earlier had its files folded then: they are dead weight (a
    // copy of the segment's texts), delete them.
    let mut parts_by_field: HashMap<u32, Vec<(OwnedBytes, Option<OwnedBytes>)>> = HashMap::new();
    let mut folded_ids: HashSet<(u32, u64)> = HashSet::new();
    let mut new_texts: usize = 0;
    for entry in shared.segment_manager.segment_entries() {
        let meta = entry.meta();
        let segment = index.segment(meta.clone());
        for &field_id in meta.sfx_field_ids() {
            if committed.contains(&meta.id()) {
                for ext in ["newtexts", "newsfx"] {
                    let path = PathBuf::from(format!("{}.{field_id}.{ext}", meta.id().uuid_string()));
                    match index.directory().delete(&path) {
                        Ok(()) | Err(crate::directory::error::DeleteError::FileDoesNotExist(_)) => {}
                        Err(e) => return Err(crate::LucivyError::SystemError(
                            format!("cannot delete consumed {}: {e}", path.display()))),
                    }
                }
                continue;
            }
            let Ok(slice) = segment.open_read_custom(&format!("{field_id}.newtexts")) else { continue };
            let texts = slice.read_bytes()?;
            let Some(reader) = TermTextsReaderV3::open(&texts) else { continue };
            let before = folded_ids.len();
            folded_ids.extend(reader.iter().map(|(g, _, _)| (field_id, g as u64)));
            if folded_ids.len() == before { continue; }
            new_texts += folded_ids.len() - before;
            let sfx = match segment.open_read_custom(&format!("{field_id}.newsfx")) {
                Ok(s) => Some(s.read_bytes()?),
                Err(_) => None,
            };
            parts_by_field.entry(field_id).or_default().push((texts, sfx));
        }
    }
    if parts_by_field.is_empty() {
        if verbose {
            eprintln!("[dictionary] commit: nothing new to fold; {}", crate::suffix_fst::dictionary::stats::take());
        }
        return Ok(());
    }
    let read_ms = t_fold.elapsed().as_secs_f64() * 1e3;
    let mut field_ids: Vec<u32> = dictionary.meta().field_ids.clone();
    for &f in parts_by_field.keys() {
        if !field_ids.contains(&f) {
            field_ids.push(f);
        }
    }
    field_ids.sort_unstable();

    let max_generations: usize = std::env::var("LUCIVY_DICT_MAX_GENERATIONS").ok()
        .and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(8);
    let directory = index.directory();
    let mut generations = dictionary.meta().generations.clone();
    let mut next_generation = dictionary.meta().next_generation.max(1);

    // The new generation: the new texts only, per field, ids ascending.
    let generation = next_generation;
    next_generation += 1;
    remove_leftovers(directory, generation, &field_ids)?;
    // The new generation: the segments' pairs stream-merged when every one
    // has its `.newsfx`; else (a segment written before `.newsfx` existed)
    // the FST is built here over the decoded texts. A field with nothing new
    // gets an empty generation, as before.
    let t_write = std::time::Instant::now();
    for &field_id in &field_ids {
        let parts = parts_by_field.remove(&field_id).unwrap_or_default();
        if !parts.is_empty() && parts.iter().all(|(_, sfx)| sfx.is_some()) {
            let (termtexts, sfx): (Vec<OwnedBytes>, Vec<OwnedBytes>) =
                parts.into_iter().map(|(t, s)| (t, s.unwrap())).unzip();
            compact_parts(directory, sfx, termtexts, field_id, generation)?;
            continue;
        }
        let mut entries: Vec<(u32, String, TermMetaV3)> = Vec::new();
        for (texts, _) in &parts {
            if let Some(decoded) = decode_newtexts(texts) {
                entries.extend(decoded);
            }
        }
        entries.sort_by_key(|e| e.0);
        entries.dedup_by_key(|e| e.0);
        write_generation(directory, generation, field_id, &entries)?;
    }
    let write_ms = t_write.elapsed().as_secs_f64() * 1e3;
    generations.push(generation);

    // Too many generations to walk: the smallest ones merge into one.
    let sizes: Vec<(u64, u64)> = generations.iter()
        .map(|&g| (g, generation_bytes(directory, g, &field_ids))).collect();
    let t_compact = std::time::Instant::now();
    let mut compacted: Option<usize> = None;
    if let Some(merged) = choose_compaction(&sizes, max_generations) {
        compacted = Some(merged.len());
        let compact = next_generation;
        next_generation += 1;
        remove_leftovers(directory, compact, &field_ids)?;
        for &field_id in &field_ids {
            compact_generations(directory, &merged, field_id, compact)?;
        }
        generations.retain(|g| !merged.contains(g));
        generations.push(compact);
    }

    let compact_ms = t_compact.elapsed().as_secs_f64() * 1e3;

    let folded = folded_ids;
    let live = generations.len();
    let meta = SfxDictionaryMeta { generations, next_generation, next_ids: dictionary.next_ids(), field_ids };
    let t_open = std::time::Instant::now();
    let next = SfxDictionary::open(directory, &meta, Some(&dictionary));
    next.forget_pending(&folded);
    index.set_sfx_dictionary(Some(Arc::new(next)));
    if verbose {
        let compaction = match compacted {
            Some(n) => format!("compaction of {n} generations {compact_ms:.0} ms"),
            None => format!("no compaction ({compact_ms:.0} ms sizing)"),
        };
        eprintln!("[dictionary] commit: generation {generation} with {new_texts} new texts: newtexts read {read_ms:.0} ms, written {write_ms:.0} ms, {compaction}, reopened {:.0} ms ({live} live); since last commit: {}",
            t_open.elapsed().as_secs_f64() * 1e3, crate::suffix_fst::dictionary::stats::take());
    }
    Ok(())
}
