//! In-RAM posting list with mutation and ceiling maintenance.

use super::cursor::SliceCursor;
use super::{Posting, RecordId, Weight};

/// A posting list held in RAM: elements sorted by record id, unique ids,
/// every element carrying its `tail_max` ceiling (see the module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Postings {
    items: Vec<Posting>,
}

impl Postings {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(id, weight)` pairs in any order. Later pairs win over
    /// earlier ones with the same id.
    pub fn from_pairs<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (RecordId, Weight)>,
    {
        let mut builder = PostingsBuilder::new();
        for (id, w) in pairs {
            builder.add(id, w);
        }
        builder.build()
    }

    /// Build from pairs already sorted by strictly increasing id. Panics in
    /// debug builds if the order is violated.
    pub fn from_sorted_pairs(pairs: &[(RecordId, Weight)]) -> Self {
        debug_assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0));
        let mut items: Vec<Posting> = pairs
            .iter()
            .map(|&(id, w)| Posting::solo(id, w))
            .collect();
        rebuild_tail_max(&mut items);
        Self { items }
    }

    /// The elements, sorted by id.
    pub fn as_slice(&self) -> &[Posting] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// A cursor at the start of the list.
    pub fn cursor(&self) -> SliceCursor<'_> {
        SliceCursor::new(&self.items)
    }

    /// Weight of `id`, if present.
    pub fn get(&self, id: RecordId) -> Option<Weight> {
        self.locate(id).ok().map(|i| self.items[i].weight)
    }

    /// Insert `id` with `weight`, or replace its weight if it is already
    /// present. Returns the previous weight when replacing.
    ///
    /// Appending an id above every existing one costs O(1) unless the new
    /// weight raises existing ceilings, and then only the raised prefix is
    /// rewritten: inserting a corpus in id order is amortised O(1) per
    /// element for weights without a rising trend.
    pub fn upsert(&mut self, id: RecordId, weight: Weight) -> Option<Weight> {
        match self.locate(id) {
            Ok(i) => {
                let old = self.items[i].weight;
                if old == weight {
                    return Some(old);
                }
                self.items[i].weight = weight;
                self.items[i].tail_max = weight.max(suffix_ceiling(&self.items, i + 1));
                repair_tail_max(&mut self.items, i);
                Some(old)
            }
            Err(i) => {
                let tail_max = weight.max(suffix_ceiling(&self.items, i));
                self.items.insert(
                    i,
                    Posting {
                        id,
                        weight,
                        tail_max,
                    },
                );
                repair_tail_max(&mut self.items, i);
                None
            }
        }
    }

    /// Remove `id`. Returns its weight if it was present.
    pub fn delete(&mut self, id: RecordId) -> Option<Weight> {
        let i = self.locate(id).ok()?;
        let removed = self.items.remove(i);
        // Elements now at `i..` are unchanged; the ones before lost a
        // member of their suffix.
        repair_tail_max(&mut self.items, i);
        Some(removed.weight)
    }

    /// Recompute every ceiling from scratch (after bulk edits through
    /// [`items_mut`](Self::items_mut)).
    pub fn recompute_tail_max(&mut self) {
        rebuild_tail_max(&mut self.items);
    }

    /// Mutable access to the raw elements for bulk edits. The caller must
    /// keep ids sorted and unique, and call
    /// [`recompute_tail_max`](Self::recompute_tail_max) afterwards.
    pub fn items_mut(&mut self) -> &mut Vec<Posting> {
        &mut self.items
    }

    /// Check the structural invariants: ids strictly increasing, and every
    /// `tail_max` equal to the maximum weight of its inclusive suffix.
    /// Returns a description of the first violation.
    pub fn check_invariants(&self) -> Result<(), String> {
        check_ceilings(self.items.iter().copied())
    }

    fn locate(&self, id: RecordId) -> Result<usize, usize> {
        self.items.binary_search_by(|p| p.id.cmp(&id))
    }
}

/// Ceiling of the suffix starting at `from`: `tail_max` of that element, or
/// `-inf` past the end.
#[inline]
fn suffix_ceiling(items: &[Posting], from: usize) -> Weight {
    items.get(from).map_or(Weight::NEG_INFINITY, |p| p.tail_max)
}

/// Recompute every ceiling from the weights alone.
fn rebuild_tail_max(items: &mut [Posting]) {
    let mut running = Weight::NEG_INFINITY;
    for p in items.iter_mut().rev() {
        running = running.max(p.weight);
        p.tail_max = running;
    }
}

/// Bring the ceilings of `0..end` back in line after a change confined to
/// the suffix `end..`, whose ceilings are already correct, given that the
/// ceilings of `0..end` were correct before the change.
///
/// Walking leftwards, the recomputed ceiling at `j` depends only on the
/// weights of `j..end` and on the ceiling at `end`. Once the recomputed
/// value equals the stored one at some `j`, every ceiling left of `j`
/// derives from the same unchanged weights and that same value, so it is
/// already right: the walk stops there. A change that raises no ceiling
/// costs a single comparison.
fn repair_tail_max(items: &mut [Posting], end: usize) {
    let end = end.min(items.len());
    let mut running = suffix_ceiling(items, end);
    for p in items[..end].iter_mut().rev() {
        running = running.max(p.weight);
        if p.tail_max == running {
            break;
        }
        p.tail_max = running;
    }
}

/// Validate id order and ceilings for any sequence of postings, in list
/// order. Shared by the RAM list and the storage adapters' tests.
pub fn check_ceilings(items: impl IntoIterator<Item = Posting>) -> Result<(), String> {
    let items: Vec<Posting> = items.into_iter().collect();
    for (i, w) in items.windows(2).enumerate() {
        if w[0].id >= w[1].id {
            return Err(format!(
                "ids not strictly increasing at {i}: {} then {}",
                w[0].id, w[1].id
            ));
        }
    }
    let mut running = Weight::NEG_INFINITY;
    for (i, p) in items.iter().enumerate().rev() {
        running = running.max(p.weight);
        if p.tail_max < running {
            return Err(format!(
                "tail_max {} at index {i} (id {}) below suffix max {running}",
                p.tail_max, p.id
            ));
        }
    }
    Ok(())
}

/// Accumulates `(id, weight)` pairs and produces a [`Postings`].
#[derive(Clone, Debug, Default)]
pub struct PostingsBuilder {
    pairs: Vec<(RecordId, Weight)>,
}

impl PostingsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a pair. Duplicated ids are resolved at build time, last wins.
    pub fn add(&mut self, id: RecordId, weight: Weight) -> &mut Self {
        self.pairs.push((id, weight));
        self
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Sort, deduplicate (last occurrence wins) and compute ceilings.
    pub fn build(mut self) -> Postings {
        // Stable sort keeps insertion order among equal ids, so the last
        // inserted duplicate is the last of its run.
        self.pairs.sort_by_key(|&(id, _)| id);
        let mut unique: Vec<(RecordId, Weight)> = Vec::with_capacity(self.pairs.len());
        for (id, w) in self.pairs {
            match unique.last_mut() {
                Some(last) if last.0 == id => last.1 = w,
                _ => unique.push((id, w)),
            }
        }
        Postings::from_sorted_pairs(&unique)
    }
}
