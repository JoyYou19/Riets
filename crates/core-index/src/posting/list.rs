use core_timing::timed;

use crate::{ posting::Posting, types::{ DocId, Position } };

/*
 * A Posting List, this is needed so we can sord by doc_id and merge duplicate docs
 */
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostingList {
    items: Vec<Posting>,
}

impl PostingList {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    #[timed(search)]
    /// Like `from_items`, but REQUIRES items already sorted by doc_id.
    pub fn from_sorted(items: Vec<Posting>) -> Self {
        // Callers that produce strictly-increasing doc_ids (every intersection
        // and restriction in ops.rs) hit this path: no duplicates to merge, and
        // positions are already sorted coming out of the index, so there is
        // nothing to do but take ownership.
        let has_duplicates = items.windows(2).any(|w| w[0].doc_id == w[1].doc_id);
        if !has_duplicates {
            return Self { items };
        }

        let mut merged: Vec<Posting> = Vec::with_capacity(items.len());
        for item in items {
            if let Some(last) = merged.last_mut() {
                if last.doc_id == item.doc_id {
                    last.positions.extend_from_slice(&item.positions);
                    last.weight = last.weight.max(item.weight);
                    // Only a posting that actually absorbed another can have
                    // out-of-order or duplicate positions.
                    last.positions.sort_unstable();
                    last.positions.dedup();
                    continue;
                }
            }
            merged.push(item);
        }

        Self { items: merged }
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
        if let Some(last) = self.items.last_mut() {
            if last.doc_id == doc_id {
                last.positions.push(position);
                last.weight = last.weight.max(weight);
                return;
            }

            if last.doc_id < doc_id {
                self.items.push(Posting::with_weight(doc_id, vec![position], weight));
                return;
            }
        }

        match self.items.binary_search_by_key(&doc_id, |p| p.doc_id) {
            Ok(index) => {
                self.items[index].positions.push(position);
                self.items[index].weight = self.items[index].weight.max(weight);
            }
            Err(index) => {
                self.items.insert(index, Posting::with_weight(doc_id, vec![position], weight));
            }
        }
    }

    #[timed(indexing_documents)]
    pub fn insert_posting(&mut self, doc_id: DocId, mut positions: Vec<Position>, weight: u16) {
        if positions.is_empty() {
            return;
        }

        if let Some(last) = self.items.last_mut() {
            if last.doc_id == doc_id {
                last.positions.append(&mut positions);
                last.weight = last.weight.max(weight);
                return;
            }

            if last.doc_id < doc_id {
                self.items.push(Posting::with_weight(doc_id, positions, weight));
                return;
            }
        }

        match self.items.binary_search_by_key(&doc_id, |p| p.doc_id) {
            Ok(index) => {
                self.items[index].positions.append(&mut positions);
                self.items[index].weight = self.items[index].weight.max(weight);
            }
            Err(index) => {
                self.items.insert(index, Posting::with_weight(doc_id, positions, weight));
            }
        }
    }

    pub fn items(&self) -> &[Posting] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
        }
    }
}
