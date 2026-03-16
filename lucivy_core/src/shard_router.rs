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

/// Default balance weight for hybrid scoring.
/// 0.0 = pure per-token, 1.0 = pure total balance.
const DEFAULT_BALANCE_WEIGHT: f64 = 0.2;

/// Token-aware shard router.
///
/// Maintains per-shard per-token document frequency counters.
/// At insertion, scores each shard by a hybrid of:
///   - per-token IDF-weighted balance (rare tokens dominate)
///   - total document balance (avoid overloading one shard)
///
/// Supports two routing modes:
///   - Full scan: score all N shards, pick the best (default)
///   - Power of two choices: score 2 random shards, pick the best (faster for large N)
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
    /// Weight for total balance in hybrid scoring (0.0 to 1.0).
    balance_weight: f64,
    /// Mapping node_id → shard_id for targeted deletes.
    node_id_to_shard: HashMap<u64, usize, FxBuildHasher>,
}

impl ShardRouter {
    /// Create a new router for `num_shards` shards with default settings.
    pub fn new(num_shards: usize) -> Self {
        Self::with_options(num_shards, DEFAULT_DF_THRESHOLD, DEFAULT_BALANCE_WEIGHT)
    }

    /// Create a new router with a custom df threshold.
    pub fn with_threshold(num_shards: usize, df_threshold: u32) -> Self {
        Self::with_options(num_shards, df_threshold, DEFAULT_BALANCE_WEIGHT)
    }

    /// Create a new router with all options.
    ///
    /// - `df_threshold`: only track tokens with global df below this (default 5000)
    /// - `balance_weight`: weight for total balance in hybrid scoring, 0.0-1.0 (default 0.2)
    pub fn with_options(num_shards: usize, df_threshold: u32, balance_weight: f64) -> Self {
        Self {
            num_shards,
            shard_token_counts: (0..num_shards)
                .map(|_| HashMap::with_hasher(FxBuildHasher::default()))
                .collect(),
            global_token_counts: HashMap::with_hasher(FxBuildHasher::default()),
            shard_doc_counts: vec![0; num_shards],
            df_threshold,
            balance_weight: balance_weight.clamp(0.0, 1.0),
            node_id_to_shard: HashMap::with_hasher(FxBuildHasher::default()),
        }
    }

    /// Route a document to the best shard based on its tokens (full scan).
    /// Returns the shard index (0..num_shards).
    ///
    /// Uses hybrid scoring: per-token IDF-weighted + total balance.
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

        let total_docs = self.shard_doc_counts.iter().sum::<u64>().max(1) as f64;

        for shard_id in 0..self.num_shards {
            let score = self.score_shard(shard_id, doc_tokens, total_docs);
            if score < best_score {
                best_score = score;
                best_shard = shard_id;
            }
        }

        self.apply_route(best_shard, doc_tokens);
        best_shard
    }

    /// Route using power of two choices: score 2 random shards, pick the best.
    ///
    /// O(tokens × 2) instead of O(tokens × N_shards). Proven quasi-optimal
    /// in distributed systems theory. Use `doc_id` as seed for deterministic
    /// shard selection.
    pub fn route_p2c(&mut self, doc_tokens: &[u64], doc_id: u64) -> usize {
        if self.num_shards <= 2 {
            return self.route(doc_tokens);
        }

        let a = (doc_id.wrapping_mul(0x9E3779B97F4A7C15) >> 32) as usize % self.num_shards;
        let b = (doc_id.wrapping_mul(0x517CC1B727220A95) >> 32) as usize % self.num_shards;
        let b = if a == b { (b + 1) % self.num_shards } else { b };

        let total_docs = self.shard_doc_counts.iter().sum::<u64>().max(1) as f64;
        let score_a = self.score_shard(a, doc_tokens, total_docs);
        let score_b = self.score_shard(b, doc_tokens, total_docs);

        let best = if score_a <= score_b { a } else { b };
        self.apply_route(best, doc_tokens);
        best
    }

    /// Score a single shard for routing (lower is better).
    ///
    /// Hybrid: `(1 - balance_weight) * per_token_score + balance_weight * shard_ratio`
    fn score_shard(&self, shard_id: usize, doc_tokens: &[u64], total_docs: f64) -> f64 {
        let token_score: f64 = doc_tokens
            .iter()
            .filter_map(|&h| {
                let global = *self.global_token_counts.get(&h)?;
                if global >= self.df_threshold {
                    return None;
                }
                let local = *self.shard_token_counts[shard_id]
                    .get(&h)
                    .unwrap_or(&0) as f64;
                Some(local / (global as f64).sqrt())
            })
            .sum();

        let shard_ratio = self.shard_doc_counts[shard_id] as f64 / total_docs;

        (1.0 - self.balance_weight) * token_score + self.balance_weight * shard_ratio
    }

    /// Apply routing decision: update doc counts and token counters.
    fn apply_route(&mut self, shard_id: usize, doc_tokens: &[u64]) {
        self.shard_doc_counts[shard_id] += 1;
        for &h in doc_tokens {
            self.update_counters_for_token(shard_id, h);
        }
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

    /// Mutable access to shard doc counts (for resync from actual index state).
    pub fn shard_doc_counts_mut(&mut self) -> &mut [u64] {
        &mut self.shard_doc_counts
    }

    /// Total documents across all shards.
    pub fn total_docs(&self) -> u64 {
        self.shard_doc_counts.iter().sum()
    }

    // ─── Node ID tracking ────────────────────────────────────────────────

    /// Record which shard a node_id was routed to.
    pub fn record_node_id(&mut self, node_id: u64, shard_id: usize) {
        self.node_id_to_shard.insert(node_id, shard_id);
    }

    /// Look up which shard holds a given node_id.
    pub fn shard_for_node_id(&self, node_id: u64) -> Option<usize> {
        self.node_id_to_shard.get(&node_id).copied()
    }

    /// Remove a node_id from the mapping (after delete).
    pub fn remove_node_id(&mut self, node_id: u64) -> Option<usize> {
        self.node_id_to_shard.remove(&node_id)
    }

    // ─── Resync from index ──────────────────────────────────────────────

    /// Rebuild token counters from the actual term dictionaries of each shard.
    ///
    /// Called after deletes+commit so the counters reflect the true state.
    /// Iterates all text fields' term dictionaries, hashes each term, and
    /// reconstructs `shard_token_counts` and `global_token_counts`.
    ///
    /// Also resyncs `shard_doc_counts` from actual searcher.num_docs().
    ///
    /// `shard_readers` provides (shard_id, text_fields, searcher) for each shard.
    pub fn resync<F>(&mut self, iter_shards: F)
    where
        F: FnOnce(&mut dyn FnMut(usize, &dyn Fn(&mut dyn FnMut(&[u8], u32)))),
    {
        // Clear all token counters.
        for counts in &mut self.shard_token_counts {
            counts.clear();
        }
        self.global_token_counts.clear();

        // Iterate each shard's terms.
        iter_shards(&mut |shard_id: usize, iter_terms: &dyn Fn(&mut dyn FnMut(&[u8], u32))| {
            iter_terms(&mut |term_bytes: &[u8], doc_freq: u32| {
                if doc_freq == 0 {
                    return;
                }
                let h = Self::hash_bytes(term_bytes);
                // Accumulate global counts.
                let global = self.global_token_counts.entry(h).or_default();
                *global += doc_freq;
                // Per-shard counts (only below threshold — we check after full iteration).
                *self.shard_token_counts[shard_id].entry(h).or_default() += doc_freq;
            });
        });

        // Prune entries above threshold.
        let threshold = self.df_threshold;
        let above_threshold: Vec<u64> = self
            .global_token_counts
            .iter()
            .filter(|(_, &count)| count >= threshold)
            .map(|(&h, _)| h)
            .collect();
        for h in &above_threshold {
            self.global_token_counts.remove(h);
            for counts in &mut self.shard_token_counts {
                counts.remove(h);
            }
        }
    }

    /// Hash raw bytes (term bytes from the term dictionary).
    pub fn hash_bytes(bytes: &[u8]) -> u64 {
        let mut hasher = FxHasher::default();
        hasher.write(bytes);
        hasher.finish()
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
        buf.push(3u8); // version 3 (added balance_weight)

        // Header
        buf.extend_from_slice(&(self.num_shards as u32).to_le_bytes());
        buf.extend_from_slice(&self.df_threshold.to_le_bytes());
        buf.extend_from_slice(&self.balance_weight.to_le_bytes());

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

        // Node ID → shard mapping
        buf.extend_from_slice(&(self.node_id_to_shard.len() as u32).to_le_bytes());
        for (&node_id, &shard_id) in &self.node_id_to_shard {
            buf.extend_from_slice(&node_id.to_le_bytes());
            buf.extend_from_slice(&(shard_id as u32).to_le_bytes());
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
        let version = data[4];
        if !(1..=3).contains(&version) {
            return Err(format!("unsupported shard stats version: {version}"));
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

        let read_f64 = |pos: &mut usize, data: &[u8]| -> Result<f64, String> {
            if *pos + 8 > data.len() {
                return Err("truncated shard stats".into());
            }
            let v = f64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(v)
        };

        let num_shards = read_u32(&mut pos, data)? as usize;
        let df_threshold = read_u32(&mut pos, data)?;
        let balance_weight = if version >= 3 {
            read_f64(&mut pos, data)?
        } else {
            DEFAULT_BALANCE_WEIGHT
        };

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

        // Node ID → shard mapping (version 2+)
        let mut node_id_to_shard = HashMap::with_hasher(FxBuildHasher::default());
        if version >= 2 && pos < data.len() {
            let num_nodes = read_u32(&mut pos, data)? as usize;
            node_id_to_shard =
                HashMap::with_capacity_and_hasher(num_nodes, FxBuildHasher::default());
            for _ in 0..num_nodes {
                let node_id = read_u64(&mut pos, data)?;
                let shard_id = read_u32(&mut pos, data)? as usize;
                node_id_to_shard.insert(node_id, shard_id);
            }
        }

        Ok(Self {
            num_shards,
            shard_token_counts,
            global_token_counts,
            shard_doc_counts,
            df_threshold,
            balance_weight,
            node_id_to_shard,
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
    fn test_power_of_two_choices() {
        let mut router = ShardRouter::new(6);
        let tokens: Vec<u64> = (0..5)
            .map(|i| ShardRouter::hash_token(&format!("term_{i}")))
            .collect();

        // Route 60 docs via p2c
        for doc_id in 0u64..60 {
            let shard = router.route_p2c(&tokens, doc_id);
            assert!(shard < 6);
        }

        // All shards should have some docs
        for &c in router.shard_doc_counts() {
            assert!(c > 0, "p2c should distribute to all shards: {:?}", router.shard_doc_counts());
        }
    }

    #[test]
    fn test_balance_weight_pure_balance() {
        // With balance_weight = 1.0, routing should be pure round-robin-like
        let mut router = ShardRouter::with_options(3, 5000, 1.0);
        let tok = ShardRouter::hash_token("same_token");

        for _ in 0..9 {
            router.route(&[tok]);
        }

        // Should be perfectly balanced: 3 docs per shard
        assert_eq!(router.shard_doc_counts(), &[3, 3, 3]);
    }

    #[test]
    fn test_balance_weight_pure_token() {
        // With balance_weight = 0.0, routing is pure per-token
        let mut router = ShardRouter::with_options(2, 5000, 0.0);
        let tok = ShardRouter::hash_token("rare");

        let s1 = router.route(&[tok]);
        let s2 = router.route(&[tok]);
        // First goes to 0, second to 1 (per-token balance)
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_serialize_roundtrip_v3() {
        let mut router = ShardRouter::with_options(4, 2000, 0.35);
        let tokens: Vec<u64> = (0..5)
            .map(|i| ShardRouter::hash_token(&format!("t_{i}")))
            .collect();
        for i in 0u64..20 {
            router.route(&tokens);
            router.record_node_id(i, (i % 4) as usize);
        }

        let bytes = router.to_bytes();
        let restored = ShardRouter::from_bytes(&bytes).unwrap();

        assert_eq!(restored.num_shards(), 4);
        assert_eq!(restored.total_docs(), 20);
        assert_eq!(restored.shard_for_node_id(3), Some(3));
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
