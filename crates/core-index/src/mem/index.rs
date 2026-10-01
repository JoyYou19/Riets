use std::collections::BTreeMap;

use ahash::{HashMapExt, HashSet};
use core_timing::timed;
use ahash::AHashMap;
use crate::analyzer::analyzer::Analyzer;
use crate::array_rows::ArrayRowIndex;
use crate::document::document::NumericPoint;
use crate::document::{DocumentPart, IndexedDocument};
use crate::numeric_values::{NumericBound, NumericPoints, NumericValue};
use crate::posting::{Posting, PostingList};
use crate::search::{SearchIndex, SearchNumeric, SearchStats};
use crate::types::{ArrayRowId, DocId, FieldStats, TermKey, XPathId};
use crate::wildcard::WildcardPattern;

// Memory inverted index, the core of the index
#[derive(Debug, Clone)]
pub struct MemIndex {
    terms: AHashMap<TermKey, PostingList>,
    doc_lengths: AHashMap<(DocId, XPathId), u32>,
    field_stats: BTreeMap<XPathId, FieldStats>,
    numeric_points: NumericPoints,
    estimated_bytes: usize,
    min_doc_id: Option<DocId>,
    max_doc_id: Option<DocId>,
    array_row_index: ArrayRowIndex,
    estimated_bytes: usize,
}

impl Default for MemIndex {
    fn default() -> Self {
        Self {
            terms: AHashMap::default(),
            doc_lengths: AHashMap::default(),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
            estimated_bytes: 0,
        }
    }
}
impl SearchIndex for MemIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup_or_empty(term, xpath)
    }

    fn resolve_array_rows(&self, rows: &HashSet<ArrayRowId>) -> HashSet<DocId> {
        rows.iter()
            .filter_map(|&r| self.array_row_index.doc_of(r))
            .collect()
    }

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId> {
        self.array_row_index.parent_of(row)
    }

    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId> {
        self.array_row_index.doc_of(row)
    }

    fn terms(&self, xpath: XPathId) -> Vec<String> {
        self.terms
            .keys()
            .filter(|k| k.xpath == xpath)
            .map(|k| k.term.clone())
            .collect()
    }
    // No TermMeta here — MemIndex stores full PostingLists directly, so
    // doc_freq is just the stored list's length. Uses the private lookup()
    // (returns &PostingList, no clone) rather than lookup_or_empty, since
    // there's no need to clone the whole posting list just to read its len.
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        self.terms
            .get(&TermKey::new(term, xpath))
            .map(|list| list.len() as u32)
            .unwrap_or(0)
    }
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.lookup_prefix(prefix, xpath)
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        self.lookup_wildcard(pattern, xpath)
    }
}

impl SearchNumeric for MemIndex {
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>
    ) -> PostingList {
        let docs = self.numeric_points.range(xpath, lo, hi);
        PostingList::from_items(
            docs
                .into_iter()
                .map(|doc_id| Posting::with_weight(doc_id, Vec::new(), 0))
                .collect()
        )
    }

    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.numeric_points.get(xpath, doc_id)
    }
}

impl SearchStats for MemIndex {
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.doc_lengths.get(&(doc_id, xpath)).copied()
    }

    fn doc_count(&self, xpath: XPathId) -> u64 {
        self.field_stats
            .get(&xpath)
            .map(|s| s.doc_count)
            .unwrap_or(0)
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        self.field_stats
            .get(&xpath)
            .map(|s| s.total_doc_len)
            .unwrap_or(0)
    }
    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.doc_id_range()
    }
}

impl MemIndex {
    pub fn new() -> Self {
        Self {
            terms: AHashMap::new(),
            doc_lengths: AHashMap::new(),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
            estimated_bytes: 0,
        }
    }
    pub fn with_capacity(expected_docs: usize, expected_terms: usize) -> Self {
        Self {
            terms: AHashMap::with_capacity(expected_terms),
            doc_lengths: AHashMap::with_capacity(expected_docs),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
            estimated_bytes: 0,
        }
    }

    #[timed(indexing_documents)]
    pub fn freeze(self) -> crate::segment::ImmutableSegment {
        let terms: BTreeMap<_, _> = self.terms.into_iter().collect();
        let doc_lengths: BTreeMap<_, _> = self.doc_lengths.into_iter().collect();
        let field_stats = self.field_stats;

        crate::segment::ImmutableSegment::new(
            terms,
            doc_lengths,
            field_stats,
            self.numeric_points.build(),
            self.array_row_index,
        )
    }
    //tracks min and max doc id in this segment
    fn track_doc_id(&mut self, doc_id: DocId) {
        self.min_doc_id = Some(self.min_doc_id.map_or(doc_id, |m| m.min(doc_id)));
        self.max_doc_id = Some(self.max_doc_id.map_or(doc_id, |m| m.max(doc_id)));
    }

    pub fn doc_id_range(&self) -> Option<(DocId, DocId)> {
        match (self.min_doc_id, self.max_doc_id) {
            (Some(min), Some(max)) => Some((min, max)),
            _ => None,
        }
    }

    pub fn add_token(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        position: u32
    ) {
        self.add_token_weighted(term, xpath, doc_id, position, 1);
    }

    #[timed(indexing_documents)]
    pub fn add_token_weighted(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        position: u32,
        weight: u16
    ) {
        self.track_doc_id(doc_id);
        // one position added to a posting: doc_id + one u32 position, plus
        // per-entry posting overhead (weight, small header). Approximate —
        // this doesn't need to be exact, just proportional to real growth.
        self.estimated_bytes +=
            std::mem::size_of::<DocId>() + std::mem::size_of::<u32>() + std::mem::size_of::<u16>();

        match self.terms.entry(TermKey::new(term, xpath)) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.get_mut().insert(doc_id, position, weight);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                self.estimated_bytes += slot.key().term.len() + std::mem::size_of::<TermKey>();
                slot.insert(PostingList::new())
                    .insert(doc_id, position, weight);
            }
        }
    }

    pub fn add_posting_weighted(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        positions: Vec<u32>,
        weight: u16
    ) {
        self.track_doc_id(doc_id);
        self.estimated_bytes +=
            std::mem::size_of::<DocId>() +
            std::mem::size_of::<u16>() +
            positions.len() * std::mem::size_of::<u32>();

        match self.terms.entry(TermKey::new(term, xpath)) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.get_mut().insert_posting(doc_id, positions, weight);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                self.estimated_bytes += slot.key().term.len() + std::mem::size_of::<TermKey>();
                slot.insert(PostingList::new()).insert_posting(doc_id, positions, weight);
            }
        }
    }

    pub fn lookup(&self, term: &str, xpath: XPathId) -> Option<&PostingList> {
        self.terms.get(&TermKey::new(term, xpath))
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }
    pub fn estimated_size_bytes(&self) -> usize {
        self.estimated_bytes
    }

    #[timed(indexing_documents)]
    pub fn add_document(&mut self, analyzer: &Analyzer, doc_id: DocId, xpath: XPathId, text: &str) {
        self.track_doc_id(doc_id);
        for token in analyzer.analyze(text) {
            self.add_token(token.text, xpath, doc_id, token.position);
        }
    }

    // For each document part we count how often each term appears.
    // Every occurance of that term receives the same part-level weight
    // wegith = min(part_min_weight + occurances_in_part, part_max_weight)
    // this is exactly where the policy matters, it determines the minimum and maximum weight of
    // the weighting this specific part or xml field in the document allows.
    // The postinglist stores all positions for phrase/proximity search while
    // the posting weight represents the terms importance in this document part.
    #[timed(indexing_documents)]
    pub fn add_document_weighted(
        &mut self,
        analyzer: &Analyzer,
        doc_id: DocId,
        xpath: XPathId,
        text: &str,
        min_weight: u16,
        max_weight: u16
    ) {
        self.track_doc_id(doc_id);
        let tokens = analyzer.analyze(text);

        let len = tokens.len().min(u32::MAX as usize) as u32;

        self.doc_lengths.insert((doc_id, xpath), len);

        let old = self.doc_lengths.insert((doc_id, xpath), len);
        let stats = self.field_stats.entry(xpath).or_default();
        match old {
            Some(old_len) => {
                stats.total_doc_len =
                    stats.total_doc_len.saturating_sub(old_len as u64) + (len as u64);
            }
            None => {
                stats.doc_count += 1;
                stats.total_doc_len += len as u64;
            }
        }

        let mut grouped = ahash::HashMap::<String, Vec<u32>>::with_capacity(tokens.len());

        for token in tokens {
            grouped.entry(token.text).or_default().push(token.position);
        }

        for (term, positions) in grouped {
            let occurrences = positions.len().min(u16::MAX as usize) as u16;
            let weight = min_weight.saturating_add(occurrences).min(max_weight);

            self.add_posting_weighted(term, xpath, doc_id, positions, weight);
        }
    }

    #[timed(indexing_documents)]
    pub fn add_indexed_document(&mut self, analyzer: &Analyzer, document: &IndexedDocument) {
        //main document
        self.track_doc_id(document.doc_id);
        self.add_parts_and_points(
            analyzer,
            document.doc_id,
            &document.parts,
            &document.numeric_points,
        );

        //each array in documents
        for row in &document.array_rows {
            self.array_row_index
                .push_row(row.array_row_id, document.doc_id, row.parent);

            self.add_parts_and_points(analyzer, row.array_row_id, &row.parts, &row.numeric_points);
        }
    }

    //helper
    fn add_parts_and_points(
        &mut self,
        analyzer: &Analyzer,
        target_id: DocId, //array row id uses the same type for id
        parts: &[DocumentPart],
        numeric_points: &[NumericPoint],
    ) {
        for part in parts {
            if part.exact {
                self.add_exact_weighted(
                    target_id,
                    part.xpath,
                    &part.text,
                    part.weight.min,
                    part.weight.max
                );
            } else {
                self.add_document_weighted(
                    analyzer,
                    target_id,
                    part.xpath,
                    &part.text,
                    part.weight.min,
                    part.weight.max
                );
            }
        }

        for point in numeric_points {
            self.numeric_points
                .insert(point.xpath, point.value, target_id);
        }
    }

    //basically add_document without lowercasing stemming
    #[timed(indexing_documents)]
    pub fn add_exact_weighted(
        &mut self,
        doc_id: DocId,
        xpath: XPathId,
        text: &str,
        min_weight: u16,
        max_weight: u16
    ) {
        self.track_doc_id(doc_id);
        let words: Vec<&str> = text.split_whitespace().collect();
        let len = words.len().min(u32::MAX as usize) as u32;

        self.doc_lengths.insert((doc_id, xpath), len);

        let old = self.doc_lengths.insert((doc_id, xpath), len);
        let stats = self.field_stats.entry(xpath).or_default();
        match old {
            Some(old_len) => {
                stats.total_doc_len =
                    stats.total_doc_len.saturating_sub(old_len as u64) + (len as u64);
            }
            None => {
                stats.doc_count += 1;
                stats.total_doc_len += len as u64;
            }
        }
        let mut grouped = ahash::HashMap::<&str, Vec<u32>>::default();
        for (position, word) in words.iter().enumerate() {
            grouped
                .entry(word)
                .or_default()
                .push(position as u32);
        }

        for (term, positions) in grouped {
            let occurrences = positions.len().min(u16::MAX as usize) as u16;
            let weight = min_weight.saturating_add(occurrences).min(max_weight);
            self.add_posting_weighted(term, xpath, doc_id, positions, weight);
        }
    }

    pub fn lookup_or_empty(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup(term, xpath).cloned().unwrap_or_default()
    }

    #[timed(search)]
    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let mut items = Vec::new();

        for (key, postings) in &self.terms {
            if key.xpath == xpath && key.term.starts_with(prefix) {
                items.extend_from_slice(postings.items());
            }
        }

        PostingList::from_items(items)
    }

    #[timed(search)]
    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        if pattern.is_prefix_only() {
            return self.lookup_prefix(pattern.prefix(), xpath);
        }

        let mut items = Vec::new();

        for (key, postings) in &self.terms {
            if key.xpath == xpath && pattern.matches(&key.term) {
                items.extend_from_slice(postings.items());
            }
        }

        PostingList::from_items(items)
    }

    //TEST

    #[timed(indexing_documents)]
    pub fn merge_from(&mut self, newer: MemIndex) {
        // Capture this before we start moving fields out of `newer`.
        let newer_doc_range = newer.doc_id_range();

        for (key, list) in newer.terms {
            match self.terms.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().append(list);
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(list);
                }
            }
        }
        self.doc_lengths.extend(newer.doc_lengths);
        for (xpath, stats) in newer.field_stats {
            self.field_stats.entry(xpath).or_default().add(&stats);
        }
        self.numeric_points.merge(newer.numeric_points);
        self.array_row_index.merge_from(&newer.array_row_index);
        self.estimated_bytes += newer.estimated_bytes;

        if let Some((min, max)) = newer_doc_range {
            self.track_doc_id(min);
            self.track_doc_id(max);
        }
    }
}
