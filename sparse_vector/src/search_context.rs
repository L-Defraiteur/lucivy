// Based on Qdrant sparse index (https://github.com/qdrant/qdrant)
// Copyright 2021-2026 Qdrant Team <info@qdrant.tech>
// Licensed under Apache License 2.0
// Modified for rag3db sparse-vector extension

//! Search context: batch scoring over posting lists with WAND-like pruning.

use std::cmp::{Ordering, max, min};

use crate::posting_list::PostingListIterator;
use crate::posting_list_common::PostingListIter;
use crate::scores_memory_pool::PooledScoresHandle;
use crate::top_k::{ScoredPointOffset, TopK};

/// Iterator over a posting list with its associated query dimension weight.
struct IndexedPostingListIterator<T: PostingListIter> {
    posting_list_iterator: T,
    query_weight: f32,
}

/// Batch size for scoring: process contiguous ID ranges of this size.
const ADVANCE_BATCH_SIZE: usize = 10_000;

/// Search context: holds posting list iterators for each query dimension,
/// drives batch scoring and WAND pruning.
pub struct SearchContext<'a, 'b, T: PostingListIter = PostingListIterator<'a>> {
    postings_iterators: Vec<IndexedPostingListIterator<T>>,
    top: usize,
    top_results: TopK,
    min_record_id: Option<u64>,
    max_record_id: u64,
    pooled: PooledScoresHandle<'b>,
    use_pruning: bool,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a, 'b, T: PostingListIter> SearchContext<'a, 'b, T> {
    /// Create a new search context from query indices/weights and posting list accessors.
    ///
    /// `get_posting` is called for each query dimension to get the posting list iterator.
    pub fn new(
        query_indices: &[u32],
        query_values: &[f32],
        top: usize,
        mut get_posting: impl FnMut(u32) -> Option<T>,
        pooled: PooledScoresHandle<'b>,
    ) -> Self {
        let mut postings_iterators = Vec::new();
        let mut max_record_id: u64 = 0;
        let mut min_record_id: u64 = u64::MAX;

        for (i, &dim_id) in query_indices.iter().enumerate() {
            if let Some(mut it) = get_posting(dim_id) {
                if let (Some(first), Some(last_id)) = (it.peek(), it.last_id()) {
                    min_record_id = min(min_record_id, first.record_id);
                    max_record_id = max(max_record_id, last_id);

                    postings_iterators.push(IndexedPostingListIterator {
                        posting_list_iterator: it,
                        query_weight: query_values[i],
                    });
                }
            }
        }

        let top_results = TopK::new(top);
        let use_pruning =
            T::reliable_max_next_weight() && query_values.iter().all(|v| *v >= 0.0);
        let min_record_id = if min_record_id == u64::MAX {
            None
        } else {
            Some(min_record_id)
        };

        Self {
            postings_iterators,
            top,
            top_results,
            min_record_id,
            max_record_id,
            pooled,
            use_pruning,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Advance posting lists in a batch, accumulate scores in pooled buffer.
    fn advance_batch<F: Fn(u64) -> bool>(
        &mut self,
        batch_start_id: u64,
        batch_last_id: u64,
        filter_condition: &F,
    ) {
        let batch_len = (batch_last_id - batch_start_id + 1) as usize;
        self.pooled.scores.clear();
        self.pooled.scores.resize(batch_len, 0.0);

        for posting in self.postings_iterators.iter_mut() {
            let query_weight = posting.query_weight;
            posting.posting_list_iterator.for_each_till_id(
                batch_last_id,
                self.pooled.scores.as_mut_slice(),
                #[inline(always)]
                |scores, id, weight| {
                    let local_id = (id - batch_start_id) as usize;
                    if local_id < scores.len() {
                        scores[local_id] += weight * query_weight;
                    }
                },
            );
        }

        for (local_index, &score) in self.pooled.scores.iter().enumerate() {
            if score != 0.0 && score > self.top_results.threshold() {
                let real_id = batch_start_id + local_index as u64;
                if !filter_condition(real_id) {
                    continue;
                }
                self.top_results.push(ScoredPointOffset {
                    score,
                    idx: real_id,
                });
            }
        }
    }

    /// Fast path when only one posting list remains.
    fn process_last_posting_list<F: Fn(u64) -> bool>(&mut self, filter_condition: &F) {
        debug_assert_eq!(self.postings_iterators.len(), 1);
        let posting = &mut self.postings_iterators[0];
        let query_weight = posting.query_weight;
        posting.posting_list_iterator.for_each_till_id(
            u64::MAX,
            &mut (),
            |_, id, weight| {
                if !filter_condition(id) {
                    return;
                }
                let score = weight * query_weight;
                self.top_results.push(ScoredPointOffset { score, idx: id });
            },
        );
    }

    /// Find the next minimum record_id across all posting list iterators.
    fn next_min_id(to_inspect: &mut [IndexedPostingListIterator<T>]) -> Option<u64> {
        let mut min_record_id = None;
        for posting_iterator in to_inspect.iter_mut() {
            if let Some(next_element) = posting_iterator.posting_list_iterator.peek() {
                match min_record_id {
                    None => min_record_id = Some(next_element.record_id),
                    Some(min_id_seen) => {
                        if next_element.record_id < min_id_seen {
                            min_record_id = Some(next_element.record_id);
                        }
                    }
                }
            }
        }
        min_record_id
    }

    /// Move the longest posting list to position 0.
    fn promote_longest_posting_lists_to_the_front(&mut self) {
        let posting_index = self
            .postings_iterators
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.posting_list_iterator
                    .len_to_end()
                    .cmp(&b.posting_list_iterator.len_to_end())
            })
            .map(|(index, _)| index);

        if let Some(posting_index) = posting_index {
            if posting_index != 0 {
                self.postings_iterators.swap(0, posting_index);
            }
        }
    }

    /// Run the search. Returns top-K scored results.
    pub fn search<F: Fn(u64) -> bool>(
        &mut self,
        filter_condition: &F,
    ) -> Vec<ScoredPointOffset> {
        if self.postings_iterators.is_empty() {
            return Vec::new();
        }

        let mut best_min_score = f32::MIN;
        loop {
            let Some(start_batch_id) = self.min_record_id else {
                break;
            };

            let last_batch_id = min(
                start_batch_id + ADVANCE_BATCH_SIZE as u64,
                self.max_record_id,
            );

            self.advance_batch(start_batch_id, last_batch_id, filter_condition);

            // Remove exhausted posting lists
            self.postings_iterators.retain(|pi| {
                pi.posting_list_iterator.len_to_end() != 0
            });

            self.min_record_id = Self::next_min_id(&mut self.postings_iterators);

            if self.postings_iterators.is_empty() {
                break;
            }

            // Fast path: single posting list left
            if self.postings_iterators.len() == 1 {
                self.process_last_posting_list(filter_condition);
                break;
            }

            // Try pruning if we have enough results
            if self.use_pruning && self.top_results.len() >= self.top {
                let new_min_score = self.top_results.threshold();
                if new_min_score == best_min_score {
                    continue;
                }
                best_min_score = new_min_score;

                self.promote_longest_posting_lists_to_the_front();
                let pruned = self.prune_longest_posting_list(new_min_score);
                if pruned {
                    self.min_record_id = Self::next_min_id(&mut self.postings_iterators);
                }
            }
        }

        let queue = std::mem::replace(&mut self.top_results, TopK::new(0));
        queue.into_vec()
    }

    /// Prune the longest posting list using WAND-like logic.
    fn prune_longest_posting_list(&mut self, min_score: f32) -> bool {
        if self.postings_iterators.is_empty() {
            return false;
        }
        let (longest_posting_iterator, rest_iterators) = self.postings_iterators.split_at_mut(1);
        let longest_posting_iterator = &mut longest_posting_iterator[0];
        if let Some(element) = longest_posting_iterator.posting_list_iterator.peek() {
            let next_min_id_in_others = Self::next_min_id(rest_iterators);
            match next_min_id_in_others {
                Some(next_min_id) => {
                    match next_min_id.cmp(&element.record_id) {
                        Ordering::Equal | Ordering::Less => {
                            return false;
                        }
                        Ordering::Greater => {
                            let max_weight_from_list =
                                element.weight.max(element.max_next_weight);
                            let max_score_contribution =
                                max_weight_from_list * longest_posting_iterator.query_weight;
                            if max_score_contribution <= min_score {
                                let position_before =
                                    longest_posting_iterator.posting_list_iterator.current_index();
                                longest_posting_iterator
                                    .posting_list_iterator
                                    .skip_to(next_min_id);
                                let position_after =
                                    longest_posting_iterator.posting_list_iterator.current_index();
                                return position_before != position_after;
                            }
                        }
                    }
                }
                None => {
                    let max_weight_from_list = element.weight.max(element.max_next_weight);
                    let max_score_contribution =
                        max_weight_from_list * longest_posting_iterator.query_weight;
                    if max_score_contribution <= min_score {
                        longest_posting_iterator
                            .posting_list_iterator
                            .skip_to_end();
                        return true;
                    }
                }
            }
        }
        false
    }
}
