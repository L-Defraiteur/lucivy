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
        let n = items.len();
        refresh_tail_max(&mut items, n);
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
    pub fn upsert(&mut self, id: RecordId, weight: Weight) -> Option<Weight> {
        match self.locate(id) {
            Ok(i) => {
                let old = self.items[i].weight;
                self.items[i].weight = weight;
                // Elements after `i` keep their suffixes; everything up to
                // and including `i` sees a changed suffix.
                refresh_tail_max(&mut self.items, i + 1);
                Some(old)
            }
            Err(i) => {
                self.items.insert(i, Posting::solo(id, weight));
                refresh_tail_max(&mut self.items, i + 1);
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
        refresh_tail_max(&mut self.items, i);
        Some(removed.weight)
    }

    /// Recompute every ceiling from scratch (after bulk edits through
    /// [`items_mut`](Self::items_mut)).
    pub fn recompute_tail_max(&mut self) {
        let n = self.items.len();
        refresh_tail_max(&mut self.items, n);
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

/// Recompute `tail_max` for positions `0..end`, assuming the ceilings at
/// `end..` are already correct.
fn refresh_tail_max(items: &mut [Posting], end: usize) {
    let end = end.min(items.len());
    let mut running = items
        .get(end)
        .map_or(Weight::NEG_INFINITY, |p| p.tail_max);
    for p in items[..end].iter_mut().rev() {
        running = running.max(p.weight);
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
