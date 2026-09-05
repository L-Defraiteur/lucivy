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
//! files `dict-<g>.<field>.sfx` and `dict-<g>.<field>.termtexts`, holding
//! the ids minted by one span of commits (its `.termtexts` names them,
//! `SECTION_IDS`), immutable. A commit that minted new ids writes the next
//! generation with those ids only, then `meta.json` names it among the
//! live ones; past `LUCIVY_DICT_MAX_GENERATIONS` (8) a commit merges the
//! smallest ones into one, in streams (`dictionary_compact.rs`). A
//! generation's files are garbage once no live
//! `meta.json` names it (`segment_updater::list_files`). Readers see the
//! live generations as one (`SfxFileReaderV3::open_parts`,
//! `TermTextsReaderV3::open_parts`).
//!
//! `meta.json` carries [`SfxDictionaryMeta`]: the live generations, the
//! next id to mint per field, and the fields. The runtime [`SfxDictionary`]
//! is what an `Index` holds and refreshes when `meta.json` changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::OwnedBytes;

use crate::directory::{Directory, FileSlice};
use crate::index::SfxDictionaryMeta;
use super::builder::SI0_PREFIX;
use super::builder_v3::{MAX_OVERLAP_BYTES, SI_STRIPPED_PREFIX};
use super::collector_v3::TokenMetaV3;
use super::file_v3::SfxFileReaderV3;
use super::termtexts_v3::{TermMetaV3, TermTextsReaderV3, TermTextsWriterV3};

/// `IndexSettings::sfx_version` of an index with a shard dictionary: the v3
/// engine, keys and files, over global ids.
pub const DICTIONARY_SFX_VERSION: u8 = 4;

/// Cumulative cost of the per-token path (`lookup_or_mint`) since the last
/// `take`, counted only under `LUCIVY_VERBOSE`: the commit prints it next to
/// the generation's write, so that the indexing time of a dictionary index
/// splits into what the collectors pay and what the commit pays.
pub mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static CALLS: AtomicU64 = AtomicU64::new(0);
    pub static HITS: AtomicU64 = AtomicU64::new(0);
    /// Answered by the pending texts.
    pub static PENDING_HITS: AtomicU64 = AtomicU64::new(0);
    pub static MINTS: AtomicU64 = AtomicU64::new(0);
    /// Whole `lookup_or_mint`, nanoseconds.
    pub static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: opening the `.termtexts` readers of the generations.
    pub static OPEN_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: the FST gets and parent decodes.
    pub static FST_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: under the shared lock (pending map and counter).
    pub static LOCK_NS: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone, Copy, Default)]
    pub struct Snapshot {
        pub calls: u64,
        pub hits: u64,
        pub pending_hits: u64,
        pub mints: u64,
        pub total_ns: u64,
        pub open_ns: u64,
        pub fst_ns: u64,
        pub lock_ns: u64,
    }

    /// Read and reset every counter.
    pub fn take() -> Snapshot {
        Snapshot {
            calls: CALLS.swap(0, Relaxed),
            hits: HITS.swap(0, Relaxed),
            pending_hits: PENDING_HITS.swap(0, Relaxed),
            mints: MINTS.swap(0, Relaxed),
            total_ns: TOTAL_NS.swap(0, Relaxed),
            open_ns: OPEN_NS.swap(0, Relaxed),
            fst_ns: FST_NS.swap(0, Relaxed),
            lock_ns: LOCK_NS.swap(0, Relaxed),
        }
    }

    impl std::fmt::Display for Snapshot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let ms = |ns: u64| ns as f64 / 1e6;
            write!(f, "{} lookups ({} in a generation, {} pending, {} minted): {:.0} ms, of which termtexts open {:.0}, fst {:.0}, lock {:.0}",
                self.calls, self.hits, self.pending_hits, self.mints,
                ms(self.total_ns), ms(self.open_ns), ms(self.fst_ns), ms(self.lock_ns))
        }
    }
}

/// The slot an `Index` keeps its live dictionary in; a collector reads it at
/// each lookup so that a commit's swap reaches writers already running.
pub type DictionarySlot = Arc<std::sync::RwLock<Option<Arc<SfxDictionary>>>>;

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

/// One field's files across the live generations, open.
pub struct DictionaryField {
    /// Suffix FST + parents of the first live generation (an `SFX3`
    /// container over global ids) — what `SegmentReader` hands out as the
    /// segment's `.sfx` for the version sniff.
    pub sfx: FileSlice,
    /// The FST reader over every live generation, opened once, memoizing.
    sfx_reader: SfxFileReaderV3,
    /// The texts of every live generation as one reader, opened once: it
    /// borrows `termtexts_bytes` below, which outlives it (declared after,
    /// never replaced, and `OwnedBytes` never moves its heap). Opening one
    /// parses every generation's id runs, so it must not happen per token
    /// — that was 8 % of `lookup_or_mint` on the kernel.
    termtexts: Option<TermTextsReaderV3<'static>>,
    /// The texts' bytes of every live generation, in order.
    termtexts_bytes: Vec<OwnedBytes>,
}

impl DictionaryField {
    fn new(sfx: FileSlice, sfx_reader: SfxFileReaderV3, termtexts_bytes: Vec<OwnedBytes>) -> Self {
        // SAFETY: `OwnedBytes` is an `Arc`-backed slice whose heap never
        // moves; `termtexts_bytes` lives in this struct as long as the
        // reader does and is never reassigned; the reader's drop touches
        // no byte. The `'static` is thus a lifetime the borrow checker
        // cannot see, not a claim about the process.
        let parts: Vec<&'static [u8]> = termtexts_bytes.iter()
            .map(|b| unsafe { std::slice::from_raw_parts(b.as_ptr(), b.len()) })
            .collect();
        let termtexts = TermTextsReaderV3::open_parts(&parts);
        Self { sfx, sfx_reader, termtexts, termtexts_bytes }
    }

    /// The texts of every live generation, as one reader.
    pub fn termtexts_reader(&self) -> Option<TermTextsReaderV3<'_>> {
        let parts: Vec<&[u8]> = self.termtexts_bytes.iter().map(|b| b.as_slice()).collect();
        TermTextsReaderV3::open_parts(&parts)
    }

    /// The same reader, opened once for the field's life.
    pub fn termtexts(&self) -> Option<&TermTextsReaderV3<'_>> {
        self.termtexts.as_ref()
    }

    /// The FST(s) of every live generation.
    pub fn sfx_reader(&self) -> &SfxFileReaderV3 {
        &self.sfx_reader
    }

    /// The global id of a token with exactly this text and shape, if the
    /// generation has it. The key is the lowercased own bytes (the
    /// content for a word entry) under the partition; the record narrows
    /// to the parents whose overlap and shape agree; the text itself,
    /// case included, is confirmed in the texts — the key does not see
    /// case, so `Mutex_` and `mutex_` are two ids under one key.
    pub fn lookup(&self, text: &str, meta: &TokenMetaV3) -> Option<u64> {
        let (partition, own, overlap) = if meta.is_word_stripped {
            let content_len = text.len().saturating_sub(meta.overlap_len as usize);
            (SI_STRIPPED_PREFIX, &text[..content_len], &text[content_len..])
        } else {
            let own_end = (meta.own_len as usize).min(text.len());
            (SI0_PREFIX, &text[..own_end], &text[own_end..])
        };
        let lower_own = lowercase_cow(own);
        let lower_overlap = lowercase_cow(overlap);
        let mut ov_end = lower_overlap.len().min(MAX_OVERLAP_BYTES);
        while ov_end > 0 && !lower_overlap.is_char_boundary(ov_end) {
            ov_end -= 1;
        }
        let want_overlap = &lower_overlap.as_bytes()[..ov_end];
        let mut key = Vec::with_capacity(1 + lower_own.len());
        key.push(partition);
        key.extend_from_slice(lower_own.as_bytes());
        let timed = crate::diag::is_verbose();
        let texts = self.termtexts.as_ref()?;
        let t_fst = timed.then(std::time::Instant::now);
        let _fst_guard = t_fst.map(|t| TimeInto(t, &stats::FST_NS));
        for part in self.sfx_reader.parts() {
            let Some(value) = part.fst().get(&key) else { continue };
            let parents = part.decode_parents_where(value, &key, |ov| ov == want_overlap);
            for p in parents {
                if p.sti != 0 { continue; }
                let shape_ok = if meta.is_word_stripped {
                    p.content_len() == meta.own_len.saturating_sub(meta.sep_len as u16)
                } else {
                    p.own_len == meta.own_len && p.sep_len == meta.sep_len && p.is_word_start == meta.is_word_start
                };
                if !shape_ok { continue; }
                if texts.text(p.raw_ordinal as u32) == Some(text) {
                    return Some(p.raw_ordinal);
                }
            }
        }
        None
    }
}

/// `s.to_lowercase()` without the allocation when `s` is already lowercase
/// ASCII — most tokens of a source tree.
fn lowercase_cow(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(s.to_lowercase())
    }
}

/// Adds the time since `.0` to `.1` when dropped (verbose accounting).
struct TimeInto<'a>(std::time::Instant, &'a std::sync::atomic::AtomicU64);

impl Drop for TimeInto<'_> {
    fn drop(&mut self) {
        self.1.fetch_add(self.0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What every generation of one shard shares while the process lives: the
/// id counter, and the texts minted but not yet folded into a generation.
///
/// Indexers mint against the live generation; a commit folds their texts
/// into the next one and swaps it in. A segment writer that started before
/// the swap keeps collecting after it, so the counter must be one for all
/// generations (or two writers would mint the same id), and a text minted
/// by one writer must be found by another before any generation has it —
/// that is `pending`, keyed by field and the collector's intern key, in
/// stripes so that collector threads rarely meet on a lock (one lock was
/// 4 s of waiting on the first commit of 30 000 kernel files).
///
/// Measured and refused (6 September): caching the keys *found* in a
/// generation too. It took 5.7 M of 8.3 M FST walks but cost as much in
/// lock time as it saved, for up to 32 MB per shard — and the walks run on
/// the collector threads, off the commit path that bounds the indexing time.
pub struct DictionaryShared {
    /// Next id per field.
    next_ids: Mutex<HashMap<u32, u64>>,
    /// The pending texts: field → collector intern key → id.
    stripes: Vec<Mutex<HashMap<u32, HashMap<String, u64>>>>,
}

const STRIPES: usize = 16;

impl DictionaryShared {
    fn new(next_ids: HashMap<u32, u64>) -> Self {
        Self {
            next_ids: Mutex::new(next_ids),
            stripes: (0..STRIPES).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn stripe(&self, field_id: u32, key: &str) -> &Mutex<HashMap<u32, HashMap<String, u64>>> {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        field_id.hash(&mut h);
        key.hash(&mut h);
        &self.stripes[(h.finish() as usize) % STRIPES]
    }
}

/// The shard dictionary an `Index` holds: its meta and its open files.
pub struct SfxDictionary {
    meta: SfxDictionaryMeta,
    fields: HashMap<u32, DictionaryField>,
    shared: Arc<DictionaryShared>,
}

impl SfxDictionary {
    /// Open the generation `meta` names from `directory`. A field whose
    /// files are absent is simply not there (an index created with the
    /// dictionary but nothing committed yet has no generation file at all).
    ///
    /// `previous` is the generation this one replaces in the same process:
    /// its counter and pending texts carry over. `None` opens fresh (a
    /// reader, or the first open).
    pub fn open(directory: &dyn Directory, meta: &SfxDictionaryMeta, previous: Option<&SfxDictionary>) -> Self {
        let mut fields = HashMap::new();
        for &field_id in &meta.field_ids {
            let mut first_sfx = None;
            let mut sfx_parts = Vec::new();
            let mut termtexts_bytes = Vec::new();
            for &g in &meta.generations {
                let sfx = directory.open_read(&PathBuf::from(dictionary_file_name(g, field_id, "sfx")));
                let termtexts = directory.open_read(&PathBuf::from(dictionary_file_name(g, field_id, "termtexts")));
                let (Ok(sfx), Ok(termtexts)) = (sfx, termtexts) else { continue };
                let (Ok(sfx_bytes), Ok(tt_bytes)) = (sfx.read_bytes(), termtexts.read_bytes()) else { continue };
                if first_sfx.is_none() { first_sfx = Some(sfx); }
                sfx_parts.push(sfx_bytes);
                termtexts_bytes.push(tt_bytes);
            }
            let (Some(sfx), Ok(sfx_reader)) = (first_sfx, SfxFileReaderV3::open_parts(sfx_parts)) else { continue };
            let sfx_reader = sfx_reader.with_memo(Arc::new(super::file_v3::FstMemo::new()));
            fields.insert(field_id, DictionaryField::new(sfx, sfx_reader, termtexts_bytes));
        }
        let shared = match previous {
            Some(prev) => prev.shared.clone(),
            None => Arc::new(DictionaryShared::new(meta.next_ids.iter().map(|(&f, &n)| (f, n)).collect())),
        };
        Self { meta: meta.clone(), fields, shared }
    }

    /// The dictionary of an index that has committed nothing yet: no file,
    /// no id minted.
    pub fn empty() -> Self {
        Self {
            meta: SfxDictionaryMeta { generations: Vec::new(), next_generation: 1, next_ids: Default::default(), field_ids: Vec::new() },
            fields: HashMap::new(),
            shared: Arc::new(DictionaryShared::new(HashMap::new())),
        }
    }

    /// The id of `text` with this shape in `field_id`, minting one if neither
    /// the generation nor the pending texts have it. `key` is the collector's
    /// intern key (text + shape). Returns `(id, minted here)`; a text minted
    /// by another writer since the last commit comes back with `false` — its
    /// minter writes it to `.newtexts`.
    pub fn lookup_or_mint(&self, field_id: u32, key: &str, text: &str, meta: &TokenMetaV3) -> (u64, bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let timed = crate::diag::is_verbose();
        let t_all = timed.then(std::time::Instant::now);
        let _all_guard = t_all.map(|t| TimeInto(t, &stats::TOTAL_NS));
        if timed { stats::CALLS.fetch_add(1, Relaxed); }
        if let Some(id) = self.field(field_id).and_then(|f| f.lookup(text, meta)) {
            if timed { stats::HITS.fetch_add(1, Relaxed); }
            return (id, false);
        }
        let t_lock = timed.then(std::time::Instant::now);
        let _lock_guard = t_lock.map(|t| TimeInto(t, &stats::LOCK_NS));
        let mut stripe = self.shared.stripe(field_id, key).lock().unwrap();
        if let Some(&id) = stripe.get(&field_id).and_then(|m| m.get(key)) {
            if timed { stats::PENDING_HITS.fetch_add(1, Relaxed); }
            return (id, false);
        }
        let id = {
            let mut next_ids = self.shared.next_ids.lock().unwrap();
            let next = next_ids.entry(field_id).or_insert(0);
            let id = *next;
            *next += 1;
            id
        };
        stripe.entry(field_id).or_default().insert(key.to_string(), id);
        if timed { stats::MINTS.fetch_add(1, Relaxed); }
        (id, true)
    }

    /// Forget the pending texts whose ids a generation now holds.
    pub fn forget_pending(&self, folded: &std::collections::HashSet<(u32, u64)>) {
        for stripe in &self.shared.stripes {
            for (f, m) in stripe.lock().unwrap().iter_mut() {
                m.retain(|_, id| !folded.contains(&(*f, *id)));
            }
        }
    }

    /// The meta this dictionary was opened from.
    pub fn meta(&self) -> &SfxDictionaryMeta {
        &self.meta
    }

    /// The live generations, ascending.
    pub fn generations(&self) -> &[u64] {
        &self.meta.generations
    }

    /// The open files of a field, if this generation has them.
    pub fn field(&self, field_id: u32) -> Option<&DictionaryField> {
        self.fields.get(&field_id)
    }

    /// The next id that would be minted, per field, as `meta.json` records it.
    pub fn next_ids(&self) -> std::collections::BTreeMap<u32, u64> {
        self.shared.next_ids.lock().unwrap().iter().map(|(&f, &n)| (f, n)).collect()
    }
}

// ─── `.newtexts` ─────────────────────────────────────────────────────────
//
// The texts and meta of the ids a segment minted first: a `.gmap` of those
// ids (sorted, since the segment numbers its locals by global id) followed
// by a `TTX3` file whose ordinal `i` is the `i`-th id. The commit folds
// them into the next generation.

/// Serialize the minted ids with their texts and meta (ids ascending): a
/// `TTX3` file whose IDS section names the ids — the same shape as a
/// generation of the dictionary.
pub fn encode_newtexts(entries: &[(u32, &str, TermMetaV3)]) -> Vec<u8> {
    let ids: Vec<u32> = entries.iter().map(|e| e.0).collect();
    let mut w = TermTextsWriterV3::new().with_ids(ids);
    for (i, (_, text, meta)) in entries.iter().enumerate() {
        w.add(i as u32, text, *meta);
    }
    w.serialize()
}

/// Read a `.newtexts` file back: `(global id, text, meta)`.
pub fn decode_newtexts(bytes: &[u8]) -> Option<Vec<(u32, String, TermMetaV3)>> {
    let texts = TermTextsReaderV3::open(bytes)?;
    Some(texts.iter().map(|(g, t, m)| (g, t.to_string(), m)).collect())
}
