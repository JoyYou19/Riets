use core_timing::timed;

use crate::{
    posting::Posting,
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

    #[timed(search)]
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
                    last.positions.extend_from_slice(&item.positions);
                    last.weight = last.weight.max(item.weight);
                    last.positions.sort_unstable();
                    last.positions.dedup();
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

    #[timed(search)]
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

    #[timed(indexing_documents)]
    pub fn insert_posting(&mut self, doc_id: DocId, mut positions: Vec<Position>, weight: u16) {
        if positions.is_empty() {
            return;
        }
        // max_weight can only stay the same or increase on insertion
        self.max_weight = self.max_weight.max(weight);
        if let Some(last) = self.items.last_mut() {
            if last.doc_id == doc_id {
                last.positions.append(&mut positions);
                last.weight = last.weight.max(weight);
                return;
            }

            if last.doc_id < doc_id {
                self.items
                    .push(Posting::with_weight(doc_id, positions, weight));
                return;
            }
        }

        match self.items.binary_search_by_key(&doc_id, |p| p.doc_id) {
            Ok(index) => {
                self.items[index].positions.append(&mut positions);
                self.items[index].weight = self.items[index].weight.max(weight);
            }

            Err(index) => {
                self.items
                    .insert(index, Posting::with_weight(doc_id, positions, weight));
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
