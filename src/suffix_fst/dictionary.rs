//! The shard dictionary: one `.sfx` (suffix FST + parents) and one
//! `.termtexts` per field, shared by every segment of the shard, with
//! ordinals that are **global ids** minted once per distinct term.
//!
//! An index whose `sfx_version` is [`DICTIONARY_SFX_VERSION`] has one.
//! Its segments carry no `.sfx` and no `.termtexts` of their own: they
//! keep local ordinals for their postings and maps and a `.gmap`
//! (`gmap.rs`) that says which global id each local is. The reason: the
//! dictionary is 60 % of an index and repeats ×2.6 across the segments of a
//! kernel index (`docs/04-09-2026/09`).
//!
//! The dictionary is written in **generations**: generation `g` is the
//! files `dict-<g>.<field>.sfx` and `dict-<g>.<field>.termtexts`, complete
//! (every id ever minted), immutable. A commit that minted new ids writes
//! generation `g + 1` next to `g`, then `meta.json` names it; `g`'s files
//! are garbage once no live `meta.json` names them (`segment_updater::
//! list_files`). Incremental generations — a file per commit holding only
//! the new ids — come after this first, whole-rebuild form.
//!
//! `meta.json` carries [`SfxDictionaryMeta`]: the generation, the next id
//! to mint, and the fields. The runtime [`SfxDictionary`] is what an
//! `Index` holds and refreshes when `meta.json` changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::directory::{Directory, FileSlice};
use crate::index::SfxDictionaryMeta;

/// `IndexSettings::sfx_version` of an index with a shard dictionary: the v3
/// engine, keys and files, over global ids.
pub const DICTIONARY_SFX_VERSION: u8 = 4;

/// File name of one generation's file for one field.
pub fn dictionary_file_name(generation: u64, field_id: u32, ext: &str) -> String {
    format!("dict-{generation}.{field_id}.{ext}")
}

/// The delta bundle id under which a generation's files travel: a prefix
/// of their names (`fs_utils::apply_delta` removes by prefix), with the dot
/// so that generation 1 never matches generation 10.
pub fn dictionary_bundle_id(generation: u64) -> String {
    format!("dict-{generation}.")
}

/// A generation of one field's files, open.
pub struct DictionaryField {
    /// Suffix FST + parents (an `SFX3` container over global ids).
    pub sfx: FileSlice,
    /// Global id → extended text + meta.
    pub termtexts: FileSlice,
}

/// The shard dictionary an `Index` holds: its meta and its open files.
pub struct SfxDictionary {
    meta: SfxDictionaryMeta,
    /// Next global id to mint; indexers take from it, the commit records it.
    next_id: AtomicU64,
    fields: HashMap<u32, DictionaryField>,
}

impl SfxDictionary {
    /// Open the generation `meta` names from `directory`. A field whose
    /// files are absent is simply not there (an index created with the
    /// dictionary but nothing committed yet has no generation file at all).
    pub fn open(directory: &dyn Directory, meta: &SfxDictionaryMeta) -> Self {
        let mut fields = HashMap::new();
        for &field_id in &meta.field_ids {
            let sfx = directory.open_read(&PathBuf::from(dictionary_file_name(meta.generation, field_id, "sfx")));
            let termtexts = directory.open_read(&PathBuf::from(dictionary_file_name(meta.generation, field_id, "termtexts")));
            if let (Ok(sfx), Ok(termtexts)) = (sfx, termtexts) {
                fields.insert(field_id, DictionaryField { sfx, termtexts });
            }
        }
        Self { meta: meta.clone(), next_id: AtomicU64::new(meta.next_id), fields }
    }

    /// The meta this dictionary was opened from.
    pub fn meta(&self) -> &SfxDictionaryMeta {
        &self.meta
    }

    /// Generation number.
    pub fn generation(&self) -> u64 {
        self.meta.generation
    }

    /// The open files of a field, if this generation has them.
    pub fn field(&self, field_id: u32) -> Option<&DictionaryField> {
        self.fields.get(&field_id)
    }

    /// Mint `count` consecutive global ids; returns the first.
    pub fn mint(&self, count: u64) -> u64 {
        self.next_id.fetch_add(count, Ordering::Relaxed)
    }

    /// The next id that would be minted.
    pub fn next_id(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed)
    }
}
