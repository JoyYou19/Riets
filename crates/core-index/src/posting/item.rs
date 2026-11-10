use smallvec::SmallVec;

use crate::types::{DocId, Position};

pub type Positions = SmallVec<[Position; 4]>;

pub type Offsets = SmallVec<[(u32, u32); 4]>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    pub doc_id: DocId,
    pub positions: Positions,
    pub offsets: Offsets,
    pub weight: u16,
}

impl Posting {
    pub fn new(doc_id: DocId, positions: impl Into<Positions>) -> Self {
        Self {
            doc_id,
            positions: positions.into(),
            offsets: Offsets::new(),
            weight: 0,
        }
    }

    pub fn with_weight(doc_id: DocId, positions: impl Into<Positions>, weight: u16) -> Self {
        Self {
            doc_id,
            positions: positions.into(),
            offsets: Offsets::new(),
            weight,
        }
    }

    pub fn with_weight_and_offsets(
        doc_id: DocId,
        positions: impl Into<Positions>,
        offsets: impl Into<Offsets>,
        weight: u16,
    ) -> Self {
        Self {
            doc_id,
            positions: positions.into(),
            offsets: offsets.into(),
            weight,
        }
    }
}
