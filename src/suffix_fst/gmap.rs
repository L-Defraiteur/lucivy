//! `.gmap` — a segment's local ordinals to the shard dictionary's global ids.
//!
//! With a shared dictionary (`sfx_version` 4, `dictionary.rs`) a segment
//! keeps its own dense ordinals for its postings, position maps and sibling
//! table, and this file says which global id each of them is. The collector
//! numbers its locals in increasing global order, so the list is sorted:
//! local → global is an index, global → local a binary search.
//!
//! Layout 2 (`GMP2`, 5 September 2026):
//!
//! ```text
//! [4 bytes] magic "GMP2"
//! [4 bytes] n (u32 LE)
//! [2 bytes] longest word-stripped content of the segment, in bytes
//!           (u16 LE; 0xFFFF = unknown) — what `.termtexts` STATS holds
//!           for a segment with its own dictionary, so that a relaxed
//!           query skips the chunk chains per segment, not per shard
//! [2 bytes] reserved (0)
//! [4 bytes × n] global id of local ordinal i (u32 LE), strictly increasing
//! [4 bytes × ceil(n / 64)] the first id of every block of 64: a global
//!           → local lookup or a galloping intersection lands on the
//!           block with one small search, then reads one block, instead
//!           of a binary search over the whole map missing the cache at
//!           every step (`keep_in_segment`, 16 ms of CPU per query on
//!           30 000 files)
//! ```
//!
//! Layout 1 (`GMAP`) had the magic, `n` and the ids only; still read.

const MAGIC: &[u8; 4] = b"GMAP";
const MAGIC2: &[u8; 4] = b"GMP2";
/// Ids per block of the head index.
const BLOCK: u32 = 64;
const UNKNOWN_WORD: u16 = 0xFFFF;

/// Serialize a sorted list of global ids (index = local ordinal), with the
/// segment's longest word-stripped content when known.
pub fn encode(globals: &[u32], max_word_content_len: Option<u16>) -> Vec<u8> {
    debug_assert!(globals.windows(2).all(|w| w[0] < w[1]), "globals must be strictly increasing");
    let n = globals.len();
    let blocks = n.div_ceil(BLOCK as usize);
    let mut buf = Vec::with_capacity(12 + n * 4 + blocks * 4);
    buf.extend_from_slice(MAGIC2);
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    buf.extend_from_slice(&max_word_content_len.map_or(UNKNOWN_WORD, |m| m.min(UNKNOWN_WORD - 1)).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    for &g in globals {
        buf.extend_from_slice(&g.to_le_bytes());
    }
    for b in 0..blocks {
        buf.extend_from_slice(&globals[b * BLOCK as usize].to_le_bytes());
    }
    buf
}

/// Zero-copy reader.
#[derive(Clone, Copy)]
pub struct GmapReader<'a> {
    data: &'a [u8],
    n: u32,
    /// Layout 2: the first id of every block of `BLOCK` ids.
    heads: &'a [u8],
    max_word: Option<u16>,
}

impl<'a> GmapReader<'a> {
    /// Open over borrowed bytes; `None` on a bad magic or a short file.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let n = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let ids_len = n as usize * 4;
        if &bytes[0..4] == MAGIC {
            if bytes.len() < 8 + ids_len {
                return None;
            }
            return Some(Self { data: &bytes[8..8 + ids_len], n, heads: &[], max_word: None });
        }
        if &bytes[0..4] != MAGIC2 || bytes.len() < 12 {
            return None;
        }
        let blocks = (n as usize).div_ceil(BLOCK as usize);
        if bytes.len() < 12 + ids_len + blocks * 4 {
            return None;
        }
        let mw = u16::from_le_bytes(bytes[8..10].try_into().ok()?);
        Some(Self {
            data: &bytes[12..12 + ids_len],
            n,
            heads: &bytes[12 + ids_len..12 + ids_len + blocks * 4],
            max_word: (mw != UNKNOWN_WORD).then_some(mw),
        })
    }

    /// Number of local ordinals.
    pub fn len(&self) -> u32 {
        self.n
    }

    /// The segment's longest word-stripped content, when the file says.
    pub fn max_word_content_len(&self) -> Option<u16> {
        self.max_word
    }

    #[inline]
    fn head(&self, block: usize) -> u32 {
        let p = block * 4;
        u32::from_le_bytes(self.heads[p..p + 4].try_into().unwrap())
    }

    /// The block whose ids may hold `target`: the last block whose head is
    /// at most `target`, searched among the blocks from `first_block` on.
    #[inline]
    fn block_of(&self, first_block: usize, target: u32) -> usize {
        let blocks = self.heads.len() / 4;
        let (mut lo, mut hi) = (first_block, blocks);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.head(mid) <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }

    /// The first local ordinal at or after `from` whose global id is at
    /// least `target` (`len()` when none): with the block heads, one search
    /// over the heads then one over a block; without, galloping from
    /// `from` (exponential then binary), so that a sorted list of ids is
    /// intersected with the map in time proportional to the list and the
    /// logarithm of the gaps, not to the map.
    pub fn lower_bound_from(&self, from: u32, target: u32) -> u32 {
        let n = self.n;
        if from >= n {
            return n;
        }
        if !self.heads.is_empty() {
            // Sorted probes mostly land in the block of the previous one:
            // stay there when the next head is past the target.
            let fb = (from / BLOCK) as usize;
            let blocks = self.heads.len() / 4;
            let b = if fb + 1 >= blocks || self.head(fb + 1) > target {
                fb
            } else {
                self.block_of(fb + 1, target)
            };
            let lo0 = ((b as u32) * BLOCK).max(from);
            let hi0 = (((b as u32) + 1) * BLOCK).min(n);
            let (mut lo, mut hi) = (lo0, hi0);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if self.global(mid) < target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            return lo;
        }
        let mut lo = from;
        let mut hi = from;
        let mut step = 1u32;
        loop {
            if hi >= n {
                hi = n;
                break;
            }
            if self.global(hi) >= target {
                break;
            }
            lo = hi + 1;
            hi = hi.saturating_add(step);
            step = step.saturating_mul(2);
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.global(mid) < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// True when the segment has no ordinal.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Global id of a local ordinal.
    #[inline]
    pub fn global(&self, local: u32) -> u32 {
        let p = local as usize * 4;
        u32::from_le_bytes(self.data[p..p + 4].try_into().unwrap())
    }

    /// Local ordinal of a global id, if the segment has it.
    #[inline]
    pub fn local(&self, global: u32) -> Option<u32> {
        if self.n == 0 {
            return None;
        }
        let (mut lo, mut hi) = if self.heads.is_empty() {
            (0u32, self.n)
        } else {
            let b = self.block_of(0, global) as u32;
            (b * BLOCK, ((b + 1) * BLOCK).min(self.n))
        };
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let g = self.global(mid);
            if g == global {
                return Some(mid);
            } else if g < global {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        None
    }

    /// Every global id, in local order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.n).map(move |i| self.global(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_lookup() {
        let globals = [3u32, 7, 8, 100, 4_000_000];
        let bytes = encode(&globals, Some(12));
        let r = GmapReader::open(&bytes).unwrap();
        assert_eq!(r.len(), 5);
        assert_eq!(r.max_word_content_len(), Some(12));
        for (i, &g) in globals.iter().enumerate() {
            assert_eq!(r.global(i as u32), g);
            assert_eq!(r.local(g), Some(i as u32));
        }
        assert_eq!(r.local(0), None);
        assert_eq!(r.local(9), None);
        assert_eq!(r.local(5_000_000), None);
        assert!(GmapReader::open(&bytes[..6]).is_none());
        assert!(GmapReader::open(b"NOPE\0\0\0\0").is_none());
        let e = encode(&[], None);
        assert!(GmapReader::open(&e).unwrap().is_empty());
        assert_eq!(GmapReader::open(&e).unwrap().max_word_content_len(), None);
    }

    /// Layout 1 still opens, without heads or statistic.
    #[test]
    fn layout_1_still_opens() {
        let globals = [1u32, 5, 9];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GMAP");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        for g in globals { bytes.extend_from_slice(&g.to_le_bytes()); }
        let r = GmapReader::open(&bytes).unwrap();
        assert_eq!(r.max_word_content_len(), None);
        assert_eq!(r.local(5), Some(1));
        assert_eq!(r.lower_bound_from(0, 6), 2);
    }

    /// Block heads and galloping agree with a plain search on a map wider
    /// than several blocks, for every from/target shape.
    #[test]
    fn lower_bound_with_heads_matches_linear() {
        let globals: Vec<u32> = (0..1000u32).map(|i| i * 7 + (i % 3)).collect();
        let bytes = encode(&globals, None);
        let r = GmapReader::open(&bytes).unwrap();
        for &from in &[0u32, 1, 63, 64, 65, 500, 999] {
            for target in (0..3100u32).step_by(13) {
                let expect = (from..1000).find(|&j| globals[j as usize] >= target).unwrap_or(1000);
                assert_eq!(r.lower_bound_from(from, target), expect, "from {from} target {target}");
            }
        }
        for (i, &g) in globals.iter().enumerate() {
            assert_eq!(r.local(g), Some(i as u32));
            assert_eq!(r.local(g + 1).map(|l| globals[l as usize]), globals.get(i + 1).filter(|&&x| x == g + 1).copied());
        }
    }
}
