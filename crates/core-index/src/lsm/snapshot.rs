use std::sync::Arc;

use arc_swap::ArcSwap;
use core_timing::timed;

use crate::{
    mem::MemIndex,
    posting::{DeleteSet, PostingList, ops::union_many},
    search::{SearchIndex, SearchReader, SearchStats},
    types::XPathId,
    wildcard::WildcardPattern,
};

/*
* This is a stable view of a current mem + query segments + deletes, so things like querying should
* go through here
*/
#[derive(Default, Clone)]
pub struct IndexSnapshot {
    mem: MemIndex,
    segments: Vec<Arc<dyn SearchReader + Send + Sync>>,
    deleted: DeleteSet,
}

impl SearchIndex for IndexSnapshot {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        IndexSnapshot::lookup(self, term, xpath)
    }

    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        IndexSnapshot::lookup_prefix(self, prefix, xpath)
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        IndexSnapshot::lookup_wildcard(self, pattern, xpath)
    }
}

impl SearchStats for IndexSnapshot {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        let mut total = self.mem.doc_count(xpath);

        for segment in &self.segments {
            total += segment.doc_count(xpath);
        }

        total
    }

    fn doc_len(&self, doc_id: crate::types::DocId, xpath: XPathId) -> Option<u32> {
        if let Some(len) = self.mem.doc_len(doc_id, xpath) {
            return Some(len);
        }

        for segment in &self.segments {
            if let Some(len) = segment.doc_len(doc_id, xpath) {
                return Some(len);
            }
        }

        None
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        let mut total = self.mem.total_doc_len(xpath);

        for segment in &self.segments {
            total += segment.total_doc_len(xpath);
        }

        total
    }
}

impl IndexSnapshot {
    pub fn new(
        mem: MemIndex,
        segments: Vec<Arc<dyn SearchReader + Send + Sync>>,
        deleted: DeleteSet,
    ) -> Self {
        Self {
            mem,
            segments,
            deleted,
        }
    }

    fn apply_deletes(&self, postings: PostingList) -> PostingList {
        self.deleted.filter(&postings)
    }

    #[timed(search)]
    pub fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_or_empty(term, xpath));

        for segment in &self.segments {
            lists.push(segment.lookup(term, xpath));
        }

        self.apply_deletes(union_many(lists.iter()))
    }

    #[timed(search)]
    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_prefix(prefix, xpath));

        for segment in &self.segments {
            lists.push(segment.lookup_prefix(prefix, xpath));
        }

        self.apply_deletes(union_many(lists.iter()))
    }

    #[timed(search)]
    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_wildcard(pattern, xpath));

        for segment in &self.segments {
            lists.push(segment.lookup_wildcard(pattern, xpath));
        }

        self.apply_deletes(union_many(lists.iter()))
    }
}

#[derive(Clone)]
pub struct SharedIndexSnapshot {
    inner: Arc<ArcSwap<IndexSnapshot>>,
}

impl SharedIndexSnapshot {
    pub fn new(snapshot: IndexSnapshot) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(snapshot))),
        }
    }

    pub fn empty() -> Self {
        Self::new(IndexSnapshot::default())
    }

    pub fn publish(&self, snapshot: IndexSnapshot) {
        self.inner.store(Arc::new(snapshot));
    }

    pub fn get(&self) -> Arc<IndexSnapshot> {
        self.inner.load_full()
    }
}
