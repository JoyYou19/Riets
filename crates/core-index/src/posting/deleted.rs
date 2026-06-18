use std::collections::BTreeSet;

use crate::{
    posting::{Posting, PostingList},
    types::DocId,
};

// Whenever a document gets deleted, a tombstone is created for it
// this means it won't apear in any queries, and won't be saved to the disk
//
#[derive(Debug, Default, Clone)]
pub struct DeleteSet {
    deleted: BTreeSet<DocId>,
}

impl DeleteSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn delete(&mut self, doc_id: DocId) {
        self.deleted.insert(doc_id);
    }

    pub fn is_deleted(&self, doc_id: DocId) -> bool {
        self.deleted.contains(&doc_id)
    }

    pub fn filter(&self, list: &PostingList) -> PostingList {
        let items: Vec<Posting> = list
            .items()
            .iter()
            .filter(|posting| !self.is_deleted(posting.doc_id))
            .cloned()
            .collect();

        PostingList::from_items(items)
    }

    pub fn contains(&self, doc_id: DocId) -> bool {
        self.deleted.contains(&doc_id)
    }
}

#[test]
fn filters_deleted_docs() {
    let list = PostingList::from_items(vec![
        Posting::new(1, vec![0]),
        Posting::new(2, vec![0]),
        Posting::new(3, vec![0]),
    ]);

    let mut deleted = DeleteSet::new();
    deleted.delete(2);

    let filtered = deleted.filter(&list);
    let ids: Vec<_> = filtered.items().iter().map(|p| p.doc_id).collect();

    assert_eq!(ids, vec![1, 3]);
}
