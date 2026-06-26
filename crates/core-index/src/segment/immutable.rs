use std::collections::BTreeMap;

use crate::{
    posting::PostingList,
    search::{SearchIndex, SearchStats},
    types::{DocId, FieldStats, TermKey, XPathId},
    wildcard::WildcardPattern,
};

// In-memory (keep in mind) segment that is supposed to be a frozen MemTable
//
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableSegment {
    terms: BTreeMap<TermKey, PostingList>,
    doc_lengths: BTreeMap<(DocId, XPathId), u32>,
    field_stats: BTreeMap<XPathId, FieldStats>,
}

// Haha, if we want to search inside of this segment, it must implement, and we do
impl SearchIndex for ImmutableSegment {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup_or_empty(term, xpath)
    }

    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        ImmutableSegment::lookup_prefix(self, prefix, xpath)
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        ImmutableSegment::lookup_wildcard(self, pattern, xpath)
    }
}

impl SearchStats for ImmutableSegment {
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

impl ImmutableSegment {
    pub fn new(
        terms: BTreeMap<TermKey, PostingList>,
        doc_lengths: BTreeMap<(DocId, XPathId), u32>,
        field_stats: BTreeMap<XPathId, FieldStats>,
    ) -> Self {
        Self {
            terms,
            doc_lengths,
            field_stats,
        }
    }

    pub fn doc_lengths(&self) -> &BTreeMap<(DocId, XPathId), u32> {
        &self.doc_lengths
    }

    pub fn lookup(&self, term: &str, xpath: XPathId) -> Option<&PostingList> {
        self.terms.get(&TermKey::new(term, xpath))
    }

    pub fn terms(&self) -> &BTreeMap<TermKey, PostingList> {
        &self.terms
    }

    pub fn lookup_or_empty(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup(term, xpath).cloned().unwrap_or_default()
    }

    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let mut items = Vec::new();

        for (key, postings) in self.terms.range(TermKey::new(prefix, xpath)..) {
            if key.xpath != xpath || !key.term.starts_with(prefix) {
                break;
            }

            items.extend_from_slice(postings.items());
        }

        PostingList::from_items(items)
    }

    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        if pattern.is_prefix_only() {
            return self.lookup_prefix(pattern.prefix(), xpath);
        }

        let prefix = pattern.prefix();
        let range_start = TermKey::new(prefix, xpath);
        let mut items = Vec::new();

        for (key, postings) in self.terms.range(range_start..) {
            if key.xpath != xpath {
                break;
            }

            if !prefix.is_empty() && !key.term.starts_with(prefix) {
                break;
            }

            if pattern.matches(&key.term) {
                items.extend_from_slice(postings.items());
            }
        }

        PostingList::from_items(items)
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }
}
