use std::sync::Arc;

use ahash::HashSet;
use arc_swap::ArcSwap;
use core_timing::timed;

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions}, mem::MemIndex, numeric_values::{NumericBound, NumericValue}, posting::{DeleteSet, PostingList, ops::union_many_owned}, search::{SearchIndex, SearchNumeric, SearchReader, SearchStats}, types::{ArrayRowId, DocId, XPathId}, wildcard::WildcardPattern,
};

/*
 * This is a stable view of a current mem + query segments + deletes, so things like querying should
 * go through here
 */
#[derive(Default, Clone)]
pub struct IndexSnapshot {
    mem: Arc<MemIndex>,
    segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
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
        for seg in self.segments.iter() {
            out.extend(seg.terms(xpath));
        }
        out.into_iter().collect()
    }

    fn is_deleted(&self, doc_id: DocId) -> bool {
        self.deleted.is_deleted(doc_id)
    }

    fn resolve_array_rows(&self, rows: &HashSet<ArrayRowId>) -> HashSet<DocId> {
        let mut out: HashSet<DocId> = self.mem.resolve_array_rows(rows);
        for seg in self.segments.iter() {
            out.extend(seg.resolve_array_rows(rows));
        }

        //filter deletes
        out.retain(|doc| !self.deleted.contains(*doc));
        out
    }

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId> {
        if let Some(p) = self.mem.parent_of_row(row) {
            return Some(p);
        }
        self.segments.iter().find_map(|seg| seg.parent_of_row(row))
    }

    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId> {
        if let Some(d) = self.mem.doc_of_row(row) {
            return Some(d);
        }
        self.segments.iter().find_map(|seg| seg.doc_of_row(row))
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        IndexSnapshot::lookup_wildcard(self, pattern, xpath)
    }

    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        IndexSnapshot::lookup_fuzzy(self, term, xpath, opts)
    }

    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        let mut total = self.mem.doc_freq(term, xpath);

        for segment in self.segments.iter() {
            total = total.saturating_add(segment.doc_freq(term, xpath));
        }

        total
    }

    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions,
    ) -> Vec<FuzzyExpansion> {
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

impl SearchNumeric for IndexSnapshot {
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.numeric_range(xpath, lo, hi));

        for segment in self.segments.iter() {
            lists.push(segment.numeric_range(xpath, lo, hi));
        }

        self.apply_deletes(union_many_owned(lists))
    }

    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        if self.deleted.contains(doc_id) {
            return None;
        }

        if let Some(value) = self.mem.numeric_value(xpath, doc_id) {
            return Some(value);
        }

        for segment in self.segments.iter() {
            if let Some(value) = segment.numeric_value(xpath, doc_id) {
                return Some(value);
            }
        }

        None
    }
}

impl SearchStats for IndexSnapshot {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        let raw = self.mem.doc_count(xpath)
            + self
                .segments
                .iter()
                .map(|s| s.doc_count(xpath))
                .sum::<u64>();
        let deleted = self
            .deleted
            .iter()
            .filter(|&doc_id| self.doc_len(doc_id, xpath).is_some())
            .count() as u64;
        raw.saturating_sub(deleted)
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        if let Some(len) = self.mem.doc_len(doc_id, xpath) {
            return Some(len);
        }
        for segment in self.segments.iter().rev() {
            if let Some((lo, hi)) = segment.doc_range() {
                if doc_id < lo || doc_id > hi {
                    continue;
                }
            }
            if let Some(len) = segment.doc_len(doc_id, xpath) {
                return Some(len);
            }
        }
        None
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        let raw = self.mem.total_doc_len(xpath)
            + self
                .segments
                .iter()
                .map(|s| s.total_doc_len(xpath))
                .sum::<u64>();
        let deleted: u64 = self
            .deleted
            .iter()
            .filter_map(|doc_id| self.doc_len(doc_id, xpath))
            .map(|len| len as u64)
            .sum();
        raw.saturating_sub(deleted)
    }

    fn doc_range(&self) -> Option<(DocId, DocId)> {
        let mut range: Option<(DocId, DocId)> = self.mem.doc_range();

        for segment in self.segments.iter() {
            if let Some((lo, hi)) = segment.doc_range() {
                range = Some(match range {
                    None => (lo, hi),
                    Some((min, max)) => (min.min(lo), max.max(hi)),
                });
            }
        }

        range
    }
}

impl IndexSnapshot {
    pub fn new(
        mem: Arc<MemIndex>,
        segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
        deleted: DeleteSet,
    ) -> Self {
        Self {
            mem,
            segments,
            deleted,
        }
    }

    fn apply_deletes(&self, mut postings: PostingList) -> PostingList {
        self.deleted.filter_in_place(&mut postings);
        postings
    }

    #[timed(search)]
    pub fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        let mut lists: Vec<PostingList> = Vec::with_capacity(1 + self.segments.len());

        //the memtable list is borrowed, so it is copied
        if let Some(list) = self.mem.lookup(term, xpath) {
            lists.push(PostingList::from_sorted(list.items().to_vec()));
        }

        //segment lists are freshly decoded and owned: moved into the merge, not copied
        for segment in self.segments.iter() {
            lists.push(segment.lookup(term, xpath));
        }

        self.apply_deletes(union_many_owned(lists))
    }

    #[timed(search)]
    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_prefix(prefix, xpath));

        for segment in self.segments.iter() {
            lists.push(segment.lookup_prefix(prefix, xpath));
        }

        self.apply_deletes(union_many_owned(lists))
    }

    #[timed(search)]
    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_wildcard(pattern, xpath));

        for segment in self.segments.iter() {
            lists.push(segment.lookup_wildcard(pattern, xpath));
        }

        self.apply_deletes(union_many_owned(lists))
    }

    #[timed(search)]
    pub fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_fuzzy(term, xpath, opts));

        for segment in self.segments.iter() {
            lists.push(segment.lookup_fuzzy(term, xpath, opts));
        }

        self.apply_deletes(union_many_owned(lists))
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

    pub fn clear(&self) {
        self.inner.store(Arc::new(IndexSnapshot::default()));
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
