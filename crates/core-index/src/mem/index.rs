use std::collections::{BTreeMap, HashMap};

use ahash::HashMapExt;
use core_timing::timed;

use crate::analyzer::analyzer::Analyzer;
use crate::document::IndexedDocument;
use crate::posting::PostingList;
use crate::search::{SearchIndex, SearchStats};
use crate::types::{DocId, FieldStats, TermKey, XPathId};
use crate::wildcard::WildcardPattern;

// Memory inverted index, the core of the index
#[derive(Debug, Default, Clone)]
pub struct MemIndex {
    terms: HashMap<TermKey, PostingList>,
    doc_lengths: HashMap<(DocId, XPathId), u32>,
    field_stats: BTreeMap<XPathId, FieldStats>,
}

impl SearchIndex for MemIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup_or_empty(term, xpath)
    }

    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.lookup_prefix(prefix, xpath)
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        self.lookup_wildcard(pattern, xpath)
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
}

impl MemIndex {
    pub fn new() -> Self {
        Self {
            terms: HashMap::new(),
            doc_lengths: HashMap::new(),
            field_stats: BTreeMap::new(),
        }
    }

    #[timed(indexing_documents)]
    pub fn freeze(self) -> crate::segment::ImmutableSegment {
        let terms: BTreeMap<_, _> = self.terms.into_iter().collect();
        let doc_lengths: BTreeMap<_, _> = self.doc_lengths.into_iter().collect();
        let field_stats = self.field_stats;

        crate::segment::ImmutableSegment::new(terms, doc_lengths, field_stats)
    }

    pub fn add_token(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        position: u32,
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
        weight: u16,
    ) {
        let key = TermKey::new(term, xpath);

        self.terms
            .entry(key)
            .or_default()
            .insert(doc_id, position, weight);
    }

    #[timed(indexing_documents)]
    pub fn add_posting_weighted(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        positions: Vec<u32>,
        weight: u16,
    ) {
        let key = TermKey::new(term, xpath);

        self.terms
            .entry(key)
            .or_default()
            .insert_posting(doc_id, positions, weight);
    }

    pub fn lookup(&self, term: &str, xpath: XPathId) -> Option<&PostingList> {
        self.terms.get(&TermKey::new(term, xpath))
    }

    #[timed(search)]
    pub fn lookup_all_xpaths(&self, term: &str) -> PostingList {
        let mut items = Vec::new();

        for (key, postings) in &self.terms {
            if key.term == term {
                items.extend_from_slice(postings.items());
            }
        }

        PostingList::from_items(items)
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    #[timed(indexing_documents)]
    pub fn add_document(&mut self, analyzer: &Analyzer, doc_id: DocId, xpath: XPathId, text: &str) {
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
        max_weight: u16,
    ) {
        let tokens = analyzer.analyze(text);

        let len = tokens.len().min(u32::MAX as usize) as u32;

        self.doc_lengths.insert((doc_id, xpath), len);

        let stats = self.field_stats.entry(xpath).or_default();
        stats.doc_count += 1;
        stats.total_doc_len += len as u64;

        let mut grouped = ahash::HashMap::<String, Vec<u32>>::new();

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
        for part in &document.parts {
            self.add_document_weighted(
                analyzer,
                document.doc_id,
                part.xpath,
                &part.text,
                part.weight.min,
                part.weight.max,
            );
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
}
