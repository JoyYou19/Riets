use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use arc_swap::ArcSwap;
use core_timing::timed;

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions},
    mem::MemIndex,
    numeric_columns::{NumericBound, NumericValue},
    posting::{DeleteSet, PostingList, ops::union_many},
    search::{SearchColumns, SearchIndex, SearchReader, SearchStats},
    types::{DocId, XPathId},
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

    fn terms(&self, xpath: XPathId) -> Vec<String> {
        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        out.extend(self.mem.terms(xpath));
        for seg in &self.segments {
            out.extend(seg.terms(xpath));
        }
        out.into_iter().collect()
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        IndexSnapshot::lookup_wildcard(self, pattern, xpath)
    }

    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        IndexSnapshot::lookup_fuzzy(self, term, xpath, opts)
    }

    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions,
    ) -> Vec<FuzzyExpansion> {
        // Per-segment doc frequencies are summed, so the merged ranking key is
        // the global one rather than whatever a single segment happened to see.
        let mut out: std::collections::BTreeMap<String, FuzzyExpansion> =
            std::collections::BTreeMap::new();

        let all = self
            .mem
            .fuzzy_expansions(term, xpath, opts)
            .into_iter()
            .chain(
                self.segments
                    .iter()
                    .flat_map(|segment| segment.fuzzy_expansions(term, xpath, opts)),
            );

        for expansion in all {
            if let Some(existing) = out.get_mut(&expansion.term) {
                existing.edits = existing.edits.min(expansion.edits);
                existing.doc_freq = existing.doc_freq.saturating_add(expansion.doc_freq);
                continue;
            }

            out.insert(expansion.term.clone(), expansion);
        }

        out.into_values().collect()
    }
}

impl SearchColumns for IndexSnapshot {
    fn column_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.column_range(xpath, lo, hi));

        for segment in &self.segments {
            lists.push(segment.column_range(xpath, lo, hi));
        }

        self.apply_deletes(union_many(lists.iter()))
    }

    fn column_values(&self, xpath: XPathId) -> Vec<(DocId, NumericValue)> {
        let mut out = self.mem.column_values(xpath);
        for segment in &self.segments {
            out.extend(segment.column_values(xpath));
        }
        out
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
        //small optimization
        if self.deleted.is_empty() {
            postings
        } else {
            self.deleted.filter(&postings)
        }
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

    #[timed(search)]
    pub fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_fuzzy(term, xpath, opts));

        for segment in &self.segments {
            lists.push(segment.lookup_fuzzy(term, xpath, opts));
        }

        self.apply_deletes(union_many(lists.iter()))
    }
}

#[derive(Clone)]
pub struct SharedIndexSnapshot {
    inner: Arc<ArcSwap<IndexSnapshot>>,
    generation: Arc<AtomicU64>,
}

impl SharedIndexSnapshot {
    pub fn new(snapshot: IndexSnapshot) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(snapshot))),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn clear(&self) {
        self.inner.store(Arc::new(IndexSnapshot::default()));
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn empty() -> Self {
        Self::new(IndexSnapshot::default())
    }

    pub fn publish(&self, snapshot: IndexSnapshot) {
        self.inner.store(Arc::new(snapshot));
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn get(&self) -> Arc<IndexSnapshot> {
        self.inner.load_full()
    }
}
