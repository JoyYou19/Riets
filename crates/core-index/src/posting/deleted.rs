use roaring::RoaringTreemap;

use crate::{posting::PostingList, types::DocId};

// Whenever a document gets deleted, a tombstone is created for it
// this means it won't apear in any queries, and won't be saved to the disk
#[derive(Debug, Default, Clone)]
pub struct DeleteSet {
    deleted: RoaringTreemap,
}

impl DeleteSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty()
    }

    pub fn delete(&mut self, doc_id: DocId) {
        self.deleted.insert(doc_id);
    }

    pub fn is_deleted(&self, doc_id: DocId) -> bool {
        self.deleted.contains(doc_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = DocId> + '_ {
        self.deleted.iter()
    }

    pub fn filter_in_place(&self, list: &mut PostingList) {
        if self.is_empty() {
            return;
        }
        list.retain(|posting| !self.is_deleted(posting.doc_id));
    }

    pub fn contains(&self, doc_id: DocId) -> bool {
        self.deleted.contains(doc_id)
    }

    /// Removes every deleted doc id from `bitset` in one bulk pass.
    pub fn remove_from(&self, bitset: &mut RoaringTreemap) {
        if self.is_empty() {
            return;
        }
        *bitset -= &self.deleted;
    }
}
