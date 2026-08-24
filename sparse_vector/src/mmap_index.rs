//! Flat binary mmap format for sparse-vector posting lists.
//!
//! Layout (every struct is `#[repr(C)]` for a stable layout):
//!
//! ```text
//! [FileHeader]                    16 bytes
//! [DimHeader × num_dims]          16 bytes × N
//! [PostingEntry × total_entries]  16 bytes × M
//! ```
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
const FORMAT_VERSION: u32 = 1;

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
        if header.version != FORMAT_VERSION {
            return Err(format!("unsupported version: {}", header.version));
        }

        Ok(Self {
            mmap,
            num_dims: header.num_dims as usize,
            num_vectors: header.num_vectors as usize,
        })
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
    use std::io::{BufWriter, Write};

    let num_dims = postings.len() as u32;
    let header_size = std::mem::size_of::<FileHeader>();
    let dim_headers_size = num_dims as usize * std::mem::size_of::<DimHeader>();
    let entries_start = header_size + dim_headers_size;

    let file = std::fs::File::create(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut out = BufWriter::new(file);

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

    out.flush().map_err(|e| format!("flush {}: {e}", path.display()))
}

/// Reinterpret a `#[repr(C)]` struct without padding as bytes.
fn as_bytes<T: Sized>(val: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>()) }
}
