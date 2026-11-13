//! Check a shard dictionary for lost words.
//!
//! Before 4.3.5, a merge that started between two commits could lose the
//! words minted by the segments written since the last commit: the segments
//! (and the segments merged from them) still referenced those word ids, but
//! no dictionary generation held their text any more. Such documents dropped
//! out of fuzzy searches entirely, and out of exact searches for the words
//! only they contained. 4.3.5 stops new losses; it cannot bring the lost words
//! back. This check finds them, so that a caller knows whether an index has
//! to be rebuilt (its documents added again to a new index).
//!
//! Read-only: it walks the committed segments the reader sees, never opens a
//! writer and never writes. Only a shard-dictionary index (sfx version 4, the
//! default) can lose words this way; any other index is reported as not
//! concerned.

use ld_lucivy::schema::{Field, FieldType};
use ld_lucivy::suffix_fst::dictionary::DICTIONARY_SFX_VERSION;
use ld_lucivy::suffix_fst::gmap::GmapReader;

use crate::handle::LucivyHandle;

/// The result for one text field of one index (or one shard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldCheck {
    /// The field's name in the schema.
    pub field: String,
    /// Word ids referenced by the segments, summed over segments.
    pub referenced: u64,
    /// Of those, the ids no dictionary generation (nor pending pair) has a
    /// text or a meta for: words lost.
    pub unknown: u64,
}

/// The result of [`LucivyHandle::check_dictionary`] or
/// [`crate::sharded_handle::ShardedHandle::check_dictionary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryCheck {
    /// False when the index has no shard dictionary (sfx version other than
    /// 4): it cannot have lost words this way.
    pub concerned: bool,
    /// Committed segments walked.
    pub segments: usize,
    /// One entry per text field (per shard on a sharded index, the shard
    /// number in the name: `shard_1/content`).
    pub fields: Vec<FieldCheck>,
}

impl DictionaryCheck {
    /// Word ids lost, all fields together.
    pub fn unknown(&self) -> u64 {
        self.fields.iter().map(|f| f.unknown).sum()
    }

    /// True when words are lost: the index answers wrong until rebuilt.
    pub fn needs_rebuild(&self) -> bool {
        self.unknown() > 0
    }

    /// What the check found, in one or two sentences, with what to do.
    pub fn summary(&self) -> String {
        if !self.concerned {
            return "This index has no shard dictionary (sfx version other than 4): \
                    it cannot have lost words this way. Nothing to do."
                .to_string();
        }
        let referenced: u64 = self.fields.iter().map(|f| f.referenced).sum();
        if !self.needs_rebuild() {
            return format!(
                "No lost word: {referenced} word ids referenced by {} segment(s), all known to \
                 the dictionary. Nothing to do.",
                self.segments
            );
        }
        let lost: Vec<String> = self.fields.iter().filter(|f| f.unknown > 0)
            .map(|f| format!("{} ({} of {})", f.field, f.unknown, f.referenced))
            .collect();
        format!(
            "{} word ids are missing from the dictionary: {}. They were lost by a merge \
             between two commits, before lucivy 4.3.5. Documents using them are missing from \
             fuzzy searches, and from exact searches for the words only they contain. Rebuild \
             this index: add its documents again to a new index with lucivy 4.3.5 or later.",
            self.unknown(),
            lost.join(", ")
        )
    }
}

impl LucivyHandle {
    /// Count the word ids the committed segments reference and the shard
    /// dictionary no longer knows (see [`crate::dictionary_check`]).
    /// Read-only; checks what the reader currently sees.
    pub fn check_dictionary(&self) -> Result<DictionaryCheck, String> {
        self.check_dictionary_named("")
    }

    pub(crate) fn check_dictionary_named(&self, prefix: &str) -> Result<DictionaryCheck, String> {
        if self.index.settings().sfx_version != DICTIONARY_SFX_VERSION {
            return Ok(DictionaryCheck { concerned: false, segments: 0, fields: Vec::new() });
        }
        let searcher = self.reader.searcher();
        let text_fields: Vec<(&str, Field)> = self.field_map.iter()
            .filter(|(_, f)| matches!(self.schema.get_field_entry(*f).field_type(), FieldType::Str(_)))
            .map(|(name, f)| (name.as_str(), *f))
            .collect();
        let mut fields = Vec::new();
        for (name, field) in text_fields {
            let mut check = FieldCheck { field: format!("{prefix}{name}"), referenced: 0, unknown: 0 };
            for segment in searcher.segment_readers() {
                let Some(file) = segment.sfx_index_file("gmap", field) else { continue };
                let bytes = file.read_bytes()
                    .map_err(|e| format!("segment {}: read gmap of {name}: {e}", segment.segment_id().short_uuid_string()))?;
                let gmap = GmapReader::open(&bytes)
                    .ok_or_else(|| format!("segment {}: invalid gmap of {name}", segment.segment_id().short_uuid_string()))?;
                let texts = segment.sfx_dictionary_field(field).and_then(|d| d.termtexts_reader());
                for global in gmap.iter() {
                    check.referenced += 1;
                    let known = texts.as_ref()
                        .is_some_and(|t| t.text(global).is_some() && t.meta(global).is_some());
                    if !known {
                        check.unknown += 1;
                    }
                }
            }
            fields.push(check);
        }
        Ok(DictionaryCheck { concerned: true, segments: searcher.segment_readers().len(), fields })
    }
}
