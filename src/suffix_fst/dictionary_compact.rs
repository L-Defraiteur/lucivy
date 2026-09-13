//! Compaction of a shard dictionary's generations as a merge of streams.
//!
//! A generation is an FST over sorted keys plus a parents table, and a
//! `.termtexts` ascending by id (`dictionary.rs`). Several of them merge
//! into one without any of them in RAM: the FSTs are walked together in
//! key order (`lucivy_fst`'s union), a key held by one file has its record
//! copied byte for byte, a key held by several has their parents merged
//! and re-encoded, and the output FST is built in the same pass straight
//! to disk — the FST builder works in bounded memory. The texts are a
//! heap over the files' cursors, written in three passes so that only
//! the offset table is ever held (`termtexts_v3::write_merged`).
//!
//! Until 5 September 2026 at night a compaction read every text of every
//! live generation into a `Vec`, fed them all to `SuffixFstBuilderV3`,
//! which generated every suffix again and sorted them all — the RAM of
//! the whole dictionary's suffixes at once, for 22 million ids on the
//! kernel, and the time of a full build, at every eighth commit.
//!
//! **Which generations merge** is `choose_compaction`: past the maximum,
//! the smallest ones, enough of them to halve the count. The largest
//! generation only joins a merge once enough others have grown past it,
//! so a commit never pays for the whole dictionary again — each byte is
//! merged about as many times as the count doubles.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use common::OwnedBytes;
use lucivy_fst::{MapBuilder, OutputTable, Streamer};

use crate::directory::{Directory, TerminatingWrite};
use super::builder_v3::{decode_parent_entries_v8, encode_parent_record_v8, ParentEntryV3, SuffixFstBuilderV3};
use super::dictionary::dictionary_file_name;
use super::file_v3::{self, SfxFileReaderV3, SfxFileWriterV3};
use super::termtexts_v3::{self, TermMetaV3, TermTextsReaderV3, TermTextsWriterV3};
use super::varint::write_varint;

/// What one field's compaction did.
#[derive(Debug, Default, Clone)]
pub struct CompactReport {
    /// Generations that held the field.
    pub parts: usize,
    /// Keys in the merged FST.
    pub keys: u64,
    /// Of which held by more than one generation (parents merged).
    pub keys_merged: u64,
    /// Entries in the merged `.termtexts`.
    pub texts: u32,
    /// Bytes of the merged `.sfx`.
    pub sfx_bytes: u64,
    /// Bytes of the merged `.termtexts`.
    pub termtexts_bytes: u64,
    /// Wall time of the FST pass.
    pub fst_wall: std::time::Duration,
    /// Wall time of the texts pass.
    pub texts_wall: std::time::Duration,
}

/// The generations to merge when `live` (`(generation, bytes)`) exceeds
/// `max_generations`: the smallest ones, as many as it takes to bring the
/// count down to half the maximum (at least one). `None` under the limit.
pub fn choose_compaction(live: &[(u64, u64)], max_generations: usize) -> Option<Vec<u64>> {
    let max_generations = max_generations.max(1);
    if live.len() <= max_generations {
        return None;
    }
    let target = (max_generations / 2).max(1);
    let merge_count = live.len() - target + 1;
    let mut by_size: Vec<(u64, u64)> = live.to_vec();
    by_size.sort_by_key(|&(g, bytes)| (bytes, g));
    let mut chosen: Vec<u64> = by_size.iter().take(merge_count).map(|&(g, _)| g).collect();
    chosen.sort_unstable();
    Some(chosen)
}

/// Bytes of a generation's files for `field_ids`, as they stand in
/// `directory` (a missing file counts zero).
pub fn generation_bytes(directory: &dyn Directory, generation: u64, field_ids: &[u32]) -> u64 {
    field_ids.iter().flat_map(|&f| ["sfx", "termtexts"].into_iter().map(move |ext| (f, ext)))
        .filter_map(|(f, ext)| directory.open_read(&PathBuf::from(dictionary_file_name(generation, f, ext))).ok())
        .map(|slice| slice.num_bytes().get_bytes())
        .sum()
}

/// The `.sfx` of a generation holding exactly `entries` (ids ascending):
/// the suffix FST over their texts, values the global ids. What a segment
/// builds for the texts it minted, on its own build thread, so that the
/// commit only merges (`compact_parts`) instead of building.
pub fn generation_sfx_bytes(entries: &[(u32, String, TermMetaV3)]) -> Result<Vec<u8>, String> {
    let mut builder = SuffixFstBuilderV3::new();
    builder.set_max_ordinal(u32::MAX as u64);
    for (global, text, m) in entries {
        if m.is_word_stripped {
            let content_len = text.len().saturating_sub(m.overlap_len as usize);
            builder.add_word_stripped(&text[..content_len], &text[content_len..], *global as u64,
                m.own_len, m.sep_len, m.is_word_start);
        } else {
            builder.add_token(text, *global as u64, m.own_len, m.sep_len, m.overlap_len, m.is_word_start);
        }
    }
    let (fst, parents) = builder.build().map_err(|e| e.to_string())?;
    Ok(SfxFileWriterV3::new(fst, parents).to_bytes())
}

/// Write one field's files of a generation from entries held in RAM: the
/// FST over `entries` (ids ascending) and the texts with their ids. What a
/// commit writes for the texts it minted — small — and the reference a
/// streaming merge is checked against.
pub fn write_generation(
    directory: &dyn Directory,
    generation: u64,
    field_id: u32,
    entries: &[(u32, String, TermMetaV3)],
) -> crate::Result<()> {
    let sfx = generation_sfx_bytes(entries).map_err(|e| crate::LucivyError::SystemError(
        format!("dictionary generation {generation} field {field_id}: {e}")))?;
    let mut texts = TermTextsWriterV3::new().with_ids(entries.iter().map(|e| e.0).collect());
    for (i, (_, text, m)) in entries.iter().enumerate() {
        texts.add(i as u32, text, *m);
    }
    let termtexts = texts.serialize();
    for (ext, bytes) in [("sfx", sfx), ("termtexts", termtexts)] {
        let path = PathBuf::from(dictionary_file_name(generation, field_id, ext));
        let mut w = directory.open_write(&path)?;
        w.write_all(&bytes)?;
        w.terminate()?;
    }
    Ok(())
}

/// Delete what a generation's number may have left behind for these
/// fields: the files of a commit that crashed between writing them and
/// saving `meta.json`, temporary files included. The next commit reuses
/// the number, and a directory refuses to create a file that exists.
pub fn remove_leftovers(directory: &dyn Directory, generation: u64, field_ids: &[u32]) -> crate::Result<()> {
    for &f in field_ids {
        for ext in ["sfx", "termtexts", "sfx.fst.tmp", "sfx.parents.tmp"] {
            let path = PathBuf::from(dictionary_file_name(generation, f, ext));
            match directory.delete(&path) {
                Ok(()) | Err(crate::directory::error::DeleteError::FileDoesNotExist(_)) => {}
                Err(e) => return Err(system_error("dictionary leftover", e)),
            }
        }
    }
    Ok(())
}

fn system_error<E: std::fmt::Display>(what: &str, e: E) -> crate::LucivyError {
    crate::LucivyError::SystemError(format!("{what}: {e}"))
}

/// Merge the field's files of `generations` into generation `out`, in
/// streams (module doc). A generation without the field is skipped; none
/// with it → nothing written, `parts` 0. The caller then names `out` live
/// in place of `generations`.
pub fn compact_generations(
    directory: &dyn Directory,
    generations: &[u64],
    field_id: u32,
    out: u64,
) -> crate::Result<CompactReport> {
    let mut sfx_parts: Vec<OwnedBytes> = Vec::new();
    let mut termtexts_parts: Vec<OwnedBytes> = Vec::new();
    for &g in generations {
        let sfx = directory.open_read(&PathBuf::from(dictionary_file_name(g, field_id, "sfx")));
        let termtexts = directory.open_read(&PathBuf::from(dictionary_file_name(g, field_id, "termtexts")));
        let (Ok(sfx), Ok(termtexts)) = (sfx, termtexts) else { continue };
        sfx_parts.push(sfx.read_bytes()?);
        termtexts_parts.push(termtexts.read_bytes()?);
    }
    compact_parts(directory, sfx_parts, termtexts_parts, field_id, out)
}

/// Merge generation-shaped parts (`.sfx` and `.termtexts` bytes, paired by
/// index, ids disjoint) into generation `out`, in streams: what
/// `compact_generations` does with live generations, and what a commit
/// does with the per-segment `.newsfx` / `.newtexts` of the segments it
/// folds. No part → nothing written, `parts` 0.
pub fn compact_parts(
    directory: &dyn Directory,
    sfx_parts: Vec<OwnedBytes>,
    termtexts_parts: Vec<OwnedBytes>,
    field_id: u32,
    out: u64,
) -> crate::Result<CompactReport> {
    let mut report = CompactReport { parts: sfx_parts.len(), ..CompactReport::default() };
    if sfx_parts.is_empty() {
        return Ok(report);
    }

    // The two passes read different files and write different files: on a
    // native build they run side by side (the texts pass is a quarter of the
    // FST pass, and the fold is on the commit path). On wasm nothing is
    // spawned outside the scheduler: one after the other.
    let (sfx_report, texts_report) = merge_both(directory, &sfx_parts, &termtexts_parts, field_id, out)?;
    report.keys = sfx_report.keys;
    report.keys_merged = sfx_report.keys_merged;
    report.sfx_bytes = sfx_report.sfx_bytes;
    report.fst_wall = sfx_report.fst_wall;
    report.texts = texts_report.texts;
    report.termtexts_bytes = texts_report.termtexts_bytes;
    report.texts_wall = texts_report.texts_wall;

    if super::briques::profile::enabled() {
        eprintln!("  [dict] compaction gen {out} field {field_id}: {} parts -> {} keys ({} merged), {} texts | fst {:.0} ms | texts {:.0} ms | .sfx {:.1} MB, .termtexts {:.1} MB",
            report.parts, report.keys, report.keys_merged, report.texts,
            report.fst_wall.as_secs_f64() * 1e3, report.texts_wall.as_secs_f64() * 1e3,
            report.sfx_bytes as f64 / 1048576.0, report.termtexts_bytes as f64 / 1048576.0);
    }
    Ok(report)
}


/// A batch of keys leaving the union stage of `merge_sfx`: the key bytes
/// concatenated, one end offset per key, and where each record comes from
/// — one part (`One`, copied verbatim when its layout is ours) or several
/// (`Many`: a run of `held`, parents merged and re-encoded). One allocation
/// per field instead of one per key: with a `Vec` per key, the threads met
/// on the allocator's lock for a quarter of the pass (13 September).
struct InBatch {
    keys: Vec<u8>,
    key_end: Vec<u32>,
    src: Vec<SrcRef>,
    held: Vec<(usize, u64)>,
}

#[derive(Clone, Copy)]
enum SrcRef {
    One(usize, u64),
    Many(u32, u32),
}

/// The same batch with its records ready: a part's bytes as they are, or a
/// run of `owned` (re-encoded).
struct OutBatch<'a> {
    keys: Vec<u8>,
    key_end: Vec<u32>,
    rec: Vec<RecRef<'a>>,
    owned: Vec<u8>,
}

#[derive(Clone, Copy)]
enum RecRef<'a> {
    Verbatim(&'a [u8]),
    Owned(u32, u32),
}

impl InBatch {
    fn with_capacity(keys: usize) -> Self {
        Self { keys: Vec::with_capacity(keys * 16), key_end: Vec::with_capacity(keys), src: Vec::with_capacity(keys), held: Vec::new() }
    }
    fn len(&self) -> usize {
        self.key_end.len()
    }
    fn key(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.key_end[i - 1] as usize };
        &self.keys[start..self.key_end[i] as usize]
    }
}

/// Encoder threads of `merge_sfx`; batches go to them round-robin and
/// come back in the same order, so the FST still sees its keys sorted.
const ENCODERS: usize = 2;

/// Keys per batch between the stages of `merge_sfx`, and batches in flight
/// per channel.
const BATCH_KEYS: usize = 1024;
const PIPELINE_DEPTH: usize = 64;

/// The `.sfx` pass of `compact_parts`: FST and parents streamed to two
/// temporary files, then the container assembled from them (the header
/// needs their lengths).
fn merge_sfx(directory: &dyn Directory, sfx_parts: &[OwnedBytes], field_id: u32, out: u64) -> crate::Result<CompactReport> {
    let t = Instant::now();
    let mut report = CompactReport::default();
    let fst_tmp = PathBuf::from(dictionary_file_name(out, field_id, "sfx.fst.tmp"));
    let parents_tmp = PathBuf::from(dictionary_file_name(out, field_id, "sfx.parents.tmp"));
    {
        let readers: Vec<SfxFileReaderV3> = sfx_parts.iter()
            .map(|b| SfxFileReaderV3::open_owned(b.clone()).map_err(|e| system_error("dictionary generation", e)))
            .collect::<crate::Result<_>>()?;
        let tables: Vec<Option<OutputTable<'_>>> = readers.iter().zip(sfx_parts).map(|(r, bytes)| {
            // A verbatim copy is only right when the record layout is ours.
            (r.container_version() == file_v3::VERSION).then(|| {
                let table = r.parents_table_bytes();
                let base = bytes.as_slice().as_ptr() as usize;
                let off = table.as_ptr() as usize - base;
                OutputTable::new(&bytes.as_slice()[off..off + table.len()])
            })
        }).collect();

        let registry: usize = std::env::var("LUCIVY_FST_REGISTRY").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
        let mut fst_writer = MapBuilder::with_registry(directory.open_write(&fst_tmp)?, registry, 2)
            .map_err(|e| system_error("dictionary FST", e))?;
        let mut parents_writer = directory.open_write(&parents_tmp)?;

        // Three stages, on a native build: the union of the input FSTs (a
        // heap over their streams), the re-encoding of the keys several
        // parts hold (two threads, batches dealt round-robin and collected
        // in the same order), and the output — FST insertion and parents
        // table, serial by nature. Batches over bounded channels keep the
        // memory flat. Measured on four kernel generations (5.8 M keys):
        // 8.4 s before, 5.3 s once the merge stopped sorting twice, 4.6 s
        // pipelined; the writer's FST insertion is the floor.
        let keys_merged = std::sync::atomic::AtomicU64::new(0);
        let write = |batch: OutBatch<'_>, fst_writer: &mut MapBuilder<_>, parents_writer: &mut dyn Write,
                     parents_offset: &mut u64, len_prefix: &mut Vec<u8>, keys: &mut u64| -> crate::Result<()> {
            let mut start = 0usize;
            for (i, &end) in batch.key_end.iter().enumerate() {
                let key = &batch.keys[start..end as usize];
                start = end as usize;
                let record: &[u8] = match batch.rec[i] {
                    RecRef::Verbatim(bytes) => bytes,
                    RecRef::Owned(a, b) => &batch.owned[a as usize..b as usize],
                };
                // The table's layout (`lucivy_fst::OutputTable`): a varint
                // length before each record, the FST value its offset.
                len_prefix.clear();
                write_varint(len_prefix, record.len() as u64);
                parents_writer.write_all(len_prefix)?;
                parents_writer.write_all(record)?;
                fst_writer.insert(key, *parents_offset).map_err(|e| system_error("dictionary FST", e))?;
                *parents_offset += (len_prefix.len() + record.len()) as u64;
                *keys += 1;
            }
            Ok(())
        };
        let encode = |batch: InBatch, scratch: &mut Vec<ParentEntryV3>| -> OutBatch<'_> {
            let mut rec: Vec<RecRef<'_>> = Vec::with_capacity(batch.len());
            let mut owned: Vec<u8> = Vec::new();
            let encode_into = |owned: &mut Vec<u8>, scratch: &mut Vec<ParentEntryV3>, key: &[u8]| -> RecRef<'static> {
                let a = owned.len();
                owned.extend_from_slice(&encode_parent_record_v8(scratch, key));
                RecRef::Owned(a as u32, owned.len() as u32)
            };
            for i in 0..batch.len() {
                let key = batch.key(i);
                let r = match batch.src[i] {
                    // The random read into the part's table happens here,
                    // off the writer's thread.
                    SrcRef::One(part, value) if tables[part].is_some() => RecRef::Verbatim(tables[part].as_ref().unwrap().get(value)),
                    SrcRef::One(part, value) => {
                        scratch.clear();
                        scratch.extend(readers[part].decode_parents(value, key));
                        encode_into(&mut owned, scratch, key)
                    }
                    SrcRef::Many(a, n) => {
                        scratch.clear();
                        for &(part, value) in &batch.held[a as usize..(a + n) as usize] {
                            match &tables[part] {
                                Some(table) => scratch.extend(decode_parent_entries_v8(table.get(value), key)),
                                None => scratch.extend(readers[part].decode_parents(value, key)),
                            }
                        }
                        // The parts hold disjoint ids (generations, or the
                        // pairs of distinct segments) and each part's builder
                        // already kept one parent per (ordinal, suffix), so
                        // nothing repeats across them; the encoder orders what
                        // it is given. A sort and a dedup here, before the
                        // encoder's own sort, were 40 % of the pass on the
                        // kernel (13 September): a frequent text has tens of
                        // thousands of parents.
                        debug_assert!({
                            let mut seen: Vec<(u64, u16)> = scratch.iter().map(|p| (p.raw_ordinal, p.sti)).collect();
                            seen.sort_unstable();
                            seen.windows(2).all(|w| w[0] != w[1])
                        }, "a (ordinal, suffix) pair held by two parts");
                        keys_merged.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        encode_into(&mut owned, scratch, key)
                    }
                };
                rec.push(r);
            }
            OutBatch { keys: batch.keys, key_end: batch.key_end, rec, owned }
        };
        let produce = |emit: &mut dyn FnMut(InBatch) -> crate::Result<()>| -> crate::Result<()> {
            let mut op = lucivy_fst::map::OpBuilder::new();
            for r in &readers {
                op.push(r.fst());
            }
            let mut union = op.union();
            let mut batch = InBatch::with_capacity(BATCH_KEYS);
            while let Some((key, held)) = union.next() {
                batch.keys.extend_from_slice(key);
                batch.key_end.push(batch.keys.len() as u32);
                if held.len() == 1 {
                    batch.src.push(SrcRef::One(held[0].index, held[0].value));
                } else {
                    let a = batch.held.len() as u32;
                    batch.held.extend(held.iter().map(|iv| (iv.index, iv.value)));
                    batch.src.push(SrcRef::Many(a, held.len() as u32));
                }
                if batch.len() == BATCH_KEYS {
                    emit(std::mem::replace(&mut batch, InBatch::with_capacity(BATCH_KEYS)))?;
                }
            }
            if batch.len() > 0 {
                emit(batch)?;
            }
            Ok(())
        };

        let mut parents_offset: u64 = 0;
        let mut len_prefix: Vec<u8> = Vec::with_capacity(10);
        let mut keys = 0u64;
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::mpsc::sync_channel;
            let encode = &encode;
            let write = &write;
            let fst_writer = &mut fst_writer;
            let parents_writer = &mut parents_writer;
            std::thread::scope(|scope| -> crate::Result<()> {
                let prof = super::briques::profile::enabled();
                // One channel in and one out per encoder; the producer deals
                // batches round-robin, the writer collects them the same way,
                // so the first closed channel it meets means no batch is left.
                let mut ins = Vec::with_capacity(ENCODERS);
                let mut outs = Vec::with_capacity(ENCODERS);
                let mut encoders = Vec::with_capacity(ENCODERS);
                for _ in 0..ENCODERS {
                    let (tx1, rx1) = sync_channel::<InBatch>(PIPELINE_DEPTH);
                    let (tx2, rx2) = sync_channel::<OutBatch<'_>>(PIPELINE_DEPTH);
                    ins.push(tx1);
                    outs.push(rx2);
                    encoders.push(scope.spawn(move || {
                        let mut scratch: Vec<ParentEntryV3> = Vec::new();
                        let mut busy = std::time::Duration::ZERO;
                        for batch in rx1 {
                            let t = Instant::now();
                            let out = encode(batch, &mut scratch);
                            busy += t.elapsed();
                            if tx2.send(out).is_err() {
                                break;
                            }
                        }
                        busy
                    }));
                }
                let writer = scope.spawn(move || -> crate::Result<(u64, u64, std::time::Duration)> {
                    let mut busy = std::time::Duration::ZERO;
                    'outer: loop {
                        for rx in &outs {
                            let Ok(batch) = rx.recv() else { break 'outer };
                            let t = Instant::now();
                            write(batch, fst_writer, parents_writer, &mut parents_offset, &mut len_prefix, &mut keys)?;
                            busy += t.elapsed();
                        }
                    }
                    Ok((keys, parents_offset, busy))
                });
                let t_produce = Instant::now();
                let mut waiting = std::time::Duration::ZERO;
                let mut next = 0usize;
                let produced = produce(&mut |batch| {
                    let t = Instant::now();
                    let r = ins[next].send(batch).map_err(|_| system_error("dictionary compaction", "encoder thread gone"));
                    next = (next + 1) % ENCODERS;
                    waiting += t.elapsed();
                    r
                });
                let produce_wall = t_produce.elapsed();
                drop(ins);
                let mut encode_busy = std::time::Duration::ZERO;
                for e in encoders {
                    encode_busy += e.join().map_err(|_| system_error("dictionary compaction", "encoder thread panicked"))?;
                }
                let (k, _, write_busy) = writer.join().map_err(|_| system_error("dictionary compaction", "writer thread panicked"))??;
                produced?;
                keys = k;
                if prof {
                    eprintln!("  [dict] merge_sfx stages: union {:.2} s (of which blocked on the channel {:.2}), encode busy {:.2} s over {ENCODERS} threads, write busy {:.2} s",
                        (produce_wall - waiting).as_secs_f64(), waiting.as_secs_f64(), encode_busy.as_secs_f64(), write_busy.as_secs_f64());
                }
                Ok(())
            })?;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let mut scratch: Vec<ParentEntryV3> = Vec::new();
            produce(&mut |batch| {
                write(encode(batch, &mut scratch), &mut fst_writer, &mut parents_writer, &mut parents_offset, &mut len_prefix, &mut keys)
            })?;
        }
        report.keys = keys;
        report.keys_merged = keys_merged.load(std::sync::atomic::Ordering::Relaxed);
        let fst_out = fst_writer.into_inner().map_err(|e| system_error("dictionary FST", e))?;
        fst_out.terminate()?;
        parents_writer.terminate()?;
    }
    {
        let fst_bytes = directory.open_read(&fst_tmp)?.read_bytes()?;
        let parents_bytes = directory.open_read(&parents_tmp)?.read_bytes()?;
        if super::briques::profile::enabled() {
            eprintln!("  [dict] merge_sfx output: FST {:.1} MB, parents {:.1} MB", fst_bytes.len() as f64 / 1048576.0, parents_bytes.len() as f64 / 1048576.0);
        }
        let path = PathBuf::from(dictionary_file_name(out, field_id, "sfx"));
        let mut w = directory.open_write(&path)?;
        file_v3::write_container(&mut w, fst_bytes.as_slice(), parents_bytes.as_slice())?;
        w.terminate()?;
        report.sfx_bytes = directory.open_read(&path)?.num_bytes().get_bytes();
    }
    for tmp in [&fst_tmp, &parents_tmp] {
        directory.delete(tmp).map_err(|e| system_error("dictionary temporary file", e))?;
    }
    report.fst_wall = t.elapsed();
    Ok(report)

}

/// The `.termtexts` pass of `compact_parts`: the heap merge, three passes.
fn merge_termtexts(directory: &dyn Directory, termtexts_parts: &[OwnedBytes], field_id: u32, out: u64) -> crate::Result<CompactReport> {
    let t = Instant::now();
    let mut report = CompactReport::default();
    {
        let parts: Vec<TermTextsReaderV3<'_>> = termtexts_parts.iter()
            .map(|b| TermTextsReaderV3::open(b.as_slice()).ok_or_else(|| crate::LucivyError::SystemError(
                format!("dictionary generation field {field_id}: unreadable .termtexts"))))
            .collect::<crate::Result<_>>()?;
        let path = PathBuf::from(dictionary_file_name(out, field_id, "termtexts"));
        let mut w = directory.open_write(&path)?;
        let mut counted = CountingWrite { inner: &mut w, written: 0 };
        report.texts = termtexts_v3::write_merged(&parts, &mut counted)?;
        report.termtexts_bytes = counted.written;
        w.terminate()?;
    }
    report.texts_wall = t.elapsed();
    Ok(report)

}

#[cfg(not(target_arch = "wasm32"))]
fn merge_both(directory: &dyn Directory, sfx_parts: &[OwnedBytes], termtexts_parts: &[OwnedBytes], field_id: u32, out: u64)
    -> crate::Result<(CompactReport, CompactReport)>
{
    std::thread::scope(|scope| {
        let texts = scope.spawn(|| merge_termtexts(directory, termtexts_parts, field_id, out));
        let sfx = merge_sfx(directory, sfx_parts, field_id, out);
        let texts = texts.join().map_err(|_| system_error("dictionary texts pass", "thread panicked"))?;
        Ok((sfx?, texts?))
    })
}

#[cfg(target_arch = "wasm32")]
fn merge_both(directory: &dyn Directory, sfx_parts: &[OwnedBytes], termtexts_parts: &[OwnedBytes], field_id: u32, out: u64)
    -> crate::Result<(CompactReport, CompactReport)>
{
    Ok((merge_sfx(directory, sfx_parts, field_id, out)?, merge_termtexts(directory, termtexts_parts, field_id, out)?))
}

struct CountingWrite<'w> {
    inner: &'w mut dyn Write,
    written: u64,
}

impl Write for CountingWrite<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::RamDirectory;
    use crate::suffix_fst::dictionary::decode_newtexts;

    fn meta(own_len: u16, sep_len: u8, overlap_len: u8, is_word_start: bool, is_word_stripped: bool) -> TermMetaV3 {
        TermMetaV3 { own_len, sep_len, overlap_len, is_word_start, is_word_stripped }
    }

    /// Entries spread over three generations with interleaved ids, keys
    /// shared across generations (same lowercased text, other case or
    /// other shape), one key with more than 32 parents (a grouped
    /// record), word-stripped entries and chunks with overlaps.
    fn synthetic() -> Vec<Vec<(u32, String, TermMetaV3)>> {
        let mut all: Vec<(u32, String, TermMetaV3)> = Vec::new();
        let mut id = 0u32;
        let mut push = |all: &mut Vec<_>, text: &str, m: TermMetaV3| { all.push((id, text.to_string(), m)); id += 1; };
        let words = ["mutex_", "Mutex_", "MUTEX_", "lock(", "spin_", "sched", "printk", "return", "if (", "kernel_", "Lock(", "regist", "er_dev", "ice_"];
        for (i, w) in words.iter().enumerate() {
            let sep = w.bytes().rev().take_while(|b| !b.is_ascii_alphanumeric()).count() as u8;
            push(&mut all, w, meta(w.len() as u16, sep, 0, i % 2 == 0, false));
            let ext = format!("{w}ab");
            push(&mut all, &ext, meta(w.len() as u16, sep, 2, i % 3 == 0, false));
            let ext = format!("{w}xy");
            push(&mut all, &ext, meta(w.len() as u16, sep, 2, true, false));
            if sep > 0 {
                let content = &w[..w.len() - sep as usize];
                push(&mut all, &format!("{content}zz"), meta(w.len() as u16, sep, 2, true, true));
                push(&mut all, content, meta(w.len() as u16, sep, 0, false, true));
            } else {
                push(&mut all, w, meta(w.len() as u16, 0, 0, true, true));
            }
        }
        // 40 parents under one key: the same own text with 40 different overlaps.
        for i in 0..40u32 {
            let ov = format!("{}{}", (b'a' + (i % 26) as u8) as char, (b'a' + (i / 26) as u8) as char);
            push(&mut all, &format!("x_{ov}"), meta(2, 1, 2, i % 4 == 0, false));
        }
        // Three generations, ids interleaved: id % 3 picks the generation,
        // so every generation's own ids ascend but no generation holds a range.
        let mut gens = vec![Vec::new(), Vec::new(), Vec::new()];
        for e in all {
            gens[(e.0 % 3) as usize].push(e);
        }
        gens
    }

    fn read(dir: &dyn Directory, g: u64, field: u32, ext: &str) -> Vec<u8> {
        dir.open_read(&PathBuf::from(dictionary_file_name(g, field, ext))).unwrap().read_bytes().unwrap().to_vec()
    }

    #[test]
    fn streamed_merge_equals_the_rebuild() {
        let dir = RamDirectory::create();
        let gens = synthetic();
        for (i, entries) in gens.iter().enumerate() {
            write_generation(&dir, i as u64 + 1, 7, entries).unwrap();
        }
        let report = compact_generations(&dir, &[1, 2, 3], 7, 10).unwrap();
        assert_eq!(report.parts, 3);

        let mut all: Vec<(u32, String, TermMetaV3)> = gens.into_iter().flatten().collect();
        all.sort_by_key(|e| e.0);
        write_generation(&dir, 11, 7, &all).unwrap();

        assert_eq!(read(&dir, 10, 7, "termtexts"), read(&dir, 11, 7, "termtexts"), ".termtexts differ");
        assert_eq!(read(&dir, 10, 7, "sfx"), read(&dir, 11, 7, "sfx"), ".sfx differ");
        assert_eq!(report.texts as usize, all.len());
        assert!(report.keys_merged > 0, "the synthetic data shares keys across generations");
        assert_eq!(report.sfx_bytes as usize, read(&dir, 10, 7, "sfx").len());
        assert_eq!(report.termtexts_bytes as usize, read(&dir, 10, 7, "termtexts").len());
        for ext in ["sfx.fst.tmp", "sfx.parents.tmp"] {
            assert!(!dir.exists(&PathBuf::from(dictionary_file_name(10, 7, ext))).unwrap(), "{ext} left behind");
        }
        // Reading back: every id, text and meta.
        let bytes = read(&dir, 10, 7, "termtexts");
        let decoded = decode_newtexts(&bytes).unwrap();
        assert_eq!(decoded, all);
    }

    #[test]
    fn a_generation_without_the_field_is_skipped() {
        let dir = RamDirectory::create();
        let gens = synthetic();
        write_generation(&dir, 1, 7, &gens[0]).unwrap();
        write_generation(&dir, 2, 7, &gens[1]).unwrap();
        write_generation(&dir, 3, 9, &gens[2]).unwrap();
        let report = compact_generations(&dir, &[1, 2, 3], 7, 10).unwrap();
        assert_eq!(report.parts, 2);
        let mut two: Vec<_> = gens[0].iter().chain(&gens[1]).cloned().collect();
        two.sort_by_key(|e| e.0);
        write_generation(&dir, 11, 7, &two).unwrap();
        assert_eq!(read(&dir, 10, 7, "sfx"), read(&dir, 11, 7, "sfx"));
        let none = compact_generations(&dir, &[1, 2], 9, 12).unwrap();
        assert_eq!(none.parts, 0);
        assert!(!dir.exists(&PathBuf::from(dictionary_file_name(12, 9, "sfx"))).unwrap());
    }

    #[test]
    fn choose_compaction_takes_the_smallest_and_halves_the_count() {
        assert_eq!(choose_compaction(&[(1, 900), (2, 20)], 8), None);
        let live: Vec<(u64, u64)> = vec![(10, 900), (11, 160), (12, 140), (13, 140), (14, 140), (15, 140), (16, 140), (17, 20), (18, 20)];
        assert_eq!(choose_compaction(&live, 8), Some(vec![12, 13, 14, 15, 17, 18]));
        // Three at most (the integration tests): everything merges into one.
        assert_eq!(choose_compaction(&[(1, 5), (2, 5), (3, 5), (4, 5)], 3), Some(vec![1, 2, 3, 4]));
        assert_eq!(choose_compaction(&[(1, 5), (2, 5)], 1), Some(vec![1, 2]));
        // Ties break on the generation number: deterministic.
        assert_eq!(choose_compaction(&[(3, 5), (1, 5), (2, 5), (4, 5), (5, 5)], 4), Some(vec![1, 2, 3, 4]));
    }

    /// The compaction of a dictionary on disk, timed, with the process's
    /// peak RSS: `LUCIVY_DICT_BENCH_DIR` names an index directory holding
    /// `dict-*` files, `LUCIVY_DICT_BENCH_OUT` a scratch directory (the
    /// files are hard-linked into it), `LUCIVY_DICT_BENCH_MODE` is
    /// `stream` (default), `naive` (the rebuild of before) or `compare`
    /// (both, then the outputs byte for byte). One mode per process: the
    /// peak is the process's.
    #[test]
    #[ignore]
    #[cfg(feature = "mmap")]
    fn compaction_of_an_index_on_disk() {
        let Ok(src) = std::env::var("LUCIVY_DICT_BENCH_DIR") else { return };
        let out = std::env::var("LUCIVY_DICT_BENCH_OUT").unwrap_or_else(|_| format!("{src}-compact"));
        let mode = std::env::var("LUCIVY_DICT_BENCH_MODE").unwrap_or_else(|_| "stream".into());
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let mut gens: Vec<u64> = Vec::new();
        let mut fields: Vec<u32> = Vec::new();
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix("dict-") else { continue };
            let mut it = rest.split('.');
            let (Some(g), Some(f)) = (it.next().and_then(|g| g.parse().ok()), it.next().and_then(|f| f.parse().ok())) else { continue };
            if !gens.contains(&g) { gens.push(g); }
            if !fields.contains(&f) { fields.push(f); }
            std::fs::hard_link(entry.path(), std::path::Path::new(&out).join(&name)).unwrap();
        }
        gens.sort_unstable();
        fields.sort_unstable();
        let dir = crate::directory::MmapDirectory::open(&out).unwrap();
        let next = gens.iter().max().copied().unwrap_or(0) + 1;
        // VmHWM counts the mapped files' resident pages too (every part is
        // an mmap); the anonymous peak — what the merge allocates — is
        // sampled from RssAnon by a thread.
        let status_field = |name: &str| -> u64 {
            std::fs::read_to_string("/proc/self/status").ok()
                .and_then(|s| s.lines().find(|l| l.starts_with(name))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok())))
                .unwrap_or(0)
        };
        let anon_peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sampler = {
            let (anon_peak, stop) = (anon_peak.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let anon = status_field("RssAnon");
                    anon_peak.fetch_max(anon, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        };
        let hwm = || -> String {
            format!("VmHWM {} MB, anonymous peak {} MB",
                status_field("VmHWM") / 1024,
                anon_peak.load(std::sync::atomic::Ordering::Relaxed) / 1024)
        };
        eprintln!("generations {gens:?}, fields {fields:?}, mode {mode}, before: {}", hwm());
        for &field in &fields {
            if mode == "stream" || mode == "compare" {
                let t = Instant::now();
                let report = compact_generations(&dir, &gens, field, next).unwrap();
                eprintln!("stream field {field}: {:?} in {:.1} s, {}", report, t.elapsed().as_secs_f64(), hwm());
            }
            if mode == "naive" || mode == "compare" {
                let t = Instant::now();
                let mut entries: Vec<(u32, String, TermMetaV3)> = Vec::new();
                for &g in &gens {
                    let Ok(slice) = dir.open_read(&PathBuf::from(dictionary_file_name(g, field, "termtexts"))) else { continue };
                    entries.extend(decode_newtexts(&slice.read_bytes().unwrap()).unwrap());
                }
                entries.sort_by_key(|e| e.0);
                entries.dedup_by_key(|e| e.0);
                let t_read = t.elapsed().as_secs_f64();
                write_generation(&dir, next + 1, field, &entries).unwrap();
                eprintln!("naive field {field}: {} entries, read {t_read:.1} s, total {:.1} s, {}", entries.len(), t.elapsed().as_secs_f64(), hwm());
            }
            if mode == "compare" {
                for ext in ["sfx", "termtexts"] {
                    let a = dir.open_read(&PathBuf::from(dictionary_file_name(next, field, ext))).unwrap().read_bytes().unwrap();
                    let b = dir.open_read(&PathBuf::from(dictionary_file_name(next + 1, field, ext))).unwrap().read_bytes().unwrap();
                    eprintln!("field {field} .{ext}: stream {} bytes, naive {} bytes, {}", a.len(), b.len(),
                        if a.as_slice() == b.as_slice() { "identical" } else { "DIFFERENT" });
                    assert_eq!(a.as_slice(), b.as_slice(), "field {field} .{ext} differ");
                }
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        sampler.join().unwrap();
        eprintln!("end: {}", hwm());
    }
}
