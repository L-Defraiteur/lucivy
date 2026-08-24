// Sparse vector inverted index V2 — inspired by Qdrant (Apache 2.0).
//
// Replaces the naive HashMap<u32, Vec<(u64, f32)>> with sorted PostingLists,
// batch scoring via SearchContext, and WAND-like pruning.
//
// API is unchanged: insert(), remove(), search(), search_filtered().

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

use crate::posting_list::{PostingBuilder, PostingList};
use crate::posting_list_common::PostingElementEx;
use crate::scores_memory_pool::ScoresMemoryPool;
use crate::search_context::SearchContext;

/// A sparse vector: parallel arrays of token IDs and weights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    pub fn new(indices: Vec<u32>, values: Vec<f32>) -> Self {
        assert_eq!(
            indices.len(),
            values.len(),
            "indices and values must have same length"
        );
        Self { indices, values }
    }

    pub fn nnz(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// In-memory inverted index for sparse vectors (V2).
///
/// Uses sorted PostingLists with max_next_weight for WAND pruning,
/// dimension remapping for dense Vec access, and batch scoring via SearchContext.
#[derive(Debug, Serialize, Deserialize)]
pub struct SparseIndex {
    /// Dimension remapping: global token_id → dense index into `postings`.
    dim_map: HashMap<u32, usize>,
    /// Reverse map: dense index → global token_id.
    dim_reverse: Vec<u32>,
    /// Posting lists indexed by remapped dimension.
    #[serde(serialize_with = "serialize_postings", deserialize_with = "deserialize_postings")]
    postings: Vec<PostingList>,
    /// Original vectors stored for delete/update support.
    vectors: HashMap<u64, SparseVector>,
    /// Score buffer pool (not serialized).
    #[serde(skip, default = "ScoresMemoryPool::new")]
    pool: ScoresMemoryPool,
}

// Custom serde for PostingList (which doesn't derive Serialize/Deserialize).
// We serialize as Vec<Vec<(u64, f32)>> for simplicity and compatibility.
fn serialize_postings<S: serde::Serializer>(
    postings: &[PostingList],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(postings.len()))?;
    for pl in postings {
        let elements: Vec<(u64, f32)> = pl
            .elements
            .iter()
            .map(|e| (e.record_id, e.weight))
            .collect();
        seq.serialize_element(&elements)?;
    }
    seq.end()
}

fn deserialize_postings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<PostingList>, D::Error> {
    let raw: Vec<Vec<(u64, f32)>> = Deserialize::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|elements| {
            let mut builder = PostingBuilder::new();
            for (id, weight) in elements {
                builder.add(id, weight);
            }
            builder.build()
        })
        .collect())
}

impl SparseIndex {
    pub fn new() -> Self {
        Self {
            dim_map: HashMap::new(),
            dim_reverse: Vec::new(),
            postings: Vec::new(),
            vectors: HashMap::new(),
            pool: ScoresMemoryPool::new(),
        }
    }

    /// Reconstruct from stored parts (dims + vectors, postings separate).
    pub fn from_parts(
        dim_map: HashMap<u32, usize>,
        dim_reverse: Vec<u32>,
        postings: Vec<PostingList>,
        vectors: HashMap<u64, SparseVector>,
    ) -> Self {
        Self {
            dim_map,
            dim_reverse,
            postings,
            vectors,
            pool: ScoresMemoryPool::new(),
        }
    }

    // -- Accessors for persistence layer --

    pub fn dim_map(&self) -> &HashMap<u32, usize> {
        &self.dim_map
    }

    pub fn dim_reverse(&self) -> &[u32] {
        &self.dim_reverse
    }

    pub fn postings(&self) -> &[PostingList] {
        &self.postings
    }

    pub fn postings_mut(&mut self) -> &mut Vec<PostingList> {
        &mut self.postings
    }

    pub fn vectors(&self) -> &HashMap<u64, SparseVector> {
        &self.vectors
    }

    pub fn set_vectors(&mut self, vectors: HashMap<u64, SparseVector>) {
        self.vectors = vectors;
    }

    pub fn pool(&self) -> &ScoresMemoryPool {
        &self.pool
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Get or create a remapped dimension index for the given token_id.
    fn get_or_create_dim(&mut self, token_id: u32) -> usize {
        if let Some(&idx) = self.dim_map.get(&token_id) {
            idx
        } else {
            let idx = self.postings.len();
            self.dim_map.insert(token_id, idx);
            self.dim_reverse.push(token_id);
            self.postings.push(PostingList::default());
            idx
        }
    }

    /// Get the remapped dimension index for the given token_id, if it exists.
    fn get_dim(&self, token_id: u32) -> Option<usize> {
        self.dim_map.get(&token_id).copied()
    }

    /// Insert a document's sparse vector. Replaces if node_id already exists.
    pub fn insert(&mut self, node_id: u64, vector: &SparseVector) {
        if self.vectors.contains_key(&node_id) {
            self.remove(node_id);
        }

        for (i, &token_id) in vector.indices.iter().enumerate() {
            let dim_idx = self.get_or_create_dim(token_id);
            self.postings[dim_idx].upsert(PostingElementEx::new(node_id, vector.values[i]));
        }

        self.vectors.insert(node_id, vector.clone());
    }

    /// Remove a document from the index. Returns true if it existed.
    pub fn remove(&mut self, node_id: u64) -> bool {
        if let Some(vector) = self.vectors.remove(&node_id) {
            for &token_id in &vector.indices {
                if let Some(dim_idx) = self.get_dim(token_id) {
                    self.postings[dim_idx].delete(node_id);
                }
            }
            true
        } else {
            false
        }
    }

    /// Search: top-k by dot product score, using batch scoring + WAND pruning.
    pub fn search(&self, query: &SparseVector, limit: usize) -> Vec<(u64, f32)> {
        self.search_with_filter(query, limit, &|_| true)
    }

    /// Search with allowed_ids filter.
    pub fn search_filtered(
        &self,
        query: &SparseVector,
        limit: usize,
        allowed_ids: &[u64],
    ) -> Vec<(u64, f32)> {
        if allowed_ids.is_empty() {
            return Vec::new();
        }
        let allowed: std::collections::HashSet<u64> = allowed_ids.iter().copied().collect();
        self.search_with_filter(query, limit, &|id| allowed.contains(&id))
    }

    /// Internal search with arbitrary filter predicate.
    fn search_with_filter<F: Fn(u64) -> bool>(
        &self,
        query: &SparseVector,
        limit: usize,
        filter: &F,
    ) -> Vec<(u64, f32)> {
        if query.is_empty() || self.is_empty() {
            return Vec::new();
        }

        let pooled = self.pool.get();

        // Remap query dimensions to our dense indices
        let mut remapped_indices: Vec<u32> = Vec::with_capacity(query.indices.len());
        let mut remapped_values: Vec<f32> = Vec::with_capacity(query.values.len());
        for (i, &token_id) in query.indices.iter().enumerate() {
            if let Some(&dim_idx) = self.dim_map.get(&token_id) {
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
            |dim_idx| {
                let pl = &self.postings[dim_idx as usize];
                if pl.is_empty() {
                    None
                } else {
                    Some(pl.iter())
                }
            },
            pooled,
        );

        let results = ctx.search(filter);
        results
            .into_iter()
            .map(|sp| (sp.idx, sp.score))
            .collect()
    }

    /// Clear the entire index.
    pub fn clear(&mut self) {
        self.dim_map.clear();
        self.dim_reverse.clear();
        self.postings.clear();
        self.vectors.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_vector_basics() {
        let v = SparseVector::new(vec![1, 3, 5], vec![0.5, 0.3, 0.2]);
        assert_eq!(v.nnz(), 3);
        assert!(!v.is_empty());

        let empty = SparseVector::new(vec![], vec![]);
        assert!(empty.is_empty());
    }

    #[test]
    #[should_panic(expected = "indices and values must have same length")]
    fn sparse_vector_mismatched_lengths() {
        SparseVector::new(vec![1, 2], vec![0.5]);
    }

    #[test]
    fn index_insert_and_search() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1, 2, 3], vec![0.5, 0.3, 0.2]));
        index.insert(2, &SparseVector::new(vec![2, 3, 4], vec![0.4, 0.6, 0.1]));
        index.insert(3, &SparseVector::new(vec![1, 4, 5], vec![0.9, 0.1, 0.1]));
        assert_eq!(index.len(), 3);

        let query = SparseVector::new(vec![1, 2], vec![1.0, 1.0]);
        let results = index.search(&query, 10);

        // doc1: 0.5 + 0.3 = 0.8
        // doc2: 0.4 = 0.4
        // doc3: 0.9 = 0.9
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 3);
        assert!((results[0].1 - 0.9).abs() < 1e-6);
        assert_eq!(results[1].0, 1);
        assert!((results[1].1 - 0.8).abs() < 1e-6);
        assert_eq!(results[2].0, 2);
        assert!((results[2].1 - 0.4).abs() < 1e-6);
    }

    #[test]
    fn index_remove() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]));
        index.insert(2, &SparseVector::new(vec![1, 3], vec![0.9, 0.1]));
        assert_eq!(index.len(), 2);

        assert!(index.remove(1));
        assert_eq!(index.len(), 1);
        assert!(!index.remove(1));

        let query = SparseVector::new(vec![1], vec![1.0]);
        let results = index.search(&query, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 2);
    }

    #[test]
    fn index_insert_replaces() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1], vec![0.5]));
        index.insert(1, &SparseVector::new(vec![2], vec![0.9]));
        assert_eq!(index.len(), 1);

        let query = SparseVector::new(vec![1, 2], vec![1.0, 1.0]);
        let results = index.search(&query, 10);
        assert_eq!(results.len(), 1);
        assert!((results[0].1 - 0.9).abs() < 1e-6);
    }

    #[test]
    fn index_search_limit() {
        let mut index = SparseIndex::new();
        for i in 0..100u64 {
            index.insert(i, &SparseVector::new(vec![1], vec![i as f32]));
        }
        let query = SparseVector::new(vec![1], vec![1.0]);
        let results = index.search(&query, 5);
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].0, 99);
    }

    #[test]
    fn index_search_disjoint() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1, 2], vec![1.0, 1.0]));
        let query = SparseVector::new(vec![3, 4], vec![1.0, 1.0]);
        let results = index.search(&query, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn index_empty_search() {
        let index = SparseIndex::new();
        let query = SparseVector::new(vec![1], vec![1.0]);
        assert!(index.search(&query, 10).is_empty());
    }

    #[test]
    fn index_clear() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1], vec![0.5]));
        index.insert(2, &SparseVector::new(vec![2], vec![0.3]));
        assert_eq!(index.len(), 2);

        index.clear();
        assert!(index.is_empty());
        assert!(index
            .search(&SparseVector::new(vec![1], vec![1.0]), 10)
            .is_empty());
    }

    #[test]
    fn index_remove_cleans_postings() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![42], vec![1.0]));
        index.remove(1);
        // Posting list still exists (dimension was created) but is empty
        let dim_idx = index.get_dim(42).unwrap();
        assert!(index.postings[dim_idx].is_empty());
    }

    #[test]
    fn search_filtered_basic() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]));
        index.insert(2, &SparseVector::new(vec![1, 3], vec![0.9, 0.1]));
        index.insert(3, &SparseVector::new(vec![1], vec![0.7]));

        let query = SparseVector::new(vec![1], vec![1.0]);

        let results = index.search_filtered(&query, 10, &[1, 3]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 3); // 0.7
        assert_eq!(results[1].0, 1); // 0.5

        let results = index.search_filtered(&query, 10, &[2]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 2);
    }

    #[test]
    fn persistence_compat() {
        // Verify bincode round-trip works
        let mut index = SparseIndex::new();
        index.insert(42, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]));
        index.insert(99, &SparseVector::new(vec![2, 3], vec![0.8, 0.2]));

        let data = bincode::serialize(&index).unwrap();
        let index2: SparseIndex = bincode::deserialize(&data).unwrap();

        assert_eq!(index2.len(), 2);
        let results = index2.search(&SparseVector::new(vec![2], vec![1.0]), 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 99);
        assert!((results[0].1 - 0.8).abs() < 1e-6);
    }

    #[test]
    fn dimension_remapping() {
        let mut index = SparseIndex::new();
        // Token IDs can be huge (vocab 250k) — they get remapped to dense indices
        index.insert(1, &SparseVector::new(vec![100000, 200000], vec![0.5, 0.3]));
        assert_eq!(index.postings.len(), 2);
        assert!(index.get_dim(100000).is_some());
        assert!(index.get_dim(200000).is_some());
        assert!(index.get_dim(300000).is_none());
    }

    #[test]
    fn many_documents_search() {
        let mut index = SparseIndex::new();
        // Insert 1000 docs with overlapping dimensions
        for i in 0..1000u64 {
            let token = (i % 50) as u32; // 50 unique tokens
            let weight = (i as f32) / 1000.0;
            index.insert(i, &SparseVector::new(vec![token, token + 50], vec![weight, weight * 0.5]));
        }

        let query = SparseVector::new(vec![0, 50], vec![1.0, 1.0]);
        let results = index.search(&query, 5);
        assert_eq!(results.len(), 5);
        // Top result should be doc with highest weight for tokens 0 and 50
        // Token 0: docs 0, 50, 100, ..., 950. Doc 950 has weight 0.95
        assert_eq!(results[0].0, 950);
    }
}
