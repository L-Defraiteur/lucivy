//! Block-coded offset tables for the per-ordinal sidecars.
//!
//! `.sfxpost`, `.word_sfxpost`, `.sibling_v3` and `.termtexts` each kept a
//! flat `u32` per ordinal to find an ordinal's bytes — four tables of
//! 4 bytes × 5.2 M ordinals on the 10 000-file reference, 84 MB, 15 % of the
//! index, for offsets that grow by a few bytes at a time. Here the offsets
//! are cut into blocks of [`BLOCK`]: a block stores a `u32` base and each of
//! its offsets as the difference to that base, in the byte width the block
//! needs (1 to 4). A lookup is two reads: the block's position in a small
//! directory, then the offset. Written since 4 September 2026, night.
//!
//! ```text
//! [u32 num]           offsets in the table (the caller's sentinel included)
//! [u32 num_blocks]    ceil(num / BLOCK)
//! [u32 blocks_bytes]  size of the blocks region
//! [u32 × num_blocks]  position of each block in the blocks region
//! blocks region, per block:
//!   [u32 base][u8 width][k × width bytes]   k = offsets in this block,
//!                                           each `offset - base`, LE
//! ```
//!
//! [`OffsetTable`] reads either shape, so a reader written for the flat
//! table keeps its `read_offset(i)` and opens both layouts.

/// Offsets per block.
pub const BLOCK: usize = 64;

/// Encode a non-decreasing offset table.
pub fn encode(offsets: &[u32]) -> Vec<u8> {
    let num = offsets.len();
    let num_blocks = num.div_ceil(BLOCK);
    let mut dir: Vec<u32> = Vec::with_capacity(num_blocks);
    let mut blocks: Vec<u8> = Vec::with_capacity(num * 2 + num_blocks * 5);
    for chunk in offsets.chunks(BLOCK) {
        dir.push(blocks.len() as u32);
        let base = chunk[0];
        let span = chunk[chunk.len() - 1] - base;
        let width: u8 = if span == 0 { 0 } else if span < 1 << 8 { 1 } else if span < 1 << 16 { 2 } else if span < 1 << 24 { 3 } else { 4 };
        blocks.extend_from_slice(&base.to_le_bytes());
        blocks.push(width);
        for &o in chunk {
            debug_assert!(o >= base, "offsets must be non-decreasing");
            let d = (o - base).to_le_bytes();
            blocks.extend_from_slice(&d[..width as usize]);
        }
    }
    let mut out = Vec::with_capacity(12 + dir.len() * 4 + blocks.len());
    out.extend_from_slice(&(num as u32).to_le_bytes());
    out.extend_from_slice(&(num_blocks as u32).to_le_bytes());
    out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    for d in &dir {
        out.extend_from_slice(&d.to_le_bytes());
    }
    out.extend_from_slice(&blocks);
    out
}

/// Zero-copy view of a block-coded table.
#[derive(Clone, Copy)]
pub struct BlockOffsets<'a> {
    num: u32,
    dir: &'a [u8],
    blocks: &'a [u8],
}

impl<'a> BlockOffsets<'a> {
    /// Parse a table at the start of `bytes`; returns it with the number of
    /// bytes it occupies, so the caller finds what follows.
    pub fn parse(bytes: &'a [u8]) -> Option<(Self, usize)> {
        if bytes.len() < 12 {
            return None;
        }
        let num = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let num_blocks = u32::from_le_bytes(bytes[4..8].try_into().ok()?) as usize;
        let blocks_bytes = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
        let dir_end = 12 + num_blocks * 4;
        let end = dir_end + blocks_bytes;
        if bytes.len() < end || num_blocks != (num as usize).div_ceil(BLOCK) {
            return None;
        }
        Some((Self { num, dir: &bytes[12..dir_end], blocks: &bytes[dir_end..end] }, end))
    }

    /// Rebuild a view from its parts (an owning reader keeps the ranges).
    pub fn from_parts(num: u32, dir: &'a [u8], blocks: &'a [u8]) -> Self {
        Self { num, dir, blocks }
    }

    /// Number of offsets.
    pub fn len(&self) -> u32 {
        self.num
    }

    /// True when the table holds no offset.
    pub fn is_empty(&self) -> bool {
        self.num == 0
    }

    /// The `i`-th offset; `i < len()`.
    #[inline]
    pub fn get(&self, i: u32) -> u32 {
        debug_assert!(i < self.num);
        let b = (i as usize) / BLOCK;
        let r = (i as usize) % BLOCK;
        let p = u32::from_le_bytes(self.dir[b * 4..b * 4 + 4].try_into().unwrap()) as usize;
        let base = u32::from_le_bytes(self.blocks[p..p + 4].try_into().unwrap());
        let width = self.blocks[p + 4] as usize;
        if width == 0 {
            return base;
        }
        let q = p + 5 + r * width;
        let mut d = [0u8; 4];
        d[..width].copy_from_slice(&self.blocks[q..q + width]);
        base + u32::from_le_bytes(d)
    }
}

/// An offset table in either shape, behind one `get`.
#[derive(Clone, Copy)]
pub enum OffsetTable<'a> {
    /// `u32` per offset, the layout every sidecar used before 4 September 2026.
    Flat(&'a [u8]),
    /// Block-coded (this module).
    Block(BlockOffsets<'a>),
}

impl<'a> OffsetTable<'a> {
    /// The `i`-th offset.
    #[inline]
    pub fn get(&self, i: u32) -> u32 {
        match self {
            OffsetTable::Flat(bytes) => {
                let pos = i as usize * 4;
                u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap())
            }
            OffsetTable::Block(t) => t.get(i),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_width() {
        // Blocks of width 0 (all equal), 1, 2, 3 and 4 in one table.
        let mut offsets: Vec<u32> = Vec::new();
        for _ in 0..BLOCK { offsets.push(10); }
        let mut o = 10u32;
        for i in 0..BLOCK { o += (i % 3) as u32; offsets.push(o); }
        for i in 0..BLOCK { o += 500 + i as u32; offsets.push(o); }
        for _ in 0..BLOCK { o += 70_000; offsets.push(o); }
        for _ in 0..20 { o += 20_000_000; offsets.push(o); }
        let bytes = encode(&offsets);
        let (t, used) = BlockOffsets::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(t.len() as usize, offsets.len());
        for (i, &o) in offsets.iter().enumerate() {
            assert_eq!(t.get(i as u32), o, "offset {i}");
        }
        let flat: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        let f = OffsetTable::Flat(&flat);
        let b = OffsetTable::Block(t);
        for i in 0..offsets.len() as u32 {
            assert_eq!(f.get(i), b.get(i));
        }
        // 4 blocks of width ≤ 2 and one of 4 out of 276 offsets: well under 4 bytes each.
        assert!(bytes.len() < offsets.len() * 3, "{} bytes for {} offsets", bytes.len(), offsets.len());
    }

    #[test]
    fn parse_finds_what_follows() {
        let bytes = encode(&[0, 3, 7]);
        let mut file = bytes.clone();
        file.extend_from_slice(b"payload");
        let (t, used) = BlockOffsets::parse(&file).unwrap();
        assert_eq!(&file[used..], b"payload");
        assert_eq!((t.get(0), t.get(1), t.get(2)), (0, 3, 7));
        assert!(BlockOffsets::parse(&bytes[..10]).is_none());
        let empty = encode(&[]);
        let (e, _) = BlockOffsets::parse(&empty).unwrap();
        assert!(e.is_empty());
    }
}
