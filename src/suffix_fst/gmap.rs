//! `.gmap` — a segment's local ordinals to the shard dictionary's global ids.
//!
//! With a shared dictionary (`sfx_version` 4, `dictionary.rs`) a segment
//! keeps its own dense ordinals for its postings, position maps and sibling
//! table, and this file says which global id each of them is. The collector
//! numbers its locals in increasing global order, so the list is sorted:
//! local → global is an index, global → local a binary search.
//!
//! ```text
//! [4 bytes] magic "GMAP"
//! [4 bytes] n (u32 LE)
//! [4 bytes × n] global id of local ordinal i (u32 LE), strictly increasing
//! ```

const MAGIC: &[u8; 4] = b"GMAP";

/// Serialize a sorted list of global ids (index = local ordinal).
pub fn encode(globals: &[u32]) -> Vec<u8> {
    debug_assert!(globals.windows(2).all(|w| w[0] < w[1]), "globals must be strictly increasing");
    let mut buf = Vec::with_capacity(8 + globals.len() * 4);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(globals.len() as u32).to_le_bytes());
    for &g in globals {
        buf.extend_from_slice(&g.to_le_bytes());
    }
    buf
}

/// Zero-copy reader.
#[derive(Clone, Copy)]
pub struct GmapReader<'a> {
    data: &'a [u8],
    n: u32,
}

impl<'a> GmapReader<'a> {
    /// Open over borrowed bytes; `None` on a bad magic or a short file.
    pub fn open(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 || &bytes[0..4] != MAGIC {
            return None;
        }
        let n = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        if bytes.len() < 8 + n as usize * 4 {
            return None;
        }
        Some(Self { data: &bytes[8..8 + n as usize * 4], n })
    }

    /// Number of local ordinals.
    pub fn len(&self) -> u32 {
        self.n
    }

    /// The first local ordinal at or after `from` whose global id is at
    /// least `target` (`len()` when none): galloping from `from`, so that a
    /// sorted list of ids is intersected with the map in time proportional
    /// to the list and the logarithm of the gaps, not to the map.
    pub fn lower_bound_from(&self, from: u32, target: u32) -> u32 {
        let n = self.n;
        if from >= n {
            return n;
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
        let (mut lo, mut hi) = (0u32, self.n);
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
        let bytes = encode(&globals);
        let r = GmapReader::open(&bytes).unwrap();
        assert_eq!(r.len(), 5);
        for (i, &g) in globals.iter().enumerate() {
            assert_eq!(r.global(i as u32), g);
            assert_eq!(r.local(g), Some(i as u32));
        }
        assert_eq!(r.local(0), None);
        assert_eq!(r.local(9), None);
        assert_eq!(r.local(5_000_000), None);
        assert!(GmapReader::open(&bytes[..6]).is_none());
        assert!(GmapReader::open(b"NOPE\0\0\0\0").is_none());
        let e = encode(&[]);
        assert!(GmapReader::open(&e).unwrap().is_empty());
    }
}
