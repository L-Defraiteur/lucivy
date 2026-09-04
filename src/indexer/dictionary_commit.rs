//! The shard dictionary's generations, written at commit.
//!
//! A commit on a `sfx_version` 4 index folds the `.newtexts` of the segments
//! it commits — the texts of the ids they minted — into a new generation
//! holding those texts only (one `.sfx` and one `.termtexts` per field),
//! then makes it live on the `Index` so that `save_metas` names it. Past
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
use crate::suffix_fst::dictionary_compact::{choose_compaction, compact_generations, generation_bytes, remove_leftovers, write_generation};
use crate::suffix_fst::termtexts_v3::TermMetaV3;

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
    let committed: HashSet<_> = shared.segment_manager.committed_segment_metas()
        .iter().map(|m| m.id()).collect();

    // field → (global id, text, meta) minted by the segments being committed.
    // A segment committed earlier had its `.newtexts` folded then: the file
    // is dead weight (a copy of the segment's texts), delete it.
    let mut new_by_field: HashMap<u32, Vec<(u32, String, TermMetaV3)>> = HashMap::new();
    for entry in shared.segment_manager.segment_entries() {
        let meta = entry.meta();
        let segment = index.segment(meta.clone());
        for &field_id in meta.sfx_field_ids() {
            let path = PathBuf::from(format!("{}.{field_id}.newtexts", meta.id().uuid_string()));
            if committed.contains(&meta.id()) {
                match index.directory().delete(&path) {
                    Ok(()) | Err(crate::directory::error::DeleteError::FileDoesNotExist(_)) => {}
                    Err(e) => return Err(crate::LucivyError::SystemError(
                        format!("cannot delete consumed {}: {e}", path.display()))),
                }
                continue;
            }
            let Ok(slice) = segment.open_read_custom(&format!("{field_id}.newtexts")) else { continue };
            let bytes = slice.read_bytes()?;
            if let Some(entries) = decode_newtexts(&bytes) {
                if !entries.is_empty() {
                    new_by_field.entry(field_id).or_default().extend(entries);
                }
            }
        }
    }
    if new_by_field.is_empty() {
        return Ok(());
    }
    let folded_ids: HashSet<(u32, u64)> = new_by_field.iter()
        .flat_map(|(f, v)| v.iter().map(move |(g, _, _)| (*f, *g as u64))).collect();

    let mut field_ids: Vec<u32> = dictionary.meta().field_ids.clone();
    for &f in new_by_field.keys() {
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
    for &field_id in &field_ids {
        let mut entries = new_by_field.remove(&field_id).unwrap_or_default();
        entries.sort_by_key(|e| e.0);
        entries.dedup_by_key(|e| e.0);
        write_generation(directory, generation, field_id, &entries)?;
    }
    generations.push(generation);

    // Too many generations to walk: the smallest ones merge into one.
    let sizes: Vec<(u64, u64)> = generations.iter()
        .map(|&g| (g, generation_bytes(directory, g, &field_ids))).collect();
    if let Some(merged) = choose_compaction(&sizes, max_generations) {
        let compact = next_generation;
        next_generation += 1;
        remove_leftovers(directory, compact, &field_ids)?;
        for &field_id in &field_ids {
            compact_generations(directory, &merged, field_id, compact)?;
        }
        generations.retain(|g| !merged.contains(g));
        generations.push(compact);
    }

    let folded: HashSet<(u32, u64)> = folded_ids;
    let meta = SfxDictionaryMeta { generations, next_generation, next_ids: dictionary.next_ids(), field_ids };
    let next = SfxDictionary::open(directory, &meta, Some(&dictionary));
    next.forget_pending(&folded);
    index.set_sfx_dictionary(Some(Arc::new(next)));
    Ok(())
}
