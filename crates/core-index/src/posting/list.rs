use core_timing::timed;

use crate::{
    posting::{Positions, Posting, item::Offsets},
    types::{DocId, Position},
};

/*
 * A Posting List, this is needed so we can sord by doc_id and merge duplicate docs
 */
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostingList {
    items: Vec<Posting>,
    // Maximum weight of any posting in this list
    //
    // WAND USES this to aclculate a safe upper bound without rescanning the entire posting list at
    // query time
    max_weight: u16,
}

impl PostingList {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            max_weight: 0,
        }
    }

    fn merge_occurrences(
        positions: &mut Positions,
        offsets: &mut Offsets,
        extra_positions: &[Position],
        extra_offsets: &[(u32, u32)],
    ) {
        debug_assert!(extra_offsets.is_empty() || extra_offsets.len() == extra_positions.len());
        debug_assert!(offsets.is_empty() || offsets.len() == positions.len());

        // No offsets anywhere: plain position merge.
        if offsets.is_empty() && extra_offsets.is_empty() {
            positions.extend_from_slice(extra_positions);
            positions.sort_unstable();
            positions.dedup();
            return;
        }

        // Fast path: strictly appended, no overlap.
        if extra_positions
            .first()
            .is_none_or(|first| positions.last().is_some_and(|last| last < first))
        {
            positions.extend_from_slice(extra_positions);
            offsets.extend_from_slice(extra_offsets);
            return;
        }

        // Zip, sort by position, dedup by position.
        let mut zipped: Vec<(Position, (u32, u32))> = positions
            .iter()
            .copied()
            .zip(offsets.iter().copied())
            .chain(
                extra_positions
                    .iter()
                    .copied()
                    .zip(extra_offsets.iter().copied()),
            )
            .collect();
        zipped.sort_unstable_by_key(|(p, _)| *p);
        zipped.dedup_by_key(|(p, _)| *p);
        positions.clear();
        offsets.clear();
        for (p, o) in zipped {
            positions.push(p);
            offsets.push(o);
        }
    }

    pub fn from_sorted(items: Vec<Posting>) -> Self {
        let has_duplicates = items.windows(2).any(|w| w[0].doc_id == w[1].doc_id);

        if !has_duplicates {
            let max_weight = items
                .iter()
                .map(|posting| posting.weight)
                .max()
                .unwrap_or(0);

            return Self { items, max_weight };
        }

        let mut merged: Vec<Posting> = Vec::with_capacity(items.len());
        for item in items {
            if let Some(last) = merged.last_mut() {
                if last.doc_id == item.doc_id {
                    Self::merge_occurrences(
                        &mut last.positions,
                        &mut last.offsets,
                        &item.positions,
                        &item.offsets,
                    );
                    last.weight = last.weight.max(item.weight);
                    continue;
                }
            }
            merged.push(item);
        }

        let max_weight = merged
            .iter()
            .map(|posting| posting.weight)
            .max()
            .unwrap_or(0);

        Self {
            items: merged,
            max_weight,
        }
    }

    // Creates a posting list that is already sorted and max weight was peristed in the segment
    // meta
    //
    // Intended for the disk decoder where postings are decoded
    pub(crate) fn from_sorted_with_max_weight(items: Vec<Posting>, max_weight: u16) -> Self {
        Self { items, max_weight }
    }

    pub fn from_items(mut items: Vec<Posting>) -> Self {
        if !items.is_sorted_by_key(|p| p.doc_id) {
            items.sort_by_key(|p| p.doc_id);
        }
        Self::from_sorted(items)
    }

    #[timed(indexing_documents)]
    pub fn insert(&mut self, doc_id: DocId, position: Position, weight: u16) {
        // max_weight can only stay the same or increase on insertion.
        self.max_weight = self.max_weight.max(weight);
        if let Some(last) = self.items.last_mut() {
            if last.doc_id == doc_id {
                last.positions.push(position);
                last.weight = last.weight.max(weight);
                return;
            }

            if last.doc_id < doc_id {
                self.items
                    .push(Posting::with_weight(doc_id, vec![position], weight));
                return;
            }
        }

        match self.items.binary_search_by_key(&doc_id, |p| p.doc_id) {
            Ok(index) => {
                self.items[index].positions.push(position);
                self.items[index].weight = self.items[index].weight.max(weight);
            }

            Err(index) => {
                self.items
                    .insert(index, Posting::with_weight(doc_id, vec![position], weight));
            }
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Posting) -> bool) {
        self.items.retain(|posting| keep(posting));
    }

    pub fn insert_posting(
        &mut self,
        doc_id: DocId,
        positions: Vec<Position>,
        offsets: Vec<(u32, u32)>,
        weight: u16,
    ) {
        if positions.is_empty() {
            return;
        }

        // Normalize the incoming batch (sort + dedup, keeping offsets aligned).
        let mut pos: Positions = Positions::new();
        let mut off: Offsets = Offsets::new();
        Self::merge_occurrences(&mut pos, &mut off, &positions, &offsets);

        // max_weight can only stay the same or increase on insertion
        self.max_weight = self.max_weight.max(weight);
        if let Some(last) = self.items.last_mut() {
            if last.doc_id == doc_id {
                Self::merge_occurrences(&mut last.positions, &mut last.offsets, &pos, &off);
                last.weight = last.weight.max(weight);
                return;
            }

            if last.doc_id < doc_id {
                self.items
                    .push(Posting::with_weight_and_offsets(doc_id, pos, off, weight));
                return;
            }
        }

        match self.items.binary_search_by_key(&doc_id, |p| p.doc_id) {
            Ok(index) => {
                let posting = &mut self.items[index];
                Self::merge_occurrences(&mut posting.positions, &mut posting.offsets, &pos, &off);
                posting.weight = posting.weight.max(weight);
            }
            Err(index) => {
                self.items.insert(
                    index,
                    Posting::with_weight_and_offsets(doc_id, pos, off, weight),
                );
            }
        }
    }

    #[inline]
    pub fn items(&self) -> &[Posting] {
        &self.items
    }
    pub fn into_items(self) -> Vec<Posting> {
        self.items
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[inline]
    pub fn max_weight(&self) -> u16 {
        self.max_weight
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
            max_weight: 0,
        }
    }
    //TEST
    pub fn append(&mut self, other: PostingList) {
        if other.items.is_empty() {
            return;
        }
        if self.items.is_empty() {
            *self = other;
            return;
        }

        let last = self.items.last().unwrap().doc_id;
        let first = other.items.first().unwrap().doc_id;

        if last < first {
            self.max_weight = self.max_weight.max(other.max_weight);
            self.items.extend(other.items);
        } else {
            let mut items = std::mem::take(&mut self.items);
            items.extend(other.items);
            *self = PostingList::from_items(items);
        }
    }
}
