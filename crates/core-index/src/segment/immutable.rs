use std::collections::BTreeMap;

use crate::{
    posting::PostingList,
    search::SearchIndex,
    types::{TermKey, XPathId},
    wildcard::WildcardPattern,
};

// In-memory (keep in mind) segment that is supposed to be a frozen MemTable
//
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableSegment {
    terms: BTreeMap<TermKey, PostingList>,
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

impl ImmutableSegment {
    pub fn new(terms: BTreeMap<TermKey, PostingList>) -> Self {
        Self { terms }
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
