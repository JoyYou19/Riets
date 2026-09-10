use crate::{
    numeric_columns::{NumericBound, NumericValue},
    posting::PostingList,
    types::{DocId, XPathId},
    wildcard::WildcardPattern,
};

// Every searchable segment should implement this, simply functions that we are going to need for
// every type of segment either it is Immutable, Snapshot or in Memory
pub trait SearchIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList;
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList;
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList;
}

// How are these documents going to be scored? Used for BM25, is needed for the math equation
pub trait SearchStats {
    fn doc_count(&self, xpath: XPathId) -> u64;
    fn total_doc_len(&self, xpath: XPathId) -> u64;
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32>;

    fn avg_doc_len(&self, xpath: XPathId) -> f32 {
        let count = self.doc_count(xpath);

        if count == 0 {
            return 0.0;
        }

        self.total_doc_len(xpath) as f32 / count as f32
    }
}

pub trait SearchColumns {
    //docs within the [lo, hi]
    fn column_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> PostingList;

    //all (doc_id, value) pairs in a column sorting pirposes
    fn column_values(&self, xpath: XPathId) -> Vec<(DocId, NumericValue)>;
}

pub trait SearchReader: SearchIndex + SearchStats + SearchColumns {}

impl<T> SearchReader for T where T: SearchIndex + SearchStats + SearchColumns {}
