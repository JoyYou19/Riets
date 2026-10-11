use std::collections::HashMap;

use core_index::{posting::PostingList, types::XPathId};

use core_index::fuzzy::FuzzyExpansion;

use crate::corpus_stats::{CorpusStats, FieldTotals, FuzzyKey, LeafKey};

/// What an index learned about a query while contributing to the statistics of a search: those
/// statistics and the posting lists it read to get them. The lists are kept so that counting
/// and ranking the query do not read them from the index a second time.
#[derive(Default)]
pub struct PreparedQuery {
    stats: CorpusStats,
    lists: HashMap<XPathId, HashMap<LeafKey, PostingList>>,
}

impl PreparedQuery {
    /// This index's share of the statistics of the search.
    pub fn stats(&self) -> &CorpusStats {
        &self.stats
    }

    pub(crate) fn insert_field(&mut self, xpath: XPathId, totals: FieldTotals) {
        self.stats.insert_field(xpath, totals);
    }

    pub(crate) fn insert_leaf(&mut self, xpath: XPathId, key: LeafKey, list: PostingList) {
        self.stats
            .insert_doc_freq(xpath, key.clone(), list.len() as u64);
        self.lists.entry(xpath).or_default().insert(key, list);
    }

    pub(crate) fn insert_fuzzy(
        &mut self,
        xpath: XPathId,
        key: FuzzyKey,
        expansions: Vec<FuzzyExpansion>,
    ) {
        self.stats.insert_fuzzy(xpath, key, expansions);
    }

    pub(crate) fn list(&self, xpath: XPathId, key: &LeafKey) -> Option<&PostingList> {
        self.lists.get(&xpath).and_then(|keys| keys.get(key))
    }
}
