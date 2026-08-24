//! In-RAM sparse-vector inverted index.
//!
//! Token ids (which can be sparse and large) are remapped to dense
//! dimension indices; each dimension owns one [`Postings`] list of the
//! `wand` module, and the original vectors are kept so a record can be
//! removed or replaced. Searches run through [`wand::search_with`] with a
//! per-thread [`Scratch`].
//!
//! # Zero weights
//!
//! A coordinate whose weight is exactly `0.0` contributes nothing to any
//! dot product, so it is not indexed: the record does not appear in that
//! dimension's postings and a query on that dimension alone does not
//! return it. The stored vector keeps the coordinate as given.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::wand::{self, DimId, PostingCursor, Postings, Scratch, SearchOptions, TopKSink};

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

thread_local! {
    /// Window buffers reused by every search on this thread.
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::new());
}

/// Top-`limit` search with the crate's default options and the thread's
/// scratch buffers. Token ids of `query` are translated through `dim_map`;
/// unknown ones are ignored, and `cursors` receives the dense dimension
/// indices. Repeated token ids in the query are summed (see
/// [`wand::search_with`]).
pub(crate) fn run_search<C, F, R>(
    query: &SparseVector,
    dim_map: &HashMap<u32, usize>,
    limit: usize,
    filter: F,
    cursors: R,
) -> Vec<(u64, f32)>
where
    C: PostingCursor,
    F: Fn(u64) -> bool,
    R: FnMut(DimId) -> Option<C>,
{
    let lanes: Vec<(DimId, f32)> = query
        .indices
        .iter()
        .zip(&query.values)
        .filter_map(|(token, &w)| dim_map.get(token).map(|&dim| (dim as DimId, w)))
        .collect();
    if lanes.is_empty() {
        return Vec::new();
    }
    SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        wand::search_with(
            &lanes,
            filter,
            cursors,
            TopKSink::new(limit),
            SearchOptions::default(),
            &mut scratch,
        )
    })
}

/// In-memory inverted index for sparse vectors.
#[derive(Debug, Serialize, Deserialize)]
pub struct SparseIndex {
    /// Dimension remapping: global token_id → dense index into `postings`.
    dim_map: HashMap<u32, usize>,
    /// Reverse map: dense index → global token_id.
    dim_reverse: Vec<u32>,
    /// Posting lists indexed by remapped dimension.
    #[serde(with = "postings_serde")]
    postings: Vec<Postings>,
    /// Original vectors stored for delete/update support.
    vectors: HashMap<u64, SparseVector>,
}

/// Posting lists travel as `Vec<Vec<(id, weight)>>`: the ceilings are
/// derived data, and the shape is the one older `sparse.bin` files carry.
mod postings_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::wand::Postings;

    pub fn serialize<S: Serializer>(postings: &[Postings], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(postings.iter().map(|p| {
            p.as_slice()
                .iter()
                .map(|x| (x.id, x.weight))
                .collect::<Vec<(u64, f32)>>()
        }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Postings>, D::Error> {
        let lists: Vec<Vec<(u64, f32)>> = Deserialize::deserialize(d)?;
        Ok(lists.into_iter().map(Postings::from_pairs).collect())
    }
}

impl Default for SparseIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SparseIndex {
    pub fn new() -> Self {
        Self {
            dim_map: HashMap::new(),
            dim_reverse: Vec::new(),
            postings: Vec::new(),
            vectors: HashMap::new(),
        }
    }

    /// Reconstruct from stored parts (dims + vectors, postings separate).
    pub fn from_parts(
        dim_map: HashMap<u32, usize>,
        dim_reverse: Vec<u32>,
        postings: Vec<Postings>,
        vectors: HashMap<u64, SparseVector>,
    ) -> Self {
        Self {
            dim_map,
            dim_reverse,
            postings,
            vectors,
        }
    }

    // -- Accessors for the persistence layer --

    pub fn dim_map(&self) -> &HashMap<u32, usize> {
        &self.dim_map
    }

    pub fn dim_reverse(&self) -> &[u32] {
        &self.dim_reverse
    }

    pub fn postings(&self) -> &[Postings] {
        &self.postings
    }

    pub fn postings_mut(&mut self) -> &mut Vec<Postings> {
        &mut self.postings
    }

    pub fn vectors(&self) -> &HashMap<u64, SparseVector> {
        &self.vectors
    }

    pub fn set_vectors(&mut self, vectors: HashMap<u64, SparseVector>) {
        self.vectors = vectors;
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Dense index of `token_id`, allocating one on first sight.
    fn get_or_create_dim(&mut self, token_id: u32) -> usize {
        if let Some(&idx) = self.dim_map.get(&token_id) {
            return idx;
        }
        let idx = self.postings.len();
        self.dim_map.insert(token_id, idx);
        self.dim_reverse.push(token_id);
        self.postings.push(Postings::new());
        idx
    }

    /// Dense index of `token_id`, if it has been seen.
    fn get_dim(&self, token_id: u32) -> Option<usize> {
        self.dim_map.get(&token_id).copied()
    }

    /// Index a record's vector, replacing any previous vector under the
    /// same id. Zero weights are not indexed (see the module docs).
    pub fn insert(&mut self, node_id: u64, vector: &SparseVector) {
        if self.vectors.contains_key(&node_id) {
            self.remove(node_id);
        }

        for (&token_id, &weight) in vector.indices.iter().zip(&vector.values) {
            if weight == 0.0 {
                continue;
            }
            let dim_idx = self.get_or_create_dim(token_id);
            self.postings[dim_idx].upsert(node_id, weight);
        }

        self.vectors.insert(node_id, vector.clone());
    }

    /// Remove a record. Returns true if it existed.
    pub fn remove(&mut self, node_id: u64) -> bool {
        let Some(vector) = self.vectors.remove(&node_id) else {
            return false;
        };
        for &token_id in &vector.indices {
            if let Some(dim_idx) = self.get_dim(token_id) {
                self.postings[dim_idx].delete(node_id);
            }
        }
        true
    }

    /// Top-`limit` records by dot product with `query`, score descending
    /// then id ascending.
    pub fn search(&self, query: &SparseVector, limit: usize) -> Vec<(u64, f32)> {
        self.search_with_filter(query, limit, &|_| true)
    }

    /// [`search`](Self::search) restricted to `allowed_ids`.
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

    fn search_with_filter<F: Fn(u64) -> bool>(
        &self,
        query: &SparseVector,
        limit: usize,
        filter: &F,
    ) -> Vec<(u64, f32)> {
        if query.is_empty() || self.is_empty() {
            return Vec::new();
        }
        run_search(query, &self.dim_map, limit, filter, |dim| {
            self.postings
                .get(dim as usize)
                .filter(|p| !p.is_empty())
                .map(Postings::cursor)
        })
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
        for p in index.postings() {
            p.check_invariants().unwrap();
        }
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
        // The dimension survives, its posting list is empty.
        let dim_idx = index.get_dim(42).unwrap();
        assert!(index.postings[dim_idx].is_empty());
    }

    #[test]
    fn zero_weights_are_not_indexed() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![1, 2], vec![0.0, 0.5]));
        index.insert(2, &SparseVector::new(vec![1], vec![0.0]));
        assert_eq!(index.len(), 2, "the records themselves are kept");
        // Dimension 1 only ever saw zeros: nothing to search there.
        assert!(index
            .search(&SparseVector::new(vec![1], vec![1.0]), 10)
            .is_empty());
        let hits = index.search(&SparseVector::new(vec![1, 2], vec![1.0, 1.0]), 10);
        assert_eq!(hits, vec![(1, 0.5)]);
        // Replacing a record with a non-zero weight on that dimension works.
        index.insert(2, &SparseVector::new(vec![1], vec![0.7]));
        let hits = index.search(&SparseVector::new(vec![1], vec![1.0]), 10);
        assert_eq!(hits, vec![(2, 0.7)]);
        assert!(index.remove(1));
        assert!(index.remove(2));
        assert!(index.is_empty());
    }

    #[test]
    fn duplicate_query_dimensions_are_summed() {
        let mut index = SparseIndex::new();
        index.insert(1, &SparseVector::new(vec![7], vec![0.5]));
        let once = index.search(&SparseVector::new(vec![7], vec![1.5]), 10);
        let twice = index.search(&SparseVector::new(vec![7, 7], vec![1.0, 0.5]), 10);
        assert_eq!(once, twice);
        assert_eq!(twice, vec![(1, 0.75)]);
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
        // bincode round-trip through the legacy `Vec<Vec<(id, weight)>>` shape.
        let mut index = SparseIndex::new();
        index.insert(42, &SparseVector::new(vec![1, 2], vec![0.5, 0.3]));
        index.insert(99, &SparseVector::new(vec![2, 3], vec![0.8, 0.2]));

        let data = bincode::serialize(&index).unwrap();
        let index2: SparseIndex = bincode::deserialize(&data).unwrap();

        assert_eq!(index2.len(), 2);
        assert_eq!(index2.postings(), index.postings());
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
