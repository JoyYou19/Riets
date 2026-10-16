use crate::types::{DocId, Position};

/*
* A posting is just a inverted-index data structure, it stores the Posting
* at the docid with positions and weight
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    pub doc_id: DocId,
    pub positions: Vec<Position>,
    pub weight: u16,
}

impl Posting {
    pub fn new(doc_id: DocId, positions: Vec<Position>) -> Self {
        Self {
            doc_id,
            positions,
            weight: 0,
        }
    }

    pub fn with_weight(doc_id: DocId, positions: Vec<Position>, weight: u16) -> Self {
        Self {
            doc_id,
            positions,
            weight,
        }
    }
}
