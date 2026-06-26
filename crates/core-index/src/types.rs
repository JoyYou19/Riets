// Keep track of all types, will be needed for binary in the future
pub type DocId = u64;
pub type Position = u32;
pub type XPathId = u32;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermKey {
    pub xpath: XPathId,
    pub term: String,
}

impl TermKey {
    pub fn new(term: impl Into<String>, xpath: XPathId) -> Self {
        Self {
            xpath,
            term: term.into(),
        }
    }
}

// Cached entry of the field stats, once computed in the index, will store smaller information
// TODO: Need to persist this on the disk probably, could be a good small size increase, but decent
// performance benefit
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldStats {
    pub doc_count: u64,
    pub total_doc_len: u64,
}
