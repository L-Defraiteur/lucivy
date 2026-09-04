//! The shard dictionary's generation, rebuilt at commit.
//!
//! A commit on a `sfx_version` 4 index folds the `.newtexts` of the segments
//! it commits — the texts of the ids they minted — into a new generation:
//! every text of the live generation plus those, one `.sfx` and one
//! `.termtexts` per field, written whole (the incremental form comes
//! later), then made live on the `Index` so that `save_metas` names it.
//! Nothing new to fold → the generation stays.
//!
//! Ids are stable and append-only, so a rebuild changes no segment.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::directory::{Directory, TerminatingWrite};
use crate::index::SfxDictionaryMeta;
use crate::suffix_fst::builder_v3::SuffixFstBuilderV3;
use crate::suffix_fst::dictionary::{decode_newtexts, dictionary_file_name, SfxDictionary, DICTIONARY_SFX_VERSION};
use crate::suffix_fst::file_v3::SfxFileWriterV3;
use crate::suffix_fst::termtexts_v3::{TermMetaV3, TermTextsWriterV3};

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

    let generation = dictionary.generation() + 1;
    let mut field_ids: Vec<u32> = dictionary.meta().field_ids.clone();
    for &f in new_by_field.keys() {
        if !field_ids.contains(&f) {
            field_ids.push(f);
        }
    }
    field_ids.sort_unstable();

    let directory = index.directory();
    for &field_id in &field_ids {
        // Every text of the live generation, then the new ones.
        let mut texts = TermTextsWriterV3::new();
        let mut builder = SuffixFstBuilderV3::new();
        builder.set_max_ordinal(u32::MAX as u64);
        let mut feed = |global: u32, text: &str, m: TermMetaV3, texts: &mut TermTextsWriterV3, builder: &mut SuffixFstBuilderV3| {
            texts.add(global, text, m);
            if m.is_word_stripped {
                let content_len = text.len().saturating_sub(m.overlap_len as usize);
                builder.add_word_stripped(&text[..content_len], &text[content_len..], global as u64,
                    m.own_len, m.sep_len, m.is_word_start);
            } else {
                builder.add_token(text, global as u64, m.own_len, m.sep_len, m.overlap_len, m.is_word_start);
            }
        };
        if let Some(old) = dictionary.field(field_id).and_then(|f| f.termtexts_reader().map(|r| r.iter().map(|(g, t, m)| (g, t.to_string(), m)).collect::<Vec<_>>())) {
            for (g, t, m) in old {
                if m.own_len == 0 && t.is_empty() { continue; } // a hole
                feed(g, &t, m, &mut texts, &mut builder);
            }
        }
        if let Some(new) = new_by_field.get(&field_id) {
            for (g, t, m) in new {
                feed(*g, t, *m, &mut texts, &mut builder);
            }
        }
        let (fst, parents) = builder.build().map_err(|e| crate::LucivyError::SystemError(
            format!("dictionary generation {generation} field {field_id}: {e}")))?;
        let sfx = SfxFileWriterV3::new(fst, parents).to_bytes();
        let termtexts = texts.serialize();
        for (ext, bytes) in [("sfx", sfx), ("termtexts", termtexts)] {
            let path = PathBuf::from(dictionary_file_name(generation, field_id, ext));
            let mut w = directory.open_write(&path)?;
            w.write_all(&bytes)?;
            w.terminate()?;
        }
    }

    let folded: HashSet<(u32, u64)> = new_by_field.iter()
        .flat_map(|(f, v)| v.iter().map(move |(g, _, _)| (*f, *g as u64))).collect();
    let meta = SfxDictionaryMeta { generation, next_ids: dictionary.next_ids(), field_ids };
    let next = SfxDictionary::open(directory, &meta, Some(&dictionary));
    next.forget_pending(&folded);
    index.set_sfx_dictionary(Some(Arc::new(next)));
    Ok(())
}
