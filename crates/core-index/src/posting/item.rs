use smallvec::SmallVec;

use crate::types::{DocId, Position};

/// A posting's positions. Most words occur a few times in a field, so up to four positions live
/// inside the posting itself: decoding a list then allocates nothing per posting.
pub type Positions = SmallVec<[Position; 4]>;

/*
* A posting is just a inverted-index data structure, it stores the Posting
* at the docid with positions and weight
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    pub doc_id: DocId,
    pub positions: Positions,
    pub weight: u16,
}

impl Posting {
    pub fn new(doc_id: DocId, positions: impl Into<Positions>) -> Self {
        Self {
            doc_id,
            positions: positions.into(),
            weight: 0,
        }
    }

    pub fn with_weight(doc_id: DocId, positions: impl Into<Positions>, weight: u16) -> Self {
        Self {
            doc_id,
            positions: positions.into(),
            weight,
        }
    }
}
