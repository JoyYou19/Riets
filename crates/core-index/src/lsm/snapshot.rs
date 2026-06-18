use std::sync::{Arc, RwLock};

use crate::{
    mem::MemIndex,
    posting::{ops::union_many, DeleteSet, PostingList},
    search::SearchIndex,
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
    segments: Vec<Arc<dyn SearchIndex + Send + Sync>>,
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

impl IndexSnapshot {
    pub fn new(
        mem: MemIndex,
        segments: Vec<Arc<dyn SearchIndex + Send + Sync>>,
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

    pub fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_or_empty(term, xpath));

        for segment in &self.segments {
            lists.push(segment.lookup(term, xpath));
        }

        self.apply_deletes(union_many(lists.iter()))
    }

    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_prefix(prefix, xpath));

        for segment in &self.segments {
            lists.push(segment.lookup_prefix(prefix, xpath));
        }

        self.apply_deletes(union_many(lists.iter()))
    }

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
    inner: Arc<RwLock<IndexSnapshot>>,
}

impl SharedIndexSnapshot {
    pub fn new(snapshot: IndexSnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
        }
    }

    pub fn empty() -> Self {
        Self::new(IndexSnapshot::default())
    }

    pub fn publish(&self, snapshot: IndexSnapshot) {
        let mut guard = self.inner.write().expect("shared snapshot lock poisoned");
        *guard = snapshot;
    }

    pub fn get(&self) -> IndexSnapshot {
        self.inner
            .read()
            .expect("shared snapshot lock poisoned")
            .clone()
    }
}
