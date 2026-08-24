// Based on Qdrant sparse index (https://github.com/qdrant/qdrant)
// Copyright 2021-2026 Qdrant Team <info@qdrant.tech>
// Licensed under Apache License 2.0
// Modified for rag3db sparse-vector extension

//! Mutable posting list: sorted Vec<PostingElementEx> with max_next_weight for WAND pruning.

use std::cmp::max;
use ordered_float::OrderedFloat;

use crate::posting_list_common::{
    DEFAULT_MAX_NEXT_WEIGHT, PostingElementEx, PostingListIter,
};

/// A posting list: sorted by record_id, with pre-computed max_next_weight for pruning.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PostingList {
    pub elements: Vec<PostingElementEx>,
}

impl PostingList {
    /// Create a posting list with a single element.
    pub fn new_one(record_id: u64, weight: f32) -> Self {
        Self {
            elements: vec![PostingElementEx::new(record_id, weight)],
        }
    }

    /// Delete an element by record_id.
    pub fn delete(&mut self, record_id: u64) {
        let index = self
            .elements
            .binary_search_by_key(&record_id, |e| e.record_id);
        if let Ok(found_index) = index {
            self.elements.remove(found_index);
            if let Some(last) = self.elements.last_mut() {
                last.max_next_weight = DEFAULT_MAX_NEXT_WEIGHT;
            }
            if found_index < self.elements.len() {
                self.propagate_max_next_weight_to_the_left(found_index);
            } else if !self.elements.is_empty() {
                self.propagate_max_next_weight_to_the_left(self.elements.len() - 1);
            }
        }
    }

    /// Upsert a posting element. Updates weight if record_id exists, inserts otherwise.
    /// Maintains sorted order and propagates max_next_weight.
    pub fn upsert(&mut self, posting_element: PostingElementEx) {
        let index = self
            .elements
            .binary_search_by_key(&posting_element.record_id, |e| e.record_id);

        let modified_index = match index {
            Ok(found_index) => {
                let element = &mut self.elements[found_index];
                if element.weight == posting_element.weight {
                    None
                } else {
                    element.weight = posting_element.weight;
                    Some(found_index)
                }
            }
            Err(insert_index) => {
                self.elements.insert(insert_index, posting_element);
                if insert_index == self.elements.len() - 1 {
                    Some(insert_index)
                } else {
                    Some(insert_index + 1)
                }
            }
        };
        if let Some(modified_index) = modified_index {
            self.propagate_max_next_weight_to_the_left(modified_index);
        }
    }

    /// Propagates max_next_weight backwards from `up_to_index`.
    fn propagate_max_next_weight_to_the_left(&mut self, up_to_index: usize) {
        let starting_element = &self.elements[up_to_index];
        let mut max_next_weight = max(
            OrderedFloat(starting_element.max_next_weight),
            OrderedFloat(starting_element.weight),
        )
        .0;

        for element in self.elements[..up_to_index].iter_mut().rev() {
            element.max_next_weight = max_next_weight;
            max_next_weight = max_next_weight.max(element.weight);
        }
    }

    /// Create from pre-sorted elements with max_next_weight already computed.
    pub fn from_sorted(elements: Vec<PostingElementEx>) -> Self {
        Self { elements }
    }

    pub fn iter(&self) -> PostingListIterator<'_> {
        PostingListIterator::new(&self.elements)
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

/// Builder for constructing a posting list from unsorted elements.
pub struct PostingBuilder {
    elements: Vec<PostingElementEx>,
}

impl Default for PostingBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PostingBuilder {
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
        }
    }

    pub fn add(&mut self, record_id: u64, weight: f32) {
        self.elements.push(PostingElementEx::new(record_id, weight));
    }

    /// Consume the builder: sort by id, compute max_next_weight, return PostingList.
    pub fn build(mut self) -> PostingList {
        self.elements.sort_unstable_by_key(|e| e.record_id);

        #[cfg(debug_assertions)]
        {
            if let Some(e) = self
                .elements
                .windows(2)
                .find(|e| e[0].record_id == e[1].record_id)
            {
                panic!("Duplicate id {} in posting list", e[0].record_id);
            }
        }

        // Compute max_next_weight from right to left
        let mut max_next_weight = f32::NEG_INFINITY;
        for element in self.elements.iter_mut().rev() {
            element.max_next_weight = max_next_weight;
            max_next_weight = max_next_weight.max(element.weight);
        }

        PostingList {
            elements: self.elements,
        }
    }
}

/// Iterator over posting list elements with skip (binary search) abilities.
#[derive(Debug, Clone)]
pub struct PostingListIterator<'a> {
    pub elements: &'a [PostingElementEx],
    pub current_index: usize,
}

impl<'a> PostingListIterator<'a> {
    pub fn new(elements: &'a [PostingElementEx]) -> Self {
        Self {
            elements,
            current_index: 0,
        }
    }

    pub fn advance(&mut self) {
        if self.current_index < self.elements.len() {
            self.current_index += 1;
        }
    }
}

impl PostingListIter for PostingListIterator<'_> {
    #[inline]
    fn peek(&mut self) -> Option<PostingElementEx> {
        self.elements.get(self.current_index).cloned()
    }

    #[inline]
    fn last_id(&self) -> Option<u64> {
        self.elements.last().map(|e| e.record_id)
    }

    fn skip_to(&mut self, record_id: u64) -> Option<PostingElementEx> {
        if self.current_index >= self.elements.len() {
            return None;
        }
        let result =
            self.elements[self.current_index..].binary_search_by(|e| e.record_id.cmp(&record_id));
        match result {
            Ok(found_offset) => {
                self.current_index += found_offset;
                Some(self.elements[self.current_index].clone())
            }
            Err(insert_index) => {
                self.current_index += insert_index;
                None
            }
        }
    }

    fn skip_to_end(&mut self) {
        self.current_index = self.elements.len();
    }

    #[inline]
    fn len_to_end(&self) -> usize {
        self.elements.len() - self.current_index
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
        let mut current_index = self.current_index;
        for element in &self.elements[current_index..] {
            if element.record_id > id {
                break;
            }
            f(ctx, element.record_id, element.weight);
            current_index += 1;
        }
        self.current_index = current_index;
    }

    fn reliable_max_next_weight() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posting_operations() {
        let mut builder = PostingBuilder::new();
        builder.add(1, 1.0);
        builder.add(2, 2.1);
        builder.add(5, 5.0);
        builder.add(3, 2.0);
        builder.add(8, 3.4);
        builder.add(10, 3.0);
        builder.add(20, 3.0);
        builder.add(7, 4.0);
        builder.add(11, 3.0);

        let posting_list = builder.build();
        let mut iter = PostingListIterator::new(&posting_list.elements);

        assert_eq!(iter.peek().unwrap().record_id, 1);
        iter.advance();
        assert_eq!(iter.peek().unwrap().record_id, 2);
        iter.advance();
        assert_eq!(iter.peek().unwrap().record_id, 3);

        assert_eq!(iter.skip_to(7).unwrap().record_id, 7);
        assert_eq!(iter.peek().unwrap().record_id, 7);

        assert!(iter.skip_to(9).is_none());
        assert_eq!(iter.peek().unwrap().record_id, 10);

        assert!(iter.skip_to(20).is_some());
        assert_eq!(iter.peek().unwrap().record_id, 20);

        assert!(iter.skip_to(21).is_none());
        assert!(iter.peek().is_none());
    }

    #[test]
    fn test_upsert_insert_last() {
        let mut builder = PostingBuilder::new();
        builder.add(1, 1.0);
        builder.add(3, 3.0);
        builder.add(2, 2.0);
        let mut posting_list = builder.build();

        assert_eq!(posting_list.elements[0].max_next_weight, 3.0);
        assert_eq!(posting_list.elements[1].max_next_weight, 3.0);
        assert_eq!(posting_list.elements[2].max_next_weight, DEFAULT_MAX_NEXT_WEIGHT);

        posting_list.upsert(PostingElementEx::new(4, 4.0));
        assert_eq!(posting_list.elements[3].record_id, 4);
        assert_eq!(posting_list.elements[3].weight, 4.0);
        assert_eq!(posting_list.elements[3].max_next_weight, DEFAULT_MAX_NEXT_WEIGHT);

        for element in posting_list.elements.iter().take(3) {
            assert_eq!(element.max_next_weight, 4.0);
        }
    }

    #[test]
    fn test_delete() {
        let mut builder = PostingBuilder::new();
        builder.add(1, 1.0);
        builder.add(2, 2.0);
        builder.add(3, 3.0);
        let mut posting_list = builder.build();

        posting_list.delete(2);
        assert_eq!(posting_list.len(), 2);
        assert_eq!(posting_list.elements[0].record_id, 1);
        assert_eq!(posting_list.elements[1].record_id, 3);
    }

    #[test]
    fn test_upsert_update_weight() {
        let mut builder = PostingBuilder::new();
        builder.add(1, 1.0);
        builder.add(2, 2.0);
        builder.add(3, 3.0);
        let mut posting_list = builder.build();

        posting_list.upsert(PostingElementEx::new(2, 5.0));
        assert_eq!(posting_list.elements[1].weight, 5.0);
        assert_eq!(posting_list.elements[0].max_next_weight, 5.0);
    }

    #[test]
    fn test_for_each_till_id() {
        let mut builder = PostingBuilder::new();
        builder.add(1, 0.1);
        builder.add(3, 0.3);
        builder.add(5, 0.5);
        builder.add(7, 0.7);
        let posting_list = builder.build();
        let mut iter = posting_list.iter();

        let mut collected = Vec::new();
        iter.for_each_till_id(4, &mut collected, |v, id, w| v.push((id, w)));
        assert_eq!(collected, vec![(1, 0.1), (3, 0.3)]);

        collected.clear();
        iter.for_each_till_id(7, &mut collected, |v, id, w| v.push((id, w)));
        assert_eq!(collected, vec![(5, 0.5), (7, 0.7)]);
    }
}
