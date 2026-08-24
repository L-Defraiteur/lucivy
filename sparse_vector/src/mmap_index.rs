// Flat binary mmap format for sparse vector posting lists.
//
// Format:
//   [FileHeader]                    16 bytes
//   [DimHeader × num_dims]          16 bytes × N
//   [PostingEntry × total_entries]  16 bytes × M
//
// All structs are #[repr(C)] for stable layout.

use std::collections::HashMap;
use std::path::Path;

use memmap2::Mmap;

use crate::posting_list::PostingList;
use crate::posting_list_common::{PostingElementEx, PostingListIter};
use crate::scores_memory_pool::ScoresMemoryPool;
use crate::search_context::SearchContext;

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

/// On-disk posting entry, matches PostingElementEx layout.
#[repr(C)]
#[derive(Clone)]
pub struct PostingEntry {
    pub record_id: u64,
    pub weight: f32,
    pub max_next_weight: f32,
}

/// Mmap'd posting data — read-only view of sparse.mmap.
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

    /// Get the raw posting entries for a given dimension index.
    fn posting_entries(&self, dim_idx: usize) -> &[PostingEntry] {
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

    /// Create an iterator for a given remapped dimension.
    pub fn iter(&self, dim_idx: usize) -> Option<MmapPostingListIterator<'_>> {
        let entries = self.posting_entries(dim_idx);
        if entries.is_empty() {
            None
        } else {
            Some(MmapPostingListIterator {
                entries,
                current_index: 0,
            })
        }
    }

    /// Load a dimension's posting entries into RAM PostingList.
    pub fn load_posting_list(&self, dim_idx: usize) -> PostingList {
        let entries = self.posting_entries(dim_idx);
        let elements: Vec<PostingElementEx> = entries
            .iter()
            .map(|e| PostingElementEx {
                record_id: e.record_id,
                weight: e.weight,
                max_next_weight: e.max_next_weight,
            })
            .collect();
        PostingList::from_sorted(elements)
    }
}

// ---------------------------------------------------------------------------
// MmapPostingListIterator
// ---------------------------------------------------------------------------

pub struct MmapPostingListIterator<'a> {
    entries: &'a [PostingEntry],
    current_index: usize,
}

impl PostingListIter for MmapPostingListIterator<'_> {
    #[inline]
    fn peek(&mut self) -> Option<PostingElementEx> {
        self.entries.get(self.current_index).map(|e| PostingElementEx {
            record_id: e.record_id,
            weight: e.weight,
            max_next_weight: e.max_next_weight,
        })
    }

    #[inline]
    fn last_id(&self) -> Option<u64> {
        self.entries.last().map(|e| e.record_id)
    }

    fn skip_to(&mut self, record_id: u64) -> Option<PostingElementEx> {
        if self.current_index >= self.entries.len() {
            return None;
        }
        let result = self.entries[self.current_index..]
            .binary_search_by(|e| e.record_id.cmp(&record_id));
        match result {
            Ok(offset) => {
                self.current_index += offset;
                self.peek()
            }
            Err(offset) => {
                self.current_index += offset;
                None
            }
        }
    }

    fn skip_to_end(&mut self) {
        self.current_index = self.entries.len();
    }

    #[inline]
    fn len_to_end(&self) -> usize {
        self.entries.len() - self.current_index
    }

    #[inline]
    fn current_index(&self) -> usize {
        self.current_index
    }

    fn for_each_till_id<Ctx: ?Sized>(
        &mut self,
        id: u64,
        ctx: &mut Ctx,
        mut f: impl FnMut(&mut Ctx, u64, f32),
    ) {
        for entry in &self.entries[self.current_index..] {
            if entry.record_id > id {
                break;
            }
            f(ctx, entry.record_id, entry.weight);
            self.current_index += 1;
        }
    }

    fn reliable_max_next_weight() -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Search using mmap data (no RAM postings needed)
// ---------------------------------------------------------------------------

use crate::index::SparseVector;

/// Search directly from mmap'd posting data without loading into RAM.
pub fn search_mmap<F: Fn(u64) -> bool>(
    mmap: &MmapPostingData,
    dim_map: &HashMap<u32, usize>,
    pool: &ScoresMemoryPool,
    query: &SparseVector,
    limit: usize,
    filter: &F,
) -> Vec<(u64, f32)> {
    if query.is_empty() || mmap.num_vectors() == 0 {
        return Vec::new();
    }

    let pooled = pool.get();

    // Remap query dimensions
    let mut remapped_indices: Vec<u32> = Vec::with_capacity(query.indices.len());
    let mut remapped_values: Vec<f32> = Vec::with_capacity(query.values.len());
    for (i, &token_id) in query.indices.iter().enumerate() {
        if let Some(&dim_idx) = dim_map.get(&token_id) {
            remapped_indices.push(dim_idx as u32);
            remapped_values.push(query.values[i]);
        }
    }

    if remapped_indices.is_empty() {
        return Vec::new();
    }

    let mut ctx = SearchContext::new(
        &remapped_indices,
        &remapped_values,
        limit,
        |dim_idx| mmap.iter(dim_idx as usize),
        pooled,
    );

    ctx.search(filter)
        .into_iter()
        .map(|sp| (sp.idx, sp.score))
        .collect()
}

// ---------------------------------------------------------------------------
// Write mmap format
// ---------------------------------------------------------------------------

/// Write the flat binary mmap format from in-memory posting lists.
pub fn write_mmap_file(
    path: &Path,
    postings: &[PostingList],
    num_vectors: u32,
) -> Result<(), String> {
    use std::io::Write;

    let num_dims = postings.len() as u32;
    let header_size = std::mem::size_of::<FileHeader>();
    let dim_headers_size = num_dims as usize * std::mem::size_of::<DimHeader>();
    let entries_start = header_size + dim_headers_size;

    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;

    // File header
    let header = FileHeader {
        magic: MAGIC,
        version: FORMAT_VERSION,
        num_dims,
        num_vectors,
    };
    file.write_all(as_bytes(&header))
        .map_err(|e| format!("write header: {e}"))?;

    // Compute dim headers
    let mut current_offset = entries_start;
    for pl in postings {
        let dh = DimHeader {
            offset: current_offset as u64,
            count: pl.len() as u32,
            _pad: 0,
        };
        file.write_all(as_bytes(&dh))
            .map_err(|e| format!("write dim header: {e}"))?;
        current_offset += pl.len() * std::mem::size_of::<PostingEntry>();
    }

    // Write posting entries
    for pl in postings {
        for elem in &pl.elements {
            let entry = PostingEntry {
                record_id: elem.record_id,
                weight: elem.weight,
                max_next_weight: elem.max_next_weight,
            };
            file.write_all(as_bytes(&entry))
                .map_err(|e| format!("write entry: {e}"))?;
        }
    }

    Ok(())
}

/// Reinterpret a #[repr(C)] struct as bytes.
fn as_bytes<T: Sized>(val: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(val as *const T as *const u8, std::mem::size_of::<T>()) }
}
