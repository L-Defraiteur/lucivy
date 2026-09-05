//! The shard dictionary's generations, written around the commit.
//!
//! On a `sfx_version` 4 index every segment writes, next to its postings,
//! the pair `<uuid>.<field>.newsfx` / `.newtexts`: the suffix FST and the
//! texts of the ids it minted, built on its own build thread — the same
//! shape as a generation of the dictionary. A commit does **not** merge
//! them: it names the new segments in `SfxDictionaryMeta::pending_segments`,
//! so that readers see the pairs as parts of the dictionary next to the
//! generations, and it starts a background task (`run_fold`) that merges
//! every pending pair into the next generation in streams
//! (`dictionary_compact::compact_parts`), compacts past
//! `LUCIVY_DICT_MAX_GENERATIONS` live generations, and swaps the live
//! dictionary in RAM; the next commit writes that to `meta.json` and the
//! consumed pairs are deleted once no `meta.json` names them.
//!
//! Why: the fold was 8.8 s of the commit path on 30 000 kernel files,
//! serial with the document stream — half the gap to a v3 index. Now the
//! commit pays the ids' read and a reopen.
//!
//! What a search sees: by default (`IndexSettings::dictionary_wait`) a
//! search waits for the running fold (`Index::wait_dictionary_fold`), so a
//! query never walks the pairs and its cost never depends on when it runs.
//!
//! Bounds: one fold at a time per index; past `LUCIVY_DICT_MAX_PENDING`
//! (16) pending segments a commit waits for the running fold and folds the
//! rest itself, synchronously; `LUCIVY_DICT_SYNC_FOLD=1` folds every commit
//! synchronously — the default on wasm32, where the background fold gains
//! no time and costs memory (see `sync_fold`).
//!
//! Ids are stable and append-only, so a fold changes no segment.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::directory::Directory;
use crate::index::Index;
use crate::suffix_fst::dictionary::{decode_newtexts, DICTIONARY_SFX_VERSION, SfxDictionary};
use crate::suffix_fst::dictionary_compact::{choose_compaction, compact_generations, compact_parts, generation_bytes, generation_sfx_bytes, remove_leftovers, write_generation};
use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;
use common::OwnedBytes;

use super::segment_updater::SegmentUpdaterShared;

fn max_pending() -> usize {
    std::env::var("LUCIVY_DICT_MAX_PENDING").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(16)
}

fn max_generations() -> usize {
    std::env::var("LUCIVY_DICT_MAX_GENERATIONS").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(8)
}

/// Fold at the commit, synchronously, instead of in the background. The
/// default on wasm32: measured on Linux 2.6.0 in the browser (6 September),
/// the background fold gained nothing (41 → 42 s, few threads) and raised
/// the memory high-water mark 2 023 → 2 279 MB, the fold's buffers and the
/// compaction's reads landing on top of the segment builds; at the commit
/// the same work runs when nothing else does. `LUCIVY_DICT_SYNC_FOLD=1`
/// natively, `=0` on wasm to try the background fold.
fn sync_fold() -> bool {
    match std::env::var("LUCIVY_DICT_SYNC_FOLD") {
        Ok(v) => v != "0",
        Err(_) => cfg!(target_arch = "wasm32"),
    }
}

/// Name the not-yet-committed segments' pairs as pending parts of the live
/// dictionary and start the background fold. No-op on other indexes.
pub(crate) fn fold_new_texts(
    shared: &Arc<SegmentUpdaterShared>,
    self_ref: Option<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>,
) -> crate::Result<()> {
    let index = &shared.index;
    if index.settings().sfx_version != DICTIONARY_SFX_VERSION {
        return Ok(());
    }
    let verbose = crate::diag::is_verbose();
    let t_commit = std::time::Instant::now();
    let committed: HashSet<_> = shared.segment_manager.committed_segment_metas()
        .iter().map(|m| m.id()).collect();

    // The new segments: their minted ids (to release the pending texts once
    // the pairs are live parts), and their pair — built now, from the
    // texts, for a segment written before `.newsfx` existed.
    let mut new_pending: Vec<String> = Vec::new();
    let mut folded_ids: HashSet<(u32, u64)> = HashSet::new();
    let mut new_texts: usize = 0;
    let mut new_fields: Vec<u32> = Vec::new();
    for entry in shared.segment_manager.segment_entries() {
        let meta = entry.meta();
        if committed.contains(&meta.id()) {
            continue;
        }
        let mut segment = index.segment(meta.clone());
        let uuid = meta.id().uuid_string();
        let mut has_pair = false;
        for &field_id in meta.sfx_field_ids() {
            let Ok(slice) = segment.open_read_custom(&format!("{field_id}.newtexts")) else { continue };
            let texts = slice.read_bytes()?;
            let Some(reader) = TermTextsReaderV3::open(&texts) else { continue };
            let before = folded_ids.len();
            folded_ids.extend(reader.iter().map(|(g, _, _)| (field_id, g as u64)));
            if folded_ids.len() == before { continue; }
            new_texts += folded_ids.len() - before;
            has_pair = true;
            if !new_fields.contains(&field_id) { new_fields.push(field_id); }
            if segment.open_read_custom(&format!("{field_id}.newsfx")).is_err() {
                let entries = decode_newtexts(&texts).unwrap_or_default();
                let sfx = generation_sfx_bytes(&entries).map_err(|e| crate::LucivyError::SystemError(
                    format!("segment {uuid} field {field_id}: dictionary FST: {e}")))?;
                let mut w = segment.open_write_custom(&format!("{field_id}.newsfx"))?;
                use std::io::Write;
                use common::TerminatingWrite;
                w.write_all(&sfx)?;
                w.terminate()?;
            }
        }
        if has_pair {
            new_pending.push(uuid);
        }
    }
    if new_pending.is_empty() {
        if verbose {
            eprintln!("[dictionary] commit: nothing new; {}", crate::suffix_fst::dictionary::stats::take());
        }
        return Ok(());
    }

    let fold = index.dictionary_fold().clone();
    let mut meta = {
        let _meta_lock = fold.lock_meta();
        let dictionary = live_dictionary(index)?;
        let mut meta = dictionary.meta().clone();
        meta.next_ids = dictionary.next_ids();
        for f in new_fields {
            if !meta.field_ids.contains(&f) { meta.field_ids.push(f); }
        }
        meta.field_ids.sort_unstable();
        meta.pending_segments.extend(new_pending);
        let next = SfxDictionary::open(index.directory(), &meta, Some(&dictionary));
        next.forget_pending(&folded_ids);
        index.set_sfx_dictionary(Some(Arc::new(next)));
        meta
    };
    let named_ms = t_commit.elapsed().as_secs_f64() * 1e3;

    // Too many pairs for the readers to walk, or asked to: fold here.
    let mut sync_ms = 0.0;
    if sync_fold() || meta.pending_segments.len() > max_pending() {
        let t = std::time::Instant::now();
        fold.wait();
        meta = live_dictionary(index)?.meta().clone();
        if sync_fold() || meta.pending_segments.len() > max_pending() {
            if fold.begin() {
                let r = fold_once(index, verbose);
                fold.finish();
                r?;
            }
        }
        sync_ms = t.elapsed().as_secs_f64() * 1e3;
    } else if fold.begin() {
        let shared = shared.clone();
        crate::actor::scheduler::global_scheduler().submit_task(crate::actor::Priority::High, move || {
            run_fold(&shared.index, self_ref);
            Ok::<(), crate::LucivyError>(())
        });
    }
    if verbose {
        eprintln!("[dictionary] commit: {new_texts} new texts in {} pair(s) named in {named_ms:.0} ms{}; since last commit: {}",
            meta.pending_segments.len(),
            if sync_ms > 0.0 { format!(", folded synchronously in {sync_ms:.0} ms") } else { String::new() },
            crate::suffix_fst::dictionary::stats::take());
    }
    Ok(())
}

fn live_dictionary(index: &Index) -> crate::Result<Arc<SfxDictionary>> {
    index.sfx_dictionary().ok_or_else(|| crate::LucivyError::SystemError(
        "sfx_version 4 index without a shard dictionary".to_string()))
}

/// The background task: fold while pairs are pending, then release the slot.
/// The caller holds the slot (`DictionaryFold::begin`).
pub(crate) fn run_fold(
    index: &Index,
    updater: Option<crate::actor::mailbox::ActorRef<crate::actor::envelope::Envelope>>,
) {
    let fold = index.dictionary_fold().clone();
    let verbose = crate::diag::is_verbose();
    let mut folded = false;
    loop {
        // One slot of the merge pool: a fold reads whole generations.
        let _permit = super::merge_permits::acquire();
        match fold_once(index, verbose) {
            Ok(true) => folded = true,
            Ok(false) => break,
            Err(e) => {
                eprintln!("[dictionary] background fold failed: {e}");
                break;
            }
        }
    }
    // The disk should name the generation, not the pairs: ask the updater
    // to rewrite meta.json (as a finished merge does), then release.
    if folded {
        if let Some(updater) = updater {
            fold.set_persist_pending();
            let sent = updater.send(crate::actor::envelope::Envelope {
                type_tag: <super::segment_updater_actor::SuDictionaryFoldedMsg as crate::actor::Message>::type_tag(),
                payload: vec![],
                reply: None,
                local: None,
            });
            if sent.is_err() {
                fold.persisted();
            }
        }
    }
    fold.finish();
}

/// Merge every pending pair of the live dictionary into the next generation
/// (and compact), then swap the live dictionary. Returns false when nothing
/// was pending.
fn fold_once(index: &Index, verbose: bool) -> crate::Result<bool> {
    let fold = index.dictionary_fold().clone();
    let directory = index.directory();
    let dictionary = live_dictionary(index)?;
    let snapshot = dictionary.meta().clone();
    if snapshot.pending_segments.is_empty() {
        return Ok(false);
    }
    let t = std::time::Instant::now();
    let field_ids = snapshot.field_ids.clone();
    let mut next_generation = snapshot.next_generation.max(1);
    let generation = next_generation;
    next_generation += 1;
    remove_leftovers(directory, generation, &field_ids)?;
    let mut texts_folded: u32 = 0;
    for &field_id in &field_ids {
        let mut sfx_parts: Vec<OwnedBytes> = Vec::new();
        let mut termtexts_parts: Vec<OwnedBytes> = Vec::new();
        for uuid in &snapshot.pending_segments {
            let sfx = directory.open_read(&PathBuf::from(format!("{uuid}.{field_id}.newsfx")));
            let termtexts = directory.open_read(&PathBuf::from(format!("{uuid}.{field_id}.newtexts")));
            let (Ok(sfx), Ok(termtexts)) = (sfx, termtexts) else { continue };
            sfx_parts.push(sfx.read_bytes()?);
            termtexts_parts.push(termtexts.read_bytes()?);
        }
        if sfx_parts.is_empty() {
            write_generation(directory, generation, field_id, &[])?;
            continue;
        }
        let report = compact_parts(directory, sfx_parts, termtexts_parts, field_id, generation)?;
        texts_folded += report.texts;
    }
    let fold_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut generations = snapshot.generations.clone();
    generations.push(generation);

    // Too many generations to walk: the smallest ones merge into one.
    let t_compact = std::time::Instant::now();
    let sizes: Vec<(u64, u64)> = generations.iter()
        .map(|&g| (g, generation_bytes(directory, g, &field_ids))).collect();
    let mut compacted: Option<usize> = None;
    if let Some(merged) = choose_compaction(&sizes, max_generations()) {
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

    // Swap: the live meta may have gained pairs since the snapshot.
    let t_open = std::time::Instant::now();
    let live_pairs = {
        let _meta_lock = fold.lock_meta();
        let current = live_dictionary(index)?;
        let mut meta = current.meta().clone();
        meta.next_ids = current.next_ids();
        meta.generations = generations;
        meta.next_generation = next_generation;
        meta.pending_segments.retain(|u| !snapshot.pending_segments.contains(u));
        let live_pairs = meta.pending_segments.len();
        let next = SfxDictionary::open(directory, &meta, Some(&current));
        index.set_sfx_dictionary(Some(Arc::new(next)));
        fold.mark_changed();
        live_pairs
    };
    if verbose {
        let compaction = match compacted {
            Some(n) => format!("compaction of {n} generations {compact_ms:.0} ms"),
            None => format!("no compaction ({compact_ms:.0} ms sizing)"),
        };
        eprintln!("[dictionary] fold: generation {generation} from {} pair(s), {texts_folded} texts, {fold_ms:.0} ms; {compaction}; reopened {:.0} ms; {live_pairs} pair(s) still pending",
            snapshot.pending_segments.len(), t_open.elapsed().as_secs_f64() * 1e3);
    }
    Ok(true)
}

/// After `save_metas`: delete the pairs of committed segments that the
/// `meta.json` just written no longer names — folded into a generation it
/// does name. A pair still named on disk stays, whatever the live
/// dictionary says: a crash before the next commit reopens from the disk.
pub(crate) fn delete_consumed_pairs(shared: &Arc<SegmentUpdaterShared>) -> crate::Result<()> {
    let index = &shared.index;
    if index.settings().sfx_version != DICTIONARY_SFX_VERSION {
        return Ok(());
    }
    let written = shared.load_meta();
    let Some(dict) = written.sfx_dictionary.as_ref() else { return Ok(()) };
    let named: HashSet<&str> = dict.pending_segments.iter().map(|s| s.as_str()).collect();
    for meta in shared.segment_manager.committed_segment_metas() {
        let uuid = meta.id().uuid_string();
        if named.contains(uuid.as_str()) {
            continue;
        }
        for &field_id in meta.sfx_field_ids() {
            for ext in ["newtexts", "newsfx"] {
                let path = PathBuf::from(format!("{uuid}.{field_id}.{ext}"));
                match index.directory().delete(&path) {
                    Ok(()) | Err(crate::directory::error::DeleteError::FileDoesNotExist(_)) => {}
                    Err(e) => return Err(crate::LucivyError::SystemError(
                        format!("cannot delete consumed {}: {e}", path.display()))),
                }
            }
        }
    }
    Ok(())
}
