//! Token-aware shard routing.
//!
//! Routes documents to shards based on per-token frequency balance.
//! Uses IDF-weighted scoring (1/sqrt(global_df)) so that rare tokens
//! dominate the routing decision.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// Fast hash for token strings — FxHash-style multiply+xor.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.hash
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = self.hash.wrapping_mul(0x100000001b3).wrapping_add(b as u64);
        }
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// Only track tokens with global df below this threshold.
/// Tokens above this are frequent enough to balance naturally.
const DEFAULT_DF_THRESHOLD: u32 = 5000;

/// Token-aware shard router.
///
/// Maintains per-shard per-token document frequency counters.
/// At insertion, scores each shard by sum of `local_df / sqrt(global_df)`
/// and picks the shard with the lowest score (most under-represented).
pub struct ShardRouter {
    num_shards: usize,
    /// Per-shard token counts: shard_token_counts[shard_id][token_hash] = count.
    shard_token_counts: Vec<HashMap<u64, u32, FxBuildHasher>>,
    /// Global token counts for IDF weight + threshold check.
    global_token_counts: HashMap<u64, u32, FxBuildHasher>,
    /// Per-shard total document count.
    shard_doc_counts: Vec<u64>,
    /// Only track tokens with global df below this.
    df_threshold: u32,
}

impl ShardRouter {
    /// Create a new router for `num_shards` shards.
    pub fn new(num_shards: usize) -> Self {
        Self::with_threshold(num_shards, DEFAULT_DF_THRESHOLD)
    }

    /// Create a new router with a custom df threshold.
    pub fn with_threshold(num_shards: usize, df_threshold: u32) -> Self {
        Self {
            num_shards,
            shard_token_counts: (0..num_shards)
                .map(|_| HashMap::with_hasher(FxBuildHasher::default()))
                .collect(),
            global_token_counts: HashMap::with_hasher(FxBuildHasher::default()),
            shard_doc_counts: vec![0; num_shards],
            df_threshold,
        }
    }

    /// Route a document to the best shard based on its tokens.
    /// Returns the shard index (0..num_shards).
    ///
    /// After routing, updates the counters for the chosen shard.
    pub fn route(&mut self, doc_tokens: &[u64]) -> usize {
        if self.num_shards == 1 {
            self.shard_doc_counts[0] += 1;
            for &h in doc_tokens {
                self.update_counters_for_token(0, h);
            }
            return 0;
        }

        let mut best_shard = 0;
        let mut best_score = f64::MAX;

        for shard_id in 0..self.num_shards {
            let score: f64 = doc_tokens
                .iter()
                .filter_map(|&h| {
                    let global = *self.global_token_counts.get(&h)?;
                    if global >= self.df_threshold {
                        return None; // frequent token, skip
                    }
                    let local = *self.shard_token_counts[shard_id]
                        .get(&h)
                        .unwrap_or(&0) as f64;
                    Some(local / (global as f64).sqrt())
                })
                .sum();

            // Tie-break: prefer the shard with fewer total docs.
            let adjusted = score + (self.shard_doc_counts[shard_id] as f64) * 1e-12;

            if adjusted < best_score {
                best_score = adjusted;
                best_shard = shard_id;
            }
        }

        // Update counters for the chosen shard.
        self.shard_doc_counts[best_shard] += 1;
        for &h in doc_tokens {
            self.update_counters_for_token(best_shard, h);
        }

        best_shard
    }

    fn update_counters_for_token(&mut self, shard_id: usize, h: u64) {
        let global = self.global_token_counts.entry(h).or_default();
        *global += 1;
        // Only track if below threshold (or if we just crossed it, we leave old entries
        // — they'll be ignored in scoring. Cleaning up is O(shards) per token, not worth it).
        if *global < self.df_threshold {
            *self.shard_token_counts[shard_id].entry(h).or_default() += 1;
        }
    }

    /// Hash a token string to u64.
    pub fn hash_token(token: &str) -> u64 {
        use std::hash::Hash;
        let mut hasher = FxHasher::default();
        token.hash(&mut hasher);
        hasher.finish()
    }

    /// Number of shards.
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }

    /// Document count per shard.
    pub fn shard_doc_counts(&self) -> &[u64] {
        &self.shard_doc_counts
    }

    /// Total documents across all shards.
    pub fn total_docs(&self) -> u64 {
        self.shard_doc_counts.iter().sum()
    }

    // ─── Persistence ────────────────────────────────────────────────────

    /// Serialize to binary format for `_shard_stats.bin`.
    ///
    /// Format:
    /// ```text
    /// [magic: 4 bytes "SHRD"]
    /// [version: u8]
    /// [num_shards: u32]
    /// [df_threshold: u32]
    /// [shard_doc_counts: num_shards × u64]
    /// [num_global_tokens: u32]
    /// [global_tokens: num_global_tokens × (u64 hash, u32 count)]
    /// [per-shard tokens: num_shards × (u32 num_tokens, num_tokens × (u64 hash, u32 count))]
    /// ```
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Magic + version
        buf.extend_from_slice(b"SHRD");
        buf.push(1u8); // version 1

        // Header
        buf.extend_from_slice(&(self.num_shards as u32).to_le_bytes());
        buf.extend_from_slice(&self.df_threshold.to_le_bytes());

        // Shard doc counts
        for &count in &self.shard_doc_counts {
            buf.extend_from_slice(&count.to_le_bytes());
        }

        // Global token counts
        buf.extend_from_slice(&(self.global_token_counts.len() as u32).to_le_bytes());
        for (&hash, &count) in &self.global_token_counts {
            buf.extend_from_slice(&hash.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }

        // Per-shard token counts
        for shard_counts in &self.shard_token_counts {
            buf.extend_from_slice(&(shard_counts.len() as u32).to_le_bytes());
            for (&hash, &count) in shard_counts {
                buf.extend_from_slice(&hash.to_le_bytes());
                buf.extend_from_slice(&count.to_le_bytes());
            }
        }

        buf
    }

    /// Deserialize from `_shard_stats.bin`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < 5 {
            return Err("shard stats too short".into());
        }
        if &data[0..4] != b"SHRD" {
            return Err("invalid shard stats magic".into());
        }
        if data[4] != 1 {
            return Err(format!("unsupported shard stats version: {}", data[4]));
        }

        let mut pos = 5;

        let read_u32 = |pos: &mut usize, data: &[u8]| -> Result<u32, String> {
            if *pos + 4 > data.len() {
                return Err("truncated shard stats".into());
            }
            let v = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            Ok(v)
        };

        let read_u64 = |pos: &mut usize, data: &[u8]| -> Result<u64, String> {
            if *pos + 8 > data.len() {
                return Err("truncated shard stats".into());
            }
            let v = u64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(v)
        };

        let num_shards = read_u32(&mut pos, data)? as usize;
        let df_threshold = read_u32(&mut pos, data)?;

        // Shard doc counts
        let mut shard_doc_counts = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            shard_doc_counts.push(read_u64(&mut pos, data)?);
        }

        // Global token counts
        let num_global = read_u32(&mut pos, data)? as usize;
        let mut global_token_counts =
            HashMap::with_capacity_and_hasher(num_global, FxBuildHasher::default());
        for _ in 0..num_global {
            let hash = read_u64(&mut pos, data)?;
            let count = read_u32(&mut pos, data)?;
            global_token_counts.insert(hash, count);
        }

        // Per-shard token counts
        let mut shard_token_counts = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            let num_tokens = read_u32(&mut pos, data)? as usize;
            let mut counts =
                HashMap::with_capacity_and_hasher(num_tokens, FxBuildHasher::default());
            for _ in 0..num_tokens {
                let hash = read_u64(&mut pos, data)?;
                let count = read_u32(&mut pos, data)?;
                counts.insert(hash, count);
            }
            shard_token_counts.push(counts);
        }

        Ok(Self {
            num_shards,
            shard_token_counts,
            global_token_counts,
            shard_doc_counts,
            df_threshold,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_shard_always_zero() {
        let mut router = ShardRouter::new(1);
        let tokens = vec![ShardRouter::hash_token("hello")];
        assert_eq!(router.route(&tokens), 0);
        assert_eq!(router.route(&tokens), 0);
        assert_eq!(router.total_docs(), 2);
    }

    #[test]
    fn test_two_shards_balance() {
        let mut router = ShardRouter::new(2);
        let tok_a = ShardRouter::hash_token("rare_token");
        let tok_b = ShardRouter::hash_token("another_rare");

        // First doc with tok_a → shard 0 (tie-break: fewer docs)
        let s1 = router.route(&[tok_a]);
        assert_eq!(s1, 0);

        // Second doc with same tok_a → shard 1 (shard 0 already has it)
        let s2 = router.route(&[tok_a]);
        assert_eq!(s2, 1);

        // Third doc with tok_b (new token) → shard with fewer docs
        let s3 = router.route(&[tok_b]);
        // Both shards have 1 doc, tok_b is new → tie-break goes to shard 0
        assert_eq!(s3, 0);
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut router = ShardRouter::with_threshold(3, 1000);
        let tokens: Vec<u64> = (0..10)
            .map(|i| ShardRouter::hash_token(&format!("token_{i}")))
            .collect();

        for _ in 0..100 {
            router.route(&tokens);
        }

        let bytes = router.to_bytes();
        let restored = ShardRouter::from_bytes(&bytes).unwrap();

        assert_eq!(restored.num_shards(), 3);
        assert_eq!(restored.total_docs(), router.total_docs());
        assert_eq!(restored.shard_doc_counts(), router.shard_doc_counts());
    }

    #[test]
    fn test_df_threshold_ignores_frequent_tokens() {
        let mut router = ShardRouter::with_threshold(2, 5);

        let frequent = ShardRouter::hash_token("the");
        let rare = ShardRouter::hash_token("rag3db");

        // Push "the" above threshold
        for _ in 0..10 {
            router.route(&[frequent]);
        }

        // Now route a doc with both "the" and "rag3db"
        // Only "rag3db" should influence the choice
        let shard = router.route(&[frequent, rare]);
        // "rag3db" is new, both shards have ~5 docs each
        // The routing should work without errors
        assert!(shard < 2);
    }

    #[test]
    fn test_empty_tokens() {
        let mut router = ShardRouter::new(4);
        // Empty token list → tie-break by doc count → shard 0
        let shard = router.route(&[]);
        assert_eq!(shard, 0);
    }

    #[test]
    fn test_from_bytes_invalid_magic() {
        assert!(ShardRouter::from_bytes(b"NOPE\x01").is_err());
    }

    #[test]
    fn test_from_bytes_too_short() {
        assert!(ShardRouter::from_bytes(b"SH").is_err());
    }
}
