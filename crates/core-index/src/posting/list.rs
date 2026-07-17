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
}

impl PostingList {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn from_items(mut items: Vec<Posting>) -> Self {
        items.sort_by_key(|p| p.doc_id);

        let mut merged: Vec<Posting> = Vec::new();

        for item in items {
            if let Some(last) = merged.last_mut()
                && last.doc_id == item.doc_id {
                    last.positions.extend_from_slice(&item.positions);
                    last.weight = last.weight.max(item.weight);
                    continue;
                }

            merged.push(item);
        }

        for posting in &mut merged {
            posting.positions.sort_unstable();
            posting.positions.dedup();
        }

        Self { items: merged }
    }

    pub fn insert(&mut self, doc_id: DocId, position: Position, weight: u16) {
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
