//! BriquesContext — unified access to all index files for query-time briques.
//!
//! Single struct passed to all briques functions instead of 10+ Option params.
//! Adding a new index file = one field here + loading it in one place.
//! Missing required file = panic with clear message (no silent fallback).

use std::collections::HashSet;

use crate::DocId;
use crate::query::posting_resolver::PostingResolver;
use crate::suffix_fst::file_v3::SfxFileReaderV3;
use crate::suffix_fst::posmap::PosMapReader;
use crate::suffix_fst::bytemap::ByteBitmapReader;
use crate::suffix_fst::word_sfxpost::WordSfxPostReader;
use crate::suffix_fst::sibling_table::SiblingTableReader;
use crate::suffix_fst::termtexts_v3::TermTextsReaderV3;

/// Unified context for all query-time briques.
///
/// All index file access goes through this struct. Functions that need
/// a specific file call `ctx.require_*()` which panics if the file
/// wasn't loaded — this surfaces config errors immediately instead of
/// silently degrading.
pub struct BriquesContext<'a> {
    // ── Required (always present) ────────────────────────────────
    pub reader: &'a SfxFileReaderV3,
    pub resolver: &'a dyn PostingResolver,

    // ── Query params ─────────────────────────────────────────────
    pub filter_docs: Option<&'a HashSet<DocId>>,

    /// When true, briques dump detailed traces to stderr.
    pub debug: bool,

    /// Trace ID for structured QueryTrace. None = no tracing (zero overhead).
    /// Set via trace::trace_begin() in V3_DIAG mode.
    pub trace_id: Option<u64>,

    // ── Optional index files ─────────────────────────────────────
    // Loaded from segment reader. None = file not present in this segment.
    pub posmap: Option<PosMapReader<'a>>,
    pub bytemap: Option<ByteBitmapReader<'a>>,
    pub word_sfxpost: Option<WordSfxPostReader<'a>>,
    pub sibling_v3: Option<SiblingTableReader<'a>>,
    pub termtexts: Option<TermTextsReaderV3<'a>>,
}

impl<'a> BriquesContext<'a> {
    // ── Require methods (panic if missing) ───────────────────────

    pub fn require_posmap(&self) -> &PosMapReader<'a> {
        self.posmap.as_ref().expect("posmap required but not loaded — check index config")
    }

    pub fn require_bytemap(&self) -> &ByteBitmapReader<'a> {
        self.bytemap.as_ref().expect("bytemap required but not loaded — check index config")
    }

    pub fn require_word_sfxpost(&self) -> &WordSfxPostReader<'a> {
        self.word_sfxpost.as_ref().expect("word_sfxpost required but not loaded — check index config")
    }

    pub fn require_sibling_v3(&self) -> &SiblingTableReader<'a> {
        self.sibling_v3.as_ref().expect("sibling_v3 required but not loaded — check index config")
    }

    pub fn require_termtexts(&self) -> &TermTextsReaderV3<'a> {
        self.termtexts.as_ref().expect("termtexts required but not loaded — check index config")
    }

    // ── Convenience: check if word pipeline is available ─────────

    /// True if all files needed for the word pipeline (relaxed mode) are loaded.
    pub fn has_word_pipeline(&self) -> bool {
        self.posmap.is_some() && self.bytemap.is_some() && self.word_sfxpost.is_some()
    }

    /// True if sibling-based chain building is available.
    pub fn has_sibling_chains(&self) -> bool {
        self.sibling_v3.is_some() && self.termtexts.is_some()
    }

    // ── Trace helpers (no-op when trace_id is None) ──────────────

    pub fn trace(&self, label: &str, data: &[(&str, &dyn std::fmt::Display)]) {
        if let Some(tid) = self.trace_id {
            super::trace::trace_event(tid, label, data);
        }
    }

    /// Convenience: trace with a pre-formatted message.
    pub fn trace_msg(&self, msg: &str) {
        if let Some(tid) = self.trace_id {
            super::trace::trace_event(tid, msg, &[]);
        }
    }

    pub fn trace_enter(&self, label: &str) {
        if let Some(tid) = self.trace_id {
            super::trace::trace_enter(tid, label);
        }
    }

    pub fn trace_exit(&self) {
        if let Some(tid) = self.trace_id {
            super::trace::trace_exit(tid);
        }
    }
}
