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
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

use super::dictionary_bloom::ScalableBloom;

use common::OwnedBytes;

use crate::directory::{Directory, FileSlice};
use crate::index::SfxDictionaryMeta;
use super::builder::SI0_PREFIX;
use super::builder_v3::{MAX_OVERLAP_BYTES, SI_STRIPPED_PREFIX};
use super::collector_v3::TokenMetaV3;
use super::file_v3::{GroupIndexSource, SfxFileReaderV3};
use super::dictionary_pidx::GroupIndex;
use super::termtexts_v3::{TermMetaV3, TermTextsReaderV3, TermTextsWriterV3};
use super::dictionary_pidx::GROUP_INDEX_EXT;

/// `IndexSettings::sfx_version` of an index with a shard dictionary: the v3
/// engine, keys and files, over global ids.
pub const DICTIONARY_SFX_VERSION: u8 = 4;

/// Cumulative cost of the per-token path (`lookup_or_mint`) since the last
/// `take`, counted only under `LUCIVY_VERBOSE`: the commit prints it next to
/// the generation's write, so that the indexing time of a dictionary index
/// splits into what the collectors pay and what the commit pays.
pub mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// `lookup_or_mint` calls.
    pub static CALLS: AtomicU64 = AtomicU64::new(0);
    /// Answered by a live generation's FST.
    pub static HITS: AtomicU64 = AtomicU64::new(0);
    /// Answered by the shared cache of found ids, verified on `.termtexts`.
    pub static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
    /// Answered by the pending texts.
    pub static PENDING_HITS: AtomicU64 = AtomicU64::new(0);
    /// Skipped the FST walk: the Bloom filter said the key was never minted.
    pub static FILTERED: AtomicU64 = AtomicU64::new(0);
    /// New ids minted (text found nowhere).
    pub static MINTS: AtomicU64 = AtomicU64::new(0);
    /// Whole `lookup_or_mint`, nanoseconds.
    pub static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    /// Of the FST walks, the decoding of the parents records (the scan of
    /// a grouped record's group headers), nanoseconds.
    pub static DECODE_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: opening the `.termtexts` readers of the generations.
    pub static OPEN_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: the FST gets and parent decodes.
    pub static FST_NS: AtomicU64 = AtomicU64::new(0);
    /// Of which: under the shared lock (pending map and counter).
    pub static LOCK_NS: AtomicU64 = AtomicU64::new(0);

    /// The counters as read (and reset) by [`take`].
    #[derive(Debug, Clone, Copy, Default)]
    pub struct Snapshot {
        /// `lookup_or_mint` calls.
        pub calls: u64,
        /// Answered by a live generation's FST.
        pub hits: u64,
        /// Answered by the shared cache, verified.
        pub cache_hits: u64,
        /// Answered by the pending texts.
        pub pending_hits: u64,
        /// New ids minted.
        pub mints: u64,
        /// FST walks skipped by the Bloom filter.
        pub filtered: u64,
        /// Whole `lookup_or_mint`, nanoseconds.
        pub total_ns: u64,
        /// Parents decoding within the FST walks, nanoseconds.
        pub decode_ns: u64,
        /// Of which: opening the `.termtexts` readers, nanoseconds.
        pub open_ns: u64,
        /// Of which: FST gets and parent decodes, nanoseconds.
        pub fst_ns: u64,
        /// Of which: under the shared lock, nanoseconds.
        pub lock_ns: u64,
    }

    /// Read and reset every counter.
    pub fn take() -> Snapshot {
        Snapshot {
            calls: CALLS.swap(0, Relaxed),
            hits: HITS.swap(0, Relaxed),
            cache_hits: CACHE_HITS.swap(0, Relaxed),
            pending_hits: PENDING_HITS.swap(0, Relaxed),
            mints: MINTS.swap(0, Relaxed),
            filtered: FILTERED.swap(0, Relaxed),
            total_ns: TOTAL_NS.swap(0, Relaxed),
            decode_ns: DECODE_NS.swap(0, Relaxed),
            open_ns: OPEN_NS.swap(0, Relaxed),
            fst_ns: FST_NS.swap(0, Relaxed),
            lock_ns: LOCK_NS.swap(0, Relaxed),
        }
    }

    impl std::fmt::Display for Snapshot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let ms = |ns: u64| ns as f64 / 1e6;
            write!(f, "{} lookups ({} from the cache, {} in a generation, {} pending, {} minted, {} FST walks skipped by the filter): {:.0} ms, of which termtexts open {:.0}, fst {:.0} (parents decoding {:.0}), lock {:.0}",
                self.calls, self.cache_hits, self.hits, self.pending_hits, self.mints, self.filtered,
                ms(self.total_ns), ms(self.open_ns), ms(self.fst_ns), ms(self.decode_ns), ms(self.lock_ns))
        }
    }
}

/// The slot an `Index` keeps its live dictionary in; a collector reads it at
/// each lookup so that a commit's swap reaches writers already running.
pub type DictionarySlot = Arc<std::sync::RwLock<Option<Arc<SfxDictionary>>>>;

/// The ids found so far, shared by every collector thread of the index:
/// a fixed table of `(hash of field + intern key, id)` pairs, two slots per
/// hash, read and written with relaxed atomics and **no lock** — two
/// writers may tear a pair, a newer text may evict an older one, and none
/// of that matters because a hit is only used once `SfxDictionary::verify`
/// has read the id's text and shape back from `.termtexts`: the cache
/// proposes, the file decides. A miss or a failed check takes the normal
/// path. Ids are stable across folds, so nothing ever needs clearing.
///
/// Why shared and why lock-free: on the whole kernel 46.9 M of 68.6 M
/// lookups per shard walked every live part's FST for a text that
/// existed, 4.1 µs each — 190 s of CPU. A cache per collector thread saw
/// 30 % of the repeats (the first hit of every text on each of the eight
/// threads still walked); the shared one locked with mutexes, measured on
/// 6 September, cost in waiting what it saved in walks.
pub struct LookupCache {
    keys: Vec<AtomicU64>,
    ids: Vec<AtomicU64>,
    /// Lookups answered by a slot (verified or not).
    pub hits: AtomicU64,
    /// Lookups with no slot for the hash.
    pub misses: AtomicU64,
    /// Hits whose id did not verify: a text minted since the last commit
    /// (not yet in any part's `.termtexts`), or a torn or evicted slot.
    pub unverified: AtomicU64,
}

/// Slots of the shared cache: 16 bytes each — 64 MB per index natively
/// (the pages are touched as they fill), 4 MB in the browser.
pub const LOOKUP_CACHE_SLOTS: usize = if cfg!(target_arch = "wasm32") { 1 << 18 } else { 1 << 22 };

impl Default for LookupCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LookupCache {
    /// An empty table of `LOOKUP_CACHE_SLOTS` slots.
    pub fn new() -> Self {
        Self {
            keys: (0..LOOKUP_CACHE_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            ids: (0..LOOKUP_CACHE_SLOTS).map(|_| AtomicU64::new(0)).collect(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            unverified: AtomicU64::new(0),
        }
    }

    #[inline]
    fn slots(key_hash: u64) -> (usize, usize, u64) {
        let h = if key_hash == 0 { 1 } else { key_hash };
        let i = (h as usize) & (LOOKUP_CACHE_SLOTS - 1);
        (i, i ^ 1, h)
    }

    /// The id last stored under this hash, if any — to be verified.
    #[inline]
    pub fn get(&self, key_hash: u64) -> Option<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        let (a, b, h) = Self::slots(key_hash);
        for i in [a, b] {
            if self.keys[i].load(Relaxed) == h {
                self.hits.fetch_add(1, Relaxed);
                return Some(self.ids[i].load(Relaxed));
            }
        }
        self.misses.fetch_add(1, Relaxed);
        None
    }

    /// Remember `id` under this hash: an empty slot of the pair if there
    /// is one, else the first — a later text may evict it, harmlessly.
    #[inline]
    pub fn insert(&self, key_hash: u64, id: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let (a, b, h) = Self::slots(key_hash);
        // An empty slot first, else the first of the pair.
        let i = if self.keys[a].load(Relaxed) == 0 || self.keys[a].load(Relaxed) == h { a }
            else if self.keys[b].load(Relaxed) == 0 { b } else { a };
        self.ids[i].store(id, Relaxed);
        self.keys[i].store(h, Relaxed);
    }

    /// `(hits, misses, unverified)` since the last call, which resets them.
    pub fn stats(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.swap(0, Relaxed), self.misses.swap(0, Relaxed), self.unverified.swap(0, Relaxed))
    }
}

/// The files of one generation of one field, by extension: the suffix FST
/// with its parents, the texts, and the derived group index of the parents
/// table (`dictionary_pidx`, written since 4.3; a generation without it
/// is read as before). One list, for the meta's file inventory, the
/// leftover removal and the size accounting alike.
pub const GENERATION_EXTENSIONS: [&str; 3] = ["sfx", "termtexts", GROUP_INDEX_EXT];

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
        let key = fst_key(text, meta.is_word_stripped, meta.own_len, meta.overlap_len);
        self.lookup_with_key(text, meta, &key)
    }

    /// `lookup` with the FST key already computed (`fst_key`) — the caller
    /// asked the Bloom filter with it first.
    pub fn lookup_with_key(&self, text: &str, meta: &TokenMetaV3, key: &[u8]) -> Option<u64> {
        let overlap = if meta.is_word_stripped {
            &text[text.len().saturating_sub(meta.overlap_len as usize)..]
        } else {
            &text[(meta.own_len as usize).min(text.len())..]
        };
        let lower_overlap = lowercase_cow(overlap);
        let mut ov_end = lower_overlap.len().min(MAX_OVERLAP_BYTES);
        while ov_end > 0 && !lower_overlap.is_char_boundary(ov_end) {
            ov_end -= 1;
        }
        let want_overlap = &lower_overlap.as_bytes()[..ov_end];
        let timed = crate::diag::is_verbose();
        let texts = self.termtexts.as_ref()?;
        let t_fst = timed.then(std::time::Instant::now);
        let _fst_guard = t_fst.map(|t| TimeInto(t, &stats::FST_NS));
        for part in self.sfx_reader.parts() {
            let Some(value) = part.fst().get(key) else { continue };
            let t_decode = timed.then(std::time::Instant::now);
            // Through the group index when the record has one: the wanted
            // group is reached by a binary search instead of a scan of
            // every group header before it (`dictionary_pidx`).
            let parents = part.parents_with_overlap(value, key, want_overlap);
            if let Some(t) = t_decode {
                stats::DECODE_NS.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            }
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

/// The key a text's own bytes sit under in the dictionary FST: the partition
/// byte, then the lowercased own text (content for a word entry) — what
/// `lookup` gets and what the Bloom filter hashes. `own_len` and
/// `overlap_len` are the entry's, as the collector or `.termtexts` carry them.
pub fn fst_key(text: &str, is_word_stripped: bool, own_len: u16, overlap_len: u8) -> Vec<u8> {
    let (partition, own) = if is_word_stripped {
        (SI_STRIPPED_PREFIX, &text[..text.len().saturating_sub(overlap_len as usize)])
    } else {
        (SI0_PREFIX, &text[..(own_len as usize).min(text.len())])
    };
    let lower_own = lowercase_cow(own);
    let mut key = Vec::with_capacity(1 + lower_own.len());
    key.push(partition);
    key.extend_from_slice(lower_own.as_bytes());
    key
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
    /// Next id per field: an atomic counter, minted with `fetch_add` under
    /// the key's stripe — one mutex for every mint of every collector
    /// thread was 1.7 s of waiting on the first commit of 2 000 kernel
    /// files (13 September). The map only grows (a field seen once stays).
    next_ids: RwLock<HashMap<u32, AtomicU64>>,
    /// The pending texts (`PendingStripe`): keys in arenas, per commit
    /// epoch, so that a commit forgets them by dropping whole epochs.
    stripes: Vec<Mutex<PendingStripe>>,
    /// The commit epoch: bumped by `begin_commit_epoch` when a commit
    /// starts flushing the writers, before any of them finalizes. A text
    /// minted before the bump belongs to a segment this commit publishes;
    /// once the commit has named the pairs, every epoch below the current
    /// one is forgotten. A text minted after the bump — by a segment cut
    /// after the flush, or by a writer finishing its documents while another
    /// already flushed — stays until the next commit: forgotten late, never
    /// early.
    epoch: AtomicU64,
    /// The group indexes built in RAM for generations without a `.pidx`
    /// (an index written before 4.3), by (field, generation): built once per
    /// process, not at every reopen of the dictionary (each commit reopens
    /// it with the new pairs).
    built_group_indexes: Mutex<HashMap<(u32, u64), GroupIndex>>,
    /// Per field, the Bloom filter over every FST key minted or folded
    /// (`dictionary_bloom`); built on first use from the live parts.
    filters: RwLock<HashMap<u32, Arc<ScalableBloom>>>,
    /// Serializes the seeding of a field's filter.
    filter_build: Mutex<()>,
    /// The ids found so far (`LookupCache`), consulted before any FST walk.
    lookup_cache: LookupCache,
}

/// 64 since 13 September 2026: with 16 collector threads (up from 8) the
/// time under the stripes' locks went from 26 to 42 s of CPU on the whole
/// kernel; 64 stripes brought it to 37 — most of it is the work under the
/// lock (the pending map, the key's `String`, the Bloom insert), not waiting.
const STRIPES: usize = 64;

impl DictionaryShared {
    fn new(next_ids: HashMap<u32, u64>) -> Self {
        Self {
            next_ids: RwLock::new(next_ids.into_iter().map(|(f, n)| (f, AtomicU64::new(n))).collect()),
            stripes: (0..STRIPES).map(|_| Mutex::new(PendingStripe::default())).collect(),
            epoch: AtomicU64::new(1),
            built_group_indexes: Mutex::new(HashMap::new()),
            filters: RwLock::new(HashMap::new()),
            filter_build: Mutex::new(()),
            lookup_cache: LookupCache::new(),
        }
    }

    /// The stripe of a (field, key) and the hash the stripe's table uses.
    fn stripe(&self, field_id: u32, key: &str) -> (&Mutex<PendingStripe>, u64) {
        let hash = pending_hash(field_id, key.as_bytes());
        (&self.stripes[(hash as usize) % STRIPES], hash)
    }
}

/// The one hash of a pending (field, key): the stripe's choice, the table's
/// probe and the table's rehash must all agree — a first version hashed the
/// key as `str` on insert and as `[u8]` on rehash (a different framing), and
/// every entry moved by a table growth was lost: 2.5 M texts minted twice on
/// the kernel, caught by the `minted` counter (13 September 2026).
#[inline]
fn pending_hash(field_id: u32, key: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    field_id.hash(&mut h);
    key.hash(&mut h);
    h.finish()
}

/// One entry of the pending texts: the key's bytes in the epoch's arena.
#[derive(Clone, Copy)]
struct PendingEntry {
    field_id: u32,
    key_start: u32,
    key_len: u32,
    id: u64,
}

/// The pending texts minted during one commit epoch: keys appended to one
/// arena, entries in a hash table over (field, key). Dropped whole when the
/// commit that publishes their segments has named the pairs — no `String`
/// per text, no `retain` over millions of entries (the old map's `retain`
/// and its `String` drops were ~0.5 s of the serial path of every kernel
/// commit, 13 September 2026).
struct PendingEpoch {
    epoch: u64,
    arena: Vec<u8>,
    table: hashbrown::HashTable<PendingEntry>,
}

impl PendingEpoch {
    fn new(epoch: u64) -> Self {
        Self { epoch, arena: Vec::new(), table: hashbrown::HashTable::new() }
    }

    fn get(&self, field_id: u32, key: &[u8], hash: u64) -> Option<u64> {
        let arena = &self.arena;
        self.table.find(hash, |e| {
            e.field_id == field_id && &arena[e.key_start as usize..(e.key_start + e.key_len) as usize] == key
        }).map(|e| e.id)
    }

    fn insert(&mut self, field_id: u32, key: &[u8], hash: u64, id: u64) {
        let key_start = self.arena.len() as u32;
        self.arena.extend_from_slice(key);
        let arena = &self.arena;
        debug_assert_eq!(hash, pending_hash(field_id, key));
        self.table.insert_unique(hash, PendingEntry { field_id, key_start, key_len: key.len() as u32, id }, |e| {
            pending_hash(e.field_id, &arena[e.key_start as usize..(e.key_start + e.key_len) as usize])
        });
    }
}

/// The pending texts of one stripe: their epochs, ascending; the last one
/// is the epoch being minted into.
#[derive(Default)]
struct PendingStripe {
    epochs: Vec<PendingEpoch>,
}

impl PendingStripe {
    fn get(&self, field_id: u32, key: &[u8], hash: u64) -> Option<u64> {
        self.epochs.iter().rev().find_map(|e| e.get(field_id, key, hash))
    }

    fn insert(&mut self, epoch: u64, field_id: u32, key: &[u8], hash: u64, id: u64) {
        if self.epochs.last().is_none_or(|e| e.epoch != epoch) {
            self.epochs.push(PendingEpoch::new(epoch));
        }
        self.epochs.last_mut().unwrap().insert(field_id, key, hash, id);
    }

    /// Drop every epoch below `epoch`.
    fn forget_before(&mut self, epoch: u64) {
        self.epochs.retain(|e| e.epoch >= epoch);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.epochs.iter().map(|e| e.table.len()).sum()
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
        let shared = match previous {
            Some(prev) => prev.shared.clone(),
            None => Arc::new(DictionaryShared::new(meta.next_ids.iter().map(|(&f, &n)| (f, n)).collect())),
        };
        let mut fields = HashMap::new();
        for &field_id in &meta.field_ids {
            let mut first_sfx = None;
            let mut sfx_parts = Vec::new();
            let mut termtexts_bytes = Vec::new();
            // The generations, then the pending segments' pairs: one part
            // each, the same shape (`SFX3` over global ids, `TTX3` with ids).
            let generation_paths = meta.generations.iter().map(|&g| (
                PathBuf::from(dictionary_file_name(g, field_id, "sfx")),
                PathBuf::from(dictionary_file_name(g, field_id, "termtexts"))));
            let pair_paths = meta.pending_segments.iter().map(|u| (
                PathBuf::from(format!("{u}.{field_id}.newsfx")),
                PathBuf::from(format!("{u}.{field_id}.newtexts"))));
            // A generation's group index (`dictionary_pidx`): its `.pidx`
            // when the file is there; a generation written before the file
            // gets one built in RAM, once per process (`built_group_indexes`,
            // the dictionary reopens at every commit); a pending pair gets
            // none (small, gone at the next fold, scanned instead).
            let index_sources = meta.generations.iter().map(|&g| Some(g))
                .chain(meta.pending_segments.iter().map(|_| None));
            let mut index_parts: Vec<GroupIndexSource> = Vec::new();
            for ((sfx_path, termtexts_path), generation) in generation_paths.chain(pair_paths).zip(index_sources) {
                let sfx = directory.open_read(&sfx_path);
                let termtexts = directory.open_read(&termtexts_path);
                let (Ok(sfx), Ok(termtexts)) = (sfx, termtexts) else {
                    if crate::diag::is_verbose() {
                        eprintln!("[dictionary] open: cannot open {} / {} (skipped)", sfx_path.display(), termtexts_path.display());
                    }
                    continue;
                };
                let (Ok(sfx_bytes), Ok(tt_bytes)) = (sfx.read_bytes(), termtexts.read_bytes()) else {
                    if crate::diag::is_verbose() {
                        eprintln!("[dictionary] open: cannot read {} / {} (skipped)", sfx_path.display(), termtexts_path.display());
                    }
                    continue;
                };
                if first_sfx.is_none() { first_sfx = Some(sfx); }
                termtexts_bytes.push(tt_bytes);
                index_parts.push(match generation {
                    None => GroupIndexSource::None,
                    Some(g) => {
                        let path = PathBuf::from(dictionary_file_name(g, field_id, GROUP_INDEX_EXT));
                        match directory.open_read(&path).ok().and_then(|f| f.read_bytes().ok()).and_then(GroupIndex::open) {
                            Some(index) => GroupIndexSource::Ready(index),
                            None => {
                                let mut built = shared.built_group_indexes.lock().unwrap();
                                match built.get(&(field_id, g)) {
                                    Some(index) => GroupIndexSource::Ready(index.clone()),
                                    None => {
                                        let Ok(reader) = SfxFileReaderV3::open_owned(sfx_bytes.clone()) else { continue };
                                        let index = GroupIndex::build(reader.parents_table_bytes());
                                        built.insert((field_id, g), index.clone());
                                        GroupIndexSource::Ready(index)
                                    }
                                }
                            }
                        }
                    }
                });
                sfx_parts.push(sfx_bytes);
            }
            let parts: Vec<(OwnedBytes, GroupIndexSource)> = sfx_parts.into_iter().zip(index_parts).collect();
            let (Some(sfx), Ok(sfx_reader)) = (first_sfx, SfxFileReaderV3::open_parts_indexed(parts)) else { continue };
            let sfx_reader = sfx_reader.with_memo(Arc::new(super::file_v3::FstMemo::new()));
            fields.insert(field_id, DictionaryField::new(sfx, sfx_reader, termtexts_bytes));
        }
        Self { meta: meta.clone(), fields, shared }
    }

    /// The dictionary of an index that has committed nothing yet: no file,
    /// no id minted.
    pub fn empty() -> Self {
        Self {
            meta: SfxDictionaryMeta { generations: Vec::new(), next_generation: 1, next_ids: Default::default(), field_ids: Vec::new(), pending_segments: Vec::new() },
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
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        field_id.hash(&mut h);
        key.hash(&mut h);
        self.lookup_or_mint_hashed(field_id, key, h.finish(), text, meta)
    }

    /// `lookup_or_mint` with the caller's hash of (field, key) — the key of
    /// the shared `LookupCache`, asked first and fed with every answer.
    pub fn lookup_or_mint_hashed(&self, field_id: u32, key: &str, key_hash: u64, text: &str, meta: &TokenMetaV3) -> (u64, bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let timed = crate::diag::is_verbose();
        let t_all = timed.then(std::time::Instant::now);
        let _all_guard = t_all.map(|t| TimeInto(t, &stats::TOTAL_NS));
        if timed { stats::CALLS.fetch_add(1, Relaxed); }
        let cache = &self.shared.lookup_cache;
        if let Some(id) = cache.get(key_hash) {
            if self.verify(field_id, id, text, meta) {
                if timed { stats::CACHE_HITS.fetch_add(1, Relaxed); }
                return (id, false);
            }
            cache.unverified.fetch_add(1, Relaxed);
        }
        let filter = self.filter(field_id);
        if filter.maybe_contains(key.as_bytes()) {
            let fst_key = fst_key(text, meta.is_word_stripped, meta.own_len, meta.overlap_len);
            if let Some(id) = self.field(field_id).and_then(|f| f.lookup_with_key(text, meta, &fst_key)) {
                if timed { stats::HITS.fetch_add(1, Relaxed); }
                cache.insert(key_hash, id);
                return (id, false);
            }
        } else if timed {
            stats::FILTERED.fetch_add(1, Relaxed);
        }
        let t_lock = timed.then(std::time::Instant::now);
        let _lock_guard = t_lock.map(|t| TimeInto(t, &stats::LOCK_NS));
        let (stripe, stripe_hash) = self.shared.stripe(field_id, key);
        let mut stripe = stripe.lock().unwrap();
        if let Some(id) = stripe.get(field_id, key.as_bytes(), stripe_hash) {
            if timed { stats::PENDING_HITS.fetch_add(1, Relaxed); }
            cache.insert(key_hash, id);
            return (id, false);
        }
        let id = {
            let ids = self.shared.next_ids.read().unwrap();
            match ids.get(&field_id) {
                Some(counter) => counter.fetch_add(1, Relaxed),
                None => {
                    drop(ids);
                    self.shared.next_ids.write().unwrap()
                        .entry(field_id).or_insert_with(|| AtomicU64::new(0))
                        .fetch_add(1, Relaxed)
                }
            }
        };
        stripe.insert(self.shared.epoch.load(Relaxed), field_id, key.as_bytes(), stripe_hash, id);
        filter.insert(key.as_bytes());
        cache.insert(key_hash, id);
        if timed { stats::MINTS.fetch_add(1, Relaxed); }
        (id, true)
    }

    /// The field's Bloom filter over the collector intern keys (text with
    /// case + shape: exactly what makes an id distinct — the FST key alone
    /// is shared by every case and shape of one lowercase text, and skipped
    /// only 1.6 M of 6.6 M walks), seeded on first use from every text the
    /// live parts hold (a writer reopening an index); a fresh index starts
    /// empty. Readers never call this.
    pub fn filter(&self, field_id: u32) -> Arc<ScalableBloom> {
        if let Some(f) = self.shared.filters.read().unwrap().get(&field_id) {
            return f.clone();
        }
        let _build = self.shared.filter_build.lock().unwrap();
        if let Some(f) = self.shared.filters.read().unwrap().get(&field_id) {
            return f.clone();
        }
        let minted = self.shared.next_ids.read().unwrap().get(&field_id).map(|c| c.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
        let filter = Arc::new(ScalableBloom::with_capacity(minted * 2));
        if let Some(texts) = self.field(field_id).and_then(|f| f.termtexts()) {
            let t = std::time::Instant::now();
            let mut n = 0u64;
            for (_, text, m) in texts.iter() {
                filter.insert(super::collector_v3::intern_key(text, m.is_word_stripped, m.own_len, m.sep_len, m.is_word_start).as_bytes());
                n += 1;
            }
            if crate::diag::is_verbose() {
                let (_, bytes) = filter.stats();
                eprintln!("[dictionary] field {field_id}: Bloom filter seeded with {n} texts in {:.0} ms ({} KB)",
                    t.elapsed().as_secs_f64() * 1e3, bytes >> 10);
            }
        }
        self.shared.filters.write().unwrap().insert(field_id, filter.clone());
        filter
    }

    /// A commit starts: the writers are about to flush. Every text minted
    /// before this call belongs to a segment the commit publishes; once it
    /// has named the pairs, `forget_committed_pending` drops them. Returns
    /// the epoch that begins.
    pub fn begin_commit_epoch(&self) -> u64 {
        self.shared.epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
    }

    /// The commit has named its pairs as live parts: forget the pending
    /// texts of every epoch before the current one (see `epoch`).
    pub fn forget_committed_pending(&self) {
        let current = self.shared.epoch.load(std::sync::atomic::Ordering::Acquire);
        for stripe in &self.shared.stripes {
            stripe.lock().unwrap().forget_before(current);
        }
    }

    /// Pending texts held, every epoch and stripe together (tests).
    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.shared.stripes.iter().map(|s| s.lock().unwrap().len()).sum()
    }

    /// The meta this dictionary was opened from.
    pub fn meta(&self) -> &SfxDictionaryMeta {
        &self.meta
    }

    /// The live generations, ascending.
    pub fn generations(&self) -> &[u64] {
        &self.meta.generations
    }

    /// True when this dictionary is made of exactly the parts `meta` names
    /// (generations and pending pairs).
    pub fn same_parts(&self, meta: &SfxDictionaryMeta) -> bool {
        self.meta.generations == meta.generations && self.meta.pending_segments == meta.pending_segments
    }

    /// The open files of a field, if this generation has them.
    pub fn field(&self, field_id: u32) -> Option<&DictionaryField> {
        self.fields.get(&field_id)
    }

    /// Whether `id` is the id of `text` with this shape in `field_id`, read
    /// from the live parts' `.termtexts` — what makes a `LookupCache` hit
    /// exact. `false` for an id no live part holds (a text minted since the
    /// last commit): the caller then takes the normal path.
    pub fn verify(&self, field_id: u32, id: u64, text: &str, meta: &TokenMetaV3) -> bool {
        if id > u32::MAX as u64 {
            return false;
        }
        let Some(texts) = self.field(field_id).and_then(|f| f.termtexts()) else { return false };
        let Some((stored, m)) = texts.entry(id as u32) else { return false };
        if stored != text || m.is_word_stripped != meta.is_word_stripped {
            return false;
        }
        if meta.is_word_stripped {
            m.own_len.saturating_sub(m.sep_len as u16) == meta.own_len.saturating_sub(meta.sep_len as u16)
        } else {
            m.own_len == meta.own_len && m.sep_len == meta.sep_len && m.is_word_start == meta.is_word_start
        }
    }

    /// The next id that would be minted, per field, as `meta.json` records it.
    pub fn next_ids(&self) -> std::collections::BTreeMap<u32, u64> {
        self.shared.next_ids.read().unwrap().iter().map(|(&f, c)| (f, c.load(std::sync::atomic::Ordering::Relaxed))).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(own_len: u16) -> TokenMetaV3 {
        TokenMetaV3 { own_len, sep_len: 0, overlap_len: 0, is_word_start: true, word_id: 0, is_word_stripped: false }
    }

    /// The pending texts live by commit epoch: a commit that starts turns
    /// the epoch, and once it has named its pairs everything minted before
    /// the turn is dropped — whole epochs, never entry by entry — while
    /// what was minted after stays.
    #[test]
    fn pending_texts_are_forgotten_by_commit_epoch() {
        let dict = SfxDictionary::empty();
        let key = |t: &str| super::super::collector_v3::intern_key(t, false, t.len() as u16, 0, true);
        let (a, minted_a) = dict.lookup_or_mint(7, &key("alpha"), "alpha", &meta(5));
        let (b, minted_b) = dict.lookup_or_mint(7, &key("beta"), "beta", &meta(4));
        assert!(minted_a && minted_b && a != b);
        assert_eq!(dict.pending_len(), 2);
        // Found while pending, from any epoch.
        assert_eq!(dict.lookup_or_mint(7, &key("alpha"), "alpha", &meta(5)), (a, false));

        let epoch = dict.begin_commit_epoch();
        assert_eq!(epoch, 2);
        let (c, minted_c) = dict.lookup_or_mint(7, &key("gamma"), "gamma", &meta(5));
        assert!(minted_c);
        assert_eq!(dict.pending_len(), 3);
        // Still found across the turn, before the commit names its pairs.
        assert_eq!(dict.lookup_or_mint(7, &key("beta"), "beta", &meta(4)), (b, false));

        dict.forget_committed_pending();
        assert_eq!(dict.pending_len(), 1, "the epoch before the turn is gone, the current one stays");
        assert_eq!(dict.lookup_or_mint(7, &key("gamma"), "gamma", &meta(5)), (c, false));
        // A second commit with nothing minted in between forgets the rest.
        dict.begin_commit_epoch();
        dict.forget_committed_pending();
        assert_eq!(dict.pending_len(), 0);
        // Ids never go backwards.
        let (d, _) = dict.lookup_or_mint(7, &key("delta"), "delta", &meta(5));
        assert!(d > c);
    }

    /// Enough texts to grow every stripe's table many times over: each one
    /// is still found afterwards, minted once. (The first version lost the
    /// entries a growth moved, see `pending_hash`.)
    #[test]
    fn pending_texts_survive_table_growth() {
        let dict = SfxDictionary::empty();
        let n = 200_000u32;
        let mut ids = Vec::with_capacity(n as usize);
        for i in 0..n {
            let text = format!("t{i}_");
            let key = super::super::collector_v3::intern_key(&text, false, text.len() as u16, 1, i % 2 == 0);
            let (id, minted) = dict.lookup_or_mint(3, &key, &text, &meta(text.len() as u16));
            assert!(minted, "{text} minted twice");
            ids.push(id);
        }
        assert_eq!(dict.pending_len(), n as usize);
        for i in 0..n {
            let text = format!("t{i}_");
            let key = super::super::collector_v3::intern_key(&text, false, text.len() as u16, 1, i % 2 == 0);
            assert_eq!(dict.lookup_or_mint(3, &key, &text, &meta(text.len() as u16)), (ids[i as usize], false), "{text} lost");
        }
        assert_eq!(dict.pending_len(), n as usize);
    }
}
