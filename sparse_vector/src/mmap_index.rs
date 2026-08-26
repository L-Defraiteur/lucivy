//! Flat binary mmap format for sparse-vector posting lists.
//!
//! Layout (every struct is `#[repr(C)]` for a stable layout):
//!
//! ```text
//! [FileHeader]                    16 bytes
//! [DimHeader × num_dims]          16 bytes × N
//! [PostingEntry × total_entries]  16 bytes × M
//! [Footer]                        8 bytes    (version 2)
//! ```
//!
//! **Version 2 adds the footer** — a CRC-32 of everything before it, then the
//! magic again — and the file is written to a temporary and renamed over the
//! destination. Until 3.0.5 the writer opened the destination itself and
//! wrote in place: an interrupted commit (a crash, a full disk) left a
//! truncated index that opened without complaining and answered wrong.
//! A version 1 file still opens, without those checks.
//!
//! What is verified at open is the **length** the headers imply — the cheap
//! check that catches a truncation. The CRC covers the whole file, so
//! verifying it means reading it: [`MmapPostingData::verify_checksum`] does
//! that on demand, and `LUCIVY_SPARSE_VERIFY_CRC=1` makes every open do it.
//!
//! Each entry carries a `max_next_weight` ceiling. Files written here store
//! the inclusive ceiling of the wand module (`tail_max`: the maximum weight
//! over the entry and everything after it); readers fold
//! `max(weight, max_next_weight)`, so a file whose ceiling excludes the
//! entry itself reads identically.

use std::collections::HashMap;
use std::path::Path;

use memmap2::Mmap;

use crate::index::{run_search, SparseVector};
use crate::wand::{MmapCursor, Postings};

const MAGIC: u32 = 0x53505253; // "SPRS"
/// Written by this version: a CRC-32 footer, and an atomic rename.
const FORMAT_VERSION: u32 = 2;
/// Read too: files written before 3.0.6, footerless.
const MIN_READABLE_VERSION: u32 = 1;
/// `[crc32: u32][magic: u32]`.
const FOOTER_SIZE: usize = 8;

#[repr(C)]
struct FileHeader {
    magic: u32,
    version: u32,
    num_dims: u32,
    num_vectors: u32,
}

#[repr(C)]
struct DimHeader {
    offset: u64,
    count: u32,
    _pad: u32,
}

/// On-disk posting entry.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PostingEntry {
    pub record_id: u64,
    pub weight: f32,
    /// Ceiling over the rest of the list (see the module docs).
    pub max_next_weight: f32,
}

/// Mmap'd posting data — read-only view of `sparse.mmap`.
pub struct MmapPostingData {
    mmap: Mmap,
    num_dims: usize,
    num_vectors: usize,
    /// Of the file on disk: 1 has no footer, 2 has the CRC.
    version: u32,
}

impl MmapPostingData {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| format!("cannot mmap {}: {e}", path.display()))?;

        if mmap.len() < std::mem::size_of::<FileHeader>() {
            return Err("sparse.mmap too small for header".into());
        }
        let header = unsafe { &*(mmap.as_ptr() as *const FileHeader) };
        if header.magic != MAGIC {
            return Err(format!("bad magic: {:#x}", header.magic));
        }
        if header.version < MIN_READABLE_VERSION || header.version > FORMAT_VERSION {
            return Err(format!("unsupported version: {} (this build reads {MIN_READABLE_VERSION}..={FORMAT_VERSION})",
                header.version));
        }
        let versioned = Self {
            mmap,
            num_dims: header.num_dims as usize,
            num_vectors: header.num_vectors as usize,
            version: header.version,
        };
        // The length the headers imply. A commit cut in half — a crash, a
        // full disk — used to leave a shorter file that opened fine and
        // answered from whatever it still held.
        versioned.check_length(path)?;
        if std::env::var("LUCIVY_SPARSE_VERIFY_CRC").is_ok_and(|v| v != "0") {
            versioned.verify_checksum(path)?;
        }
        Ok(versioned)
    }

    /// Bytes the file must have, from its own headers.
    fn expected_len(&self) -> usize {
        let dim_headers = std::mem::size_of::<FileHeader>()
            + self.num_dims * std::mem::size_of::<DimHeader>();
        let entries: usize = (0..self.num_dims)
            .map(|i| {
                let ptr = unsafe {
                    self.mmap.as_ptr().add(
                        std::mem::size_of::<FileHeader>() + i * std::mem::size_of::<DimHeader>(),
                    ) as *const DimHeader
                };
                unsafe { (*ptr).count as usize }
            })
            .sum();
        dim_headers + entries * std::mem::size_of::<PostingEntry>()
            + if self.version >= 2 { FOOTER_SIZE } else { 0 }
    }

    fn check_length(&self, path: &Path) -> Result<(), String> {
        // The dimension headers must be there before their counts are read.
        let headers_end = std::mem::size_of::<FileHeader>()
            + self.num_dims * std::mem::size_of::<DimHeader>();
        if self.mmap.len() < headers_end {
            return Err(format!(
                "{}: truncated — {} bytes for {} dimension headers",
                path.display(), self.mmap.len(), self.num_dims,
            ));
        }
        let expected = self.expected_len();
        if self.mmap.len() != expected {
            return Err(format!(
                "{}: truncated or corrupt — {} bytes, its headers describe {expected}",
                path.display(), self.mmap.len(),
            ));
        }
        Ok(())
    }

    /// Recompute the CRC-32 of the whole file and compare it with the
    /// footer's. Reads every byte; a version 1 file has no footer and passes.
    pub fn verify_checksum(&self, path: &Path) -> Result<(), String> {
        if self.version < 2 {
            return Ok(());
        }
        let len = self.mmap.len();
        let body = &self.mmap[..len - FOOTER_SIZE];
        let stored = u32::from_le_bytes(self.mmap[len - FOOTER_SIZE..len - 4].try_into().unwrap());
        let magic = u32::from_le_bytes(self.mmap[len - 4..].try_into().unwrap());
        if magic != MAGIC {
            return Err(format!("{}: footer magic is {magic:#x}", path.display()));
        }
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(body);
        let actual = hasher.finalize();
        if actual != stored {
            return Err(format!("{}: checksum mismatch — {actual:#x}, the file says {stored:#x}",
                path.display()));
        }
        Ok(())
    }

    pub fn num_dims(&self) -> usize {
        self.num_dims
    }

    pub fn num_vectors(&self) -> usize {
        self.num_vectors
    }

    /// The entries of a remapped dimension, sorted by record id; empty for
    /// an empty or unknown dimension.
    pub fn entries(&self, dim_idx: usize) -> &[PostingEntry] {
        if dim_idx >= self.num_dims {
            return &[];
        }
        let dim_headers_offset = std::mem::size_of::<FileHeader>();
        let dh_ptr = unsafe {
            self.mmap
                .as_ptr()
                .add(dim_headers_offset + dim_idx * std::mem::size_of::<DimHeader>())
        } as *const DimHeader;
        let dh = unsafe { &*dh_ptr };

        if dh.count == 0 {
            return &[];
        }

        let entries_ptr =
            unsafe { self.mmap.as_ptr().add(dh.offset as usize) } as *const PostingEntry;
        unsafe { std::slice::from_raw_parts(entries_ptr, dh.count as usize) }
    }

    /// Cursor over a remapped dimension, `None` when it has no postings.
    pub fn cursor(&self, dim_idx: usize) -> Option<MmapCursor<'_>> {
        MmapCursor::open(self, dim_idx as u32)
    }

    /// Load a dimension's entries into an in-RAM list, recomputing the
    /// ceilings from the weights.
    pub fn load_postings(&self, dim_idx: usize) -> Postings {
        let pairs: Vec<(u64, f32)> = self
            .entries(dim_idx)
            .iter()
            .map(|e| (e.record_id, e.weight))
            .collect();
        Postings::from_sorted_pairs(&pairs)
    }
}

// ---------------------------------------------------------------------------
// Search using mmap data (no RAM postings needed)
// ---------------------------------------------------------------------------

/// Top-`limit` search straight from the mapping, without loading anything
/// into RAM. `dim_map` translates the query's token ids into the file's
/// dimension indices.
pub fn search_mmap<F: Fn(u64) -> bool>(
    mmap: &MmapPostingData,
    dim_map: &HashMap<u32, usize>,
    query: &SparseVector,
    limit: usize,
    filter: &F,
) -> Vec<(u64, f32)> {
    if query.is_empty() || mmap.num_vectors() == 0 {
        return Vec::new();
    }
    run_search(query, dim_map, limit, filter, |dim| mmap.cursor(dim as usize))
}

/// [`search_mmap`] restricted to `allowed` ids (see
/// [`run_search_allowed`](crate::index::run_search_allowed)).
pub fn search_mmap_allowed(
    mmap: &MmapPostingData,
    dim_map: &HashMap<u32, usize>,
    query: &SparseVector,
    limit: usize,
    allowed: &[u64],
) -> Vec<(u64, f32)> {
    if query.is_empty() || mmap.num_vectors() == 0 {
        return Vec::new();
    }
    crate::index::run_search_allowed(query, dim_map, limit, allowed, |dim| mmap.cursor(dim as usize))
}

// ---------------------------------------------------------------------------
// Write mmap format
// ---------------------------------------------------------------------------

/// Write the flat binary mmap format from in-RAM posting lists, one per
/// remapped dimension.
pub fn write_mmap_file(
    path: &Path,
    postings: &[Postings],
    num_vectors: u32,
) -> Result<(), String> {
    use std::io::Write;

    let num_dims = postings.len() as u32;
    let header_size = std::mem::size_of::<FileHeader>();
    let dim_headers_size = num_dims as usize * std::mem::size_of::<DimHeader>();
    let entries_start = header_size + dim_headers_size;

    write_atomic(path, |file| {
    let mut out = CrcWriter { inner: file, hasher: crc32fast::Hasher::new() };
    let out = &mut out;
    let header = FileHeader {
        magic: MAGIC,
        version: FORMAT_VERSION,
        num_dims,
        num_vectors,
    };
    out.write_all(as_bytes(&header))
        .map_err(|e| format!("write header: {e}"))?;

    let mut current_offset = entries_start;
    for p in postings {
        let dh = DimHeader {
            offset: current_offset as u64,
            count: p.len() as u32,
            _pad: 0,
        };
        out.write_all(as_bytes(&dh))
            .map_err(|e| format!("write dim header: {e}"))?;
        current_offset += p.len() * std::mem::size_of::<PostingEntry>();
    }

    for p in postings {
        for x in p.as_slice() {
            let entry = PostingEntry {
                record_id: x.id,
                weight: x.weight,
                max_next_weight: x.tail_max,
            };
            out.write_all(as_bytes(&entry))
                .map_err(|e| format!("write entry: {e}"))?;
        }
    }
    // Footer: the CRC-32 of everything above, then the magic again.
    let crc = out.hasher.clone().finalize();
    out.inner.write_all(&crc.to_le_bytes()).map_err(|e| format!("write checksum: {e}"))?;
    out.inner.write_all(&MAGIC.to_le_bytes()).map_err(|e| format!("write footer magic: {e}"))?;
    Ok(())
    })
}

/// A writer that checksums what goes through it, so the file is not
/// buffered a second time to be hashed.
struct CrcWriter<'a> {
    inner: &'a mut std::io::BufWriter<std::fs::File>,
    hasher: crc32fast::Hasher,
}

impl std::io::Write for CrcWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write into `path` through a temporary in the same directory, flushed and
/// synced, then renamed over the destination. A crash or a full disk leaves
/// the previous file intact, never half of the new one. `File::create` on
/// the destination did the opposite: it truncated first and wrote after.
fn write_atomic(
    path: &Path,
    body: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> Result<(), String>,
) -> Result<(), String> {
    use std::io::Write;

    let name = path.file_name().ok_or_else(|| format!("{}: no file name", path.display()))?;
    let tmp = path.with_file_name(format!("{}.tmp", name.to_string_lossy()));
    let file = std::fs::File::create(&tmp)
        .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    let mut out = std::io::BufWriter::new(file);

    let written = body(&mut out).and_then(|()| {
        out.flush().map_err(|e| format!("flush {}: {e}", tmp.display()))?;
        out.get_ref().sync_all().map_err(|e| format!("sync {}: {e}", tmp.display()))
    });
    drop(out);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("cannot rename {} onto {}: {e}", tmp.display(), path.display()))?;
    // The rename itself must reach the disk, or a crash can lose it while
    // keeping the file it replaced. Best effort: not every platform lets a
    // directory be opened.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// `data` into `path`, atomically — the sidecars of a sparse shard
/// (`vectors.bin`, `dims.bin`), which `fs::write` truncated in place just
/// like the postings did. Their bytes are unchanged: no footer, no header,
/// the same bincode a previous version wrote and reads.
pub fn write_file_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    write_atomic(path, |out| out.write_all(data).map_err(|e| format!("write {}: {e}", path.display())))
}

/// Reinterpret a `#[repr(C)]` struct without padding as bytes.
fn as_bytes<T: Sized>(val: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>()) }
}
