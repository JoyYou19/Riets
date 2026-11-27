use std::sync::{Arc, Mutex, PoisonError};

use ahash::{HashMap, HashMapExt};
use arc_swap::ArcSwap;
use core_timing::timed;
use roaring::RoaringTreemap;

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions},
    mem::MemIndex,
    numeric_values::{NumericBound, NumericValue},
    posting::{
        DeleteSet, PostingCursor, PostingList,
        cursor::{TermCursor, UnionCursor},
        ops::union_many,
    },
    search::{SearchIndex, SearchNumeric, SearchReader, SearchStats, TermCursorInfo},
    types::{ArrayRowId, DocId, XPathId},
    vector::{SegmentVectors, VectorFilter, VectorHit, search_segments},
    wildcard::WildcardPattern,
};

/*
 * This is a stable view of a current mem + query segments + deletes, so things like querying should
 * go through here
 */
#[derive(Default, Clone)]
pub struct IndexSnapshot {
    mem: Arc<MemIndex>,
    segments: Arc<[Arc<dyn SearchReader + Send + Sync>]>,
    deleted: Arc<DeleteSet>,
      /// Per field, how many deleted documents have it and their total length. Worked out on the
    /// first query that asks; a delete publishes a new snapshot, which starts empty.
    deleted_totals:Arc<Mutex<HashMap<XPathId,(u64,u64)>>>,
    #[allow(clippy::rc_buffer, reason = "shared with LsmIndex, which mutates it via Arc::make_mut")]
    vectors: Arc<Vec<SegmentVectors>>,
}

impl SearchIndex for IndexSnapshot {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        Self::lookup(self, term, xpath)
    }
    fn term_cursor(&self, term: &str, xpath: XPathId) -> Option<TermCursorInfo<'_>> {
        Self::term_cursor(self, term, xpath)
    }

    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        Self::lookup_prefix(self, prefix, xpath)
    }

    fn terms(&self, xpath: XPathId) -> Vec<String> {
        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        out.extend(self.mem.terms(xpath));
        for seg in self.segments.iter() {
            out.extend(seg.terms(xpath));
        }
        out.into_iter().collect()
    }

    fn prefix_terms(&self, prefix: &str, xpath: XPathId) -> Vec<(String, u32)> {
        let mut freq: HashMap<String, u32> = HashMap::new();
        for (term, df) in self.mem.prefix_terms(prefix, xpath).into_iter().chain(
            self.segments
                .iter()
                .flat_map(|s| s.prefix_terms(prefix, xpath)),
        ) {
            *freq.entry(term).or_default() += df;
        }
        freq.into_iter().collect()
    }

    fn element_index_of_row(&self, row: ArrayRowId) -> Option<u32> {
        if let Some(e) = self.mem.element_index_of_row(row) {
            return Some(e);
        }
        self.segments
            .iter()
            .find_map(|seg| seg.element_index_of_row(row))
    }

    fn is_deleted(&self, doc_id: DocId) -> bool {
        self.deleted.is_deleted(doc_id)
    }

    fn resolve_array_rows(&self, rows: &RoaringTreemap) -> RoaringTreemap {
        let mut out = self.mem.resolve_array_rows(rows);
        for seg in self.segments.iter() {
            out |= seg.resolve_array_rows(rows);
        }
        self.deleted.remove_from(&mut out);
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

    fn union_bool_ids(&self, xpath: XPathId, value: bool, out: &mut RoaringTreemap) {
        self.mem.union_bool_ids(xpath, value, out);
        for seg in self.segments.iter() {
            seg.union_bool_ids(xpath, value, out);
        }
        self.deleted.remove_from(out);
    }

    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool> {
        if self.deleted.contains(doc_id) {
            return None;
        }
        if let Some(value) = self.mem.bool_value(xpath, doc_id) {
            return Some(value);
        }
        self.segments
            .iter()
            .find_map(|seg| seg.bool_value(xpath, doc_id))
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        Self::lookup_wildcard(self, pattern, xpath)
    }

    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        Self::lookup_fuzzy(self, term, xpath, opts)
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
    fn doc_ids_into(&self, term: &str, xpath: XPathId, out: &mut RoaringTreemap) {
        let mut ids = RoaringTreemap::new();
        self.mem.doc_ids_into(term, xpath, &mut ids);
        for segment in self.segments.iter() {
            segment.doc_ids_into(term, xpath, &mut ids);
        }
        self.deleted.remove_from(&mut ids);
        *out |= ids;
    }
    fn live_doc_ids(&self) -> Option<RoaringTreemap> {
        let mut out = RoaringTreemap::new();
        if let Some((min, max)) = self.doc_range() {
            out.insert_range(min..=max);
            self.deleted.remove_from(&mut out);
        }
        Some(out)
    }
}

impl SearchNumeric for IndexSnapshot {
        fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
        out: &mut RoaringTreemap,
    ) {
        self.mem.numeric_range(xpath, lo, hi, out);
        for segment in self.segments.iter() {
            segment.numeric_range(xpath, lo, hi, out);
        }
        self.deleted.remove_from(out);
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

    fn numeric_top(
        &self,
        xpath: XPathId,
        k: usize,
        descending: bool,
    ) -> Vec<(DocId, NumericValue)> {
        let mut doc_ids: Vec<DocId> = self
            .mem
            .numeric_top(xpath, k, descending)
            .into_iter()
            .map(|(doc_id, _)| doc_id)
            .collect();
        for segment in self.segments.iter() {
            doc_ids.extend(
                segment
                    .numeric_top(xpath, k, descending)
                    .into_iter()
                    .map(|(doc_id, _)| doc_id),
            );
        }
        doc_ids.sort_unstable();
        doc_ids.dedup();

        let mut merged: Vec<(DocId, NumericValue)> = doc_ids
            .into_iter()
            .filter(|doc_id| !self.deleted.contains(*doc_id))
            .filter_map(|doc_id| self.numeric_value(xpath, doc_id).map(|v| (doc_id, v)))
            .collect();
        merged.sort_by_key(|(_, v)| *v);
        if descending {
            merged.reverse();
        }
        merged.truncate(k);
        merged
    }

    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        let mut result = self.mem.numeric_bounds(xpath);
        for seg in self.segments.iter() {
            let (seg_min, seg_max) = seg.numeric_bounds(xpath);
            result.0 = match (result.0, seg_min) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (None, b) => b,
                (a, _) => a,
            };
            result.1 = match (result.1, seg_max) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (None, b) => b,
                (a, _) => a,
            };
        }
        result
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
        raw.saturating_sub(self.deleted_totals(xpath).0)
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        if let Some(len) = self.mem.doc_len(doc_id, xpath) {
            return Some(len);
        }
        for segment in self.segments.iter().rev() {
            if let Some((lo, hi)) = segment.doc_range()
                && (doc_id < lo || doc_id > hi)
            {
                continue;
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
        raw.saturating_sub(self.deleted_totals(xpath).1)
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
      segments: Arc<[Arc<dyn SearchReader + Send + Sync>]>,
        deleted: Arc<DeleteSet>,
    ) -> Self {
        Self {
            mem,
            segments,
            deleted,
            deleted_totals: Arc::default(),
            vectors: Arc::default(),
        }
    }

    fn apply_deletes(&self, mut postings: PostingList) -> PostingList {
        self.deleted.filter_in_place(&mut postings);
        postings
    }

    fn apply_deletes_to_bitset(&self, mut bitset: RoaringTreemap) -> RoaringTreemap {
        self.deleted.remove_from(&mut bitset);
        bitset
    }
        /// How many deleted documents have the field, and their total length in it.
    fn deleted_totals(&self, xpath: XPathId) -> (u64, u64) {
        if self.deleted.is_empty() {
            return (0, 0);
        }
        // only a cache: after a panic elsewhere its values are still right, or get recomputed
        let mut cache = self
            .deleted_totals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *cache.entry(xpath).or_insert_with(|| {
            self.deleted
                .iter()
                .filter_map(|doc_id| self.doc_len(doc_id, xpath))
                .fold((0, 0), |(count, len), doc_len| {
                    (count.saturating_add(1), len.saturating_add(u64::from(doc_len)))
                })
        })
    }
    #[timed(search)]
    #[must_use]
    pub fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        let mem_list = self.mem.lookup(term, xpath); // Option<&PostingList> — no clone

        let mut segment_lists: Vec<PostingList> = self
            .segments
            .iter()
            .map(|segment| segment.lookup(term, xpath))
            .collect();
        // the word is in one segment only (the usual case after compaction): that list was just
        // decoded for this lookup, hand it over instead of copying every posting into a union
        if mem_list.is_none() && segment_lists.len() <= 1 {
            return self.apply_deletes(segment_lists.pop().unwrap_or_default());
        }
        let mut lists: Vec<&PostingList> = Vec::with_capacity(1 + segment_lists.len());
        if let Some(list) = mem_list {
            lists.push(list);
        }
        lists.extend(segment_lists.iter());

        self.apply_deletes(union_many(lists))
    }

    #[timed(search)]
    #[must_use]
    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let terms = self.pattern_terms(prefix, xpath, |_| true);
        self.union_terms(&terms, xpath)
    }

    #[timed(search)]
    #[must_use]
    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        let terms = self.pattern_terms(pattern.prefix(), xpath, |term| pattern.matches(term));
        self.union_terms(&terms, xpath)
    }
    fn pattern_terms(
        &self,
        prefix: &str,
        xpath: XPathId,
        keep: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut freq: HashMap<String, u32> = HashMap::new();
         let mem: &dyn SearchReader = &*self.mem;
        let sources = std::iter::once(mem)
            .chain(self.segments.iter().map(|s| -> &dyn SearchReader { &**s }));
        for source in sources {
            for (term, df) in source.prefix_terms(prefix, xpath) {
                if keep(&term) {
                    *freq.entry(term).or_default() += df;
                }
            }
        }
        let mut terms: Vec<(String, u32)> = freq.into_iter().collect();
        terms.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        terms.truncate(crate::wildcard::MAX_PATTERN_TERMS);
        terms.into_iter().map(|(term, _)| term).collect()
    }

    /// Union of the postings of `terms` from memory and every segment, deletes removed.
    fn union_terms(&self, terms: &[String], xpath: XPathId) -> PostingList {
        let mut lists = Vec::with_capacity(terms.len() * (self.segments.len() + 1));
        for term in terms {
            lists.push(self.mem.lookup_or_empty(term, xpath));
            for segment in self.segments.iter() {
                lists.push(segment.lookup(term, xpath));
            }
        }
        self.apply_deletes(union_many(lists.iter()))
    }

    #[timed(search)]
    #[must_use]
    pub fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let mut lists = Vec::new();

        lists.push(self.mem.lookup_fuzzy(term, xpath, opts));

        for segment in self.segments.iter() {
            lists.push(segment.lookup_fuzzy(term, xpath, opts));
        }

        self.apply_deletes(union_many(lists.iter()))
    }
    //vectors
    #[must_use]
    pub fn with_vectors(mut self, vectors: Arc<Vec<SegmentVectors>>) -> Self {
        self.vectors = vectors;
        self
    }
    /// Vector files of the published segments.
    #[must_use]
    pub fn vectors(&self) -> &[SegmentVectors] {
        &self.vectors
    }
    #[must_use]
    pub fn deleted(&self) -> &DeleteSet{
        &self.deleted
    }

    /// Exact vector search over the published segments. `query` must be L2-normalized.
    pub fn vector_search(
        &self,
        xpath: XPathId,
        query: &[f32],
        k: usize,
        allowed: Option<&RoaringTreemap>,
    ) -> std::io::Result<Vec<VectorHit>> {
        let filter = VectorFilter::new(&self.deleted, allowed);
        search_segments(&self.vectors, xpath, query, k, &filter)
    }
    #[timed(search)]
    fn term_cursor(&self, term: &str, xpath: XPathId) -> Option<TermCursorInfo<'_>> {
        let mut children = Vec::with_capacity(1 + self.segments.len());
        let mut doc_freq = 0u32;
        let mut max_weight = 0u16;

        if let Some(list) = self.mem.lookup(term, xpath).filter(|list| !list.is_empty()) {
            doc_freq = doc_freq.saturating_add(u32::try_from(list.len()).unwrap_or(u32::MAX));
            max_weight = max_weight.max(list.max_weight());
            children.push(TermCursor::Slice(PostingCursor::new(list)));
        }
        for segment in self.segments.iter() {
            if let Some(info) = segment.term_cursor(term, xpath) {
                doc_freq = doc_freq.saturating_add(info.doc_freq);
                max_weight = max_weight.max(info.max_weight);
                children.push(info.cursor);
            }
        }
        let cursor = if children.is_empty() {
            return None;
        } else if children.len() == 1 && self.deleted.is_empty() {
            children.pop()?
        } else {
            TermCursor::Union(Box::new(UnionCursor::new(children, &self.deleted)))
        };

        Some(TermCursorInfo {
            cursor,
            doc_freq,
            max_weight,
        })
    }
}

#[derive(Clone)]
pub struct SharedIndexSnapshot {
    inner: Arc<ArcSwap<IndexSnapshot>>,
}

impl SharedIndexSnapshot {
    #[must_use]
    pub fn new(snapshot: IndexSnapshot) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(snapshot))),
        }
    }

    pub fn clear(&self) {
        self.inner.store(Arc::new(IndexSnapshot::default()));
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(IndexSnapshot::default())
    }

    pub fn publish(&self, snapshot: IndexSnapshot) {
        self.inner.store(Arc::new(snapshot));
    }

    #[must_use]
    pub fn get(&self) -> Arc<IndexSnapshot> {
        self.inner.load_full()
    }
}
