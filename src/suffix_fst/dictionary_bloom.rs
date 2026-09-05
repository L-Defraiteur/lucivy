//! A Bloom filter over the FST keys of a shard dictionary, in front of the
//! per-token lookup (`dictionary::SfxDictionary::lookup_or_mint`).
//!
//! Why: on 30 000 kernel files, 6.6 M of the 15 M lookups are texts no
//! generation has — and each one still walked every live FST (a `get` per
//! generation, ~3.5 µs) to find nothing: 23 of the 35 s of the per-token
//! path. The filter answers "surely not" in ~50 ns; a "maybe" (every real
//! hit, plus ~1 % false alarms at 10 bits per key) walks as before. The FSTs
//! stay the truth: the filter never changes an answer, only skips walks
//! that would have found nothing.
//!
//! Fed at **mint** time (the text will be in a generation later; until then
//! the pending table has it, and a "no" still goes through that table under
//! its lock, so two writers cannot mint one text twice). On a writer that
//! opens an existing index, seeded once from every live part's `.termtexts`
//! before the first mint (`SfxDictionary::filter`). Readers never build it.
//!
//! Scalable: a chain of filters, each twice the previous capacity; a query
//! asks each (few). Bits are atomic words, so inserts and queries run on
//! every collector thread without a lock.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::RwLock;

/// Bits per key: ~1 % false positives with `HASHES` probes.
const BITS_PER_KEY: u64 = 10;
const HASHES: u64 = 7;
/// Capacity of the first filter of a chain, in keys (320 KB of bits).
const FIRST_CAPACITY: u64 = 1 << 18;

struct Bloom {
    bits: Vec<AtomicU64>,
    nbits: u64,
    capacity: u64,
    inserted: AtomicU64,
}

impl Bloom {
    fn with_capacity(capacity: u64) -> Self {
        let nbits = (capacity * BITS_PER_KEY).max(64);
        let words = nbits.div_ceil(64) as usize;
        Self { bits: (0..words).map(|_| AtomicU64::new(0)).collect(), nbits, capacity, inserted: AtomicU64::new(0) }
    }

    #[inline]
    fn probes(&self, h1: u64, h2: u64) -> impl Iterator<Item = (usize, u64)> + '_ {
        (0..HASHES).map(move |i| {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % self.nbits;
            ((bit / 64) as usize, 1u64 << (bit % 64))
        })
    }

    #[inline]
    fn maybe_contains(&self, h1: u64, h2: u64) -> bool {
        self.probes(h1, h2).all(|(w, m)| self.bits[w].load(Relaxed) & m != 0)
    }

    #[inline]
    fn insert(&self, h1: u64, h2: u64) {
        for (w, m) in self.probes(h1, h2) {
            self.bits[w].fetch_or(m, Relaxed);
        }
        self.inserted.fetch_add(1, Relaxed);
    }

    fn full(&self) -> bool {
        self.inserted.load(Relaxed) >= self.capacity
    }

    fn bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

/// The chain (module doc).
pub struct ScalableBloom {
    chain: RwLock<Vec<Bloom>>,
}

impl Default for ScalableBloom {
    fn default() -> Self {
        Self::with_capacity(FIRST_CAPACITY)
    }
}

impl ScalableBloom {
    /// A chain whose first filter takes `capacity` keys; sized from the
    /// texts already minted when it is known.
    pub fn with_capacity(capacity: u64) -> Self {
        Self { chain: RwLock::new(vec![Bloom::with_capacity(capacity.max(FIRST_CAPACITY))]) }
    }

    /// Two hashes of `key` for double hashing: one pass of FxHash, then a
    /// 64-bit finalizer for the second.
    #[inline]
    fn hashes(key: &[u8]) -> (u64, u64) {
        use std::hash::Hasher;
        let mut h = rustc_hash::FxHasher::default();
        h.write(key);
        let h1 = h.finish();
        let mut x = h1 ^ 0x9E37_79B9_7F4A_7C15;
        x = (x ^ (x >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
        x = (x ^ (x >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        x ^= x >> 33;
        (h1, x | 1)
    }

    /// False when no filter of the chain has `key`: surely never inserted.
    #[inline]
    pub fn maybe_contains(&self, key: &[u8]) -> bool {
        let (h1, h2) = Self::hashes(key);
        self.chain.read().unwrap().iter().any(|b| b.maybe_contains(h1, h2))
    }

    /// Insert `key` into the last filter, opening a larger one when full.
    pub fn insert(&self, key: &[u8]) {
        let (h1, h2) = Self::hashes(key);
        {
            let chain = self.chain.read().unwrap();
            let last = chain.last().expect("a chain has a filter");
            if !last.full() {
                last.insert(h1, h2);
                return;
            }
        }
        let mut chain = self.chain.write().unwrap();
        if chain.last().is_some_and(|b| b.full()) {
            let next = chain.last().map(|b| b.capacity * 2).unwrap_or(FIRST_CAPACITY);
            chain.push(Bloom::with_capacity(next));
        }
        chain.last().expect("a chain has a filter").insert(h1, h2);
    }

    /// Keys inserted, and bytes held.
    pub fn stats(&self) -> (u64, usize) {
        let chain = self.chain.read().unwrap();
        (chain.iter().map(|b| b.inserted.load(Relaxed)).sum(), chain.iter().map(|b| b.bytes()).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_a_false_negative_and_few_false_positives() {
        let f = ScalableBloom::with_capacity(1000);
        let keys: Vec<Vec<u8>> = (0..600_000u32).map(|i| format!("\x00key-{i}").into_bytes()).collect();
        for k in &keys { f.insert(k); }
        assert!(keys.iter().all(|k| f.maybe_contains(k)), "a false negative");
        let absent = (0..100_000u32).filter(|i| f.maybe_contains(format!("\x00nope-{i}").as_bytes())).count();
        assert!(absent < 3_000, "{absent} false positives in 100 000 (chain of filters, ~1 % each)");
        let (n, bytes) = f.stats();
        assert_eq!(n, 600_000);
        assert!(bytes < 4 << 20, "{bytes} bytes for 600 000 keys");
    }
}
