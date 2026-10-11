use std::collections::HashMap;

use core_index::{
    search::SearchStats,
    types::{DocId, XPathId},
};

/// Live document count and total token length of one field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldTotals {
    pub doc_count: u64,
    pub total_doc_len: u64,
}

/// What a query leaf looks up in a field.
///
/// The document frequency BM25 uses for a leaf is the length of the posting list the leaf reads,
/// so a leaf and its statistics are always keyed the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum LeafKey {
    Term(String),
    Prefix(String),
    Wildcard(String),
    Phrase(Vec<String>),
    Exact(String),
}

/// The BM25 statistics of one query: the totals of every field it scores and the document
/// frequency of every leaf.
///
/// Every shard reports its own values. A document lives in exactly one shard, so the sum over all
/// shards (`merge`) is what one shard holding every document would report. Scoring with that sum
/// gives a document the same score wherever it is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusStats {
    fields: HashMap<XPathId, FieldTotals>,
    doc_freqs: HashMap<XPathId, HashMap<LeafKey, u64>>,
}

impl CorpusStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_field(&mut self, xpath: XPathId, totals: FieldTotals) {
        self.fields.insert(xpath, totals);
    }

    pub(crate) fn insert_doc_freq(&mut self, xpath: XPathId, key: LeafKey, doc_freq: u64) {
        self.doc_freqs
            .entry(xpath)
            .or_default()
            .insert(key, doc_freq);
    }

    /// Adds another shard's statistics to these.
    pub fn merge(&mut self, other: &CorpusStats) {
        for (&xpath, totals) in &other.fields {
            let entry = self.fields.entry(xpath).or_default();
            entry.doc_count = entry.doc_count.saturating_add(totals.doc_count);
            entry.total_doc_len = entry.total_doc_len.saturating_add(totals.total_doc_len);
        }
        for (&xpath, keys) in &other.doc_freqs {
            let field = self.doc_freqs.entry(xpath).or_default();
            for (key, &doc_freq) in keys {
                let entry = field.entry(key.clone()).or_default();
                *entry = entry.saturating_add(doc_freq);
            }
        }
    }

    pub fn field(&self, xpath: XPathId) -> Option<FieldTotals> {
        self.fields.get(&xpath).copied()
    }

    /// Document frequency of a leaf, saturated to the `u32` the scorer takes; `None` when the
    /// statistics do not cover the leaf.
    pub(crate) fn doc_freq(&self, xpath: XPathId, key: &LeafKey) -> Option<u32> {
        self.doc_freqs
            .get(&xpath)
            .and_then(|keys| keys.get(key))
            .map(|&doc_freq| u32::try_from(doc_freq).unwrap_or(u32::MAX))
    }
}

/// The statistics a query is scored with: the search-wide totals where there are any, the
/// index's own otherwise. Document lengths always come from the index, they belong to its
/// documents.
pub(crate) struct ScoringStats<'a, S> {
    pub(crate) index: &'a S,
    pub(crate) corpus: Option<&'a CorpusStats>,
}

impl<S: SearchStats> SearchStats for ScoringStats<'_, S> {
    fn doc_count(&self, xpath: XPathId) -> u64 {
        match self.corpus.and_then(|corpus| corpus.field(xpath)) {
            Some(totals) => totals.doc_count,
            None => self.index.doc_count(xpath),
        }
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        match self.corpus.and_then(|corpus| corpus.field(xpath)) {
            Some(totals) => totals.total_doc_len,
            None => self.index.total_doc_len(xpath),
        }
    }

    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.index.doc_len(doc_id, xpath)
    }

    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.index.doc_range()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_adds_every_shard() {
        let mut first = CorpusStats::new();
        first.insert_field(
            1,
            FieldTotals {
                doc_count: 3,
                total_doc_len: 30,
            },
        );
        first.insert_doc_freq(1, LeafKey::Term("penguin".into()), 2);

        let mut second = CorpusStats::new();
        second.insert_field(
            1,
            FieldTotals {
                doc_count: 5,
                total_doc_len: 12,
            },
        );
        second.insert_doc_freq(1, LeafKey::Term("penguin".into()), 1);
        second.insert_doc_freq(1, LeafKey::Prefix("pengu".into()), 4);

        let mut corpus = CorpusStats::new();
        corpus.merge(&first);
        corpus.merge(&second);

        assert_eq!(
            corpus.field(1),
            Some(FieldTotals {
                doc_count: 8,
                total_doc_len: 42
            })
        );
        assert_eq!(corpus.field(2), None);
        assert_eq!(
            corpus.doc_freq(1, &LeafKey::Term("penguin".into())),
            Some(3)
        );
        assert_eq!(
            corpus.doc_freq(1, &LeafKey::Prefix("pengu".into())),
            Some(4)
        );
        assert_eq!(corpus.doc_freq(1, &LeafKey::Term("emperor".into())), None);
        assert_eq!(corpus.doc_freq(2, &LeafKey::Term("penguin".into())), None);
    }

    #[test]
    fn doc_freq_saturates_to_u32() {
        let mut corpus = CorpusStats::new();
        corpus.insert_doc_freq(7, LeafKey::Term("a".into()), u64::from(u32::MAX) + 10);
        assert_eq!(
            corpus.doc_freq(7, &LeafKey::Term("a".into())),
            Some(u32::MAX)
        );
    }
}
