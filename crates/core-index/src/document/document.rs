use roaring::RoaringTreemap;

use crate::{
    document::policy::WeightInterval,
    numeric_values::NumericValue,
    types::{ArrayRowId, DocId, XPathId},
};

// Represents one entry in the database documents
#[derive(Debug, Clone)]
pub struct IndexedDocument {
    pub doc_id: DocId,
    pub parts: Vec<DocumentPart>,
    pub numeric_points: Vec<NumericPoint>,
    pub array_rows: Vec<ArrayRow>,
    pub bool_points: Vec<BoolPoint>,
}

#[derive(Debug, Clone)]
pub struct ArrayRow {
    pub array_row_id: ArrayRowId,
    pub parent: Option<ArrayRowId>,
    pub parts: Vec<DocumentPart>,
    pub numeric_points: Vec<NumericPoint>,
    pub bool_points: Vec<BoolPoint>,
}

impl ArrayRow {
    pub fn new(array_row_id: ArrayRowId, parent: Option<ArrayRowId>) -> Self {
        Self {
            array_row_id,
            parent,
            parts: Vec::new(),
            numeric_points: Vec::new(),
            bool_points: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NumericPoint {
    pub xpath: XPathId,
    pub value: NumericValue,
}

#[derive(Debug, Default, Clone)]
pub struct BoolPoint {
    pub xpath: XPathId,
    pub value: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoolField {
    pub true_ids: RoaringTreemap,
    pub false_ids: RoaringTreemap,
}

impl IndexedDocument {
    pub fn new(doc_id: DocId) -> Self {
        Self {
            doc_id,
            parts: Vec::new(),
            numeric_points: Vec::new(),
            array_rows: Vec::new(),
            bool_points: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPart {
    pub xpath: XPathId,
    pub text: String,
    pub weight: WeightInterval,
    pub exact: bool,
    pub gap: u32,              // gap of joined arr elements
    pub segments: Vec<String>, // element texts for flattened repeated fields
}

impl DocumentPart {
    pub fn single(xpath: XPathId, text: String, weight: WeightInterval, exact: bool) -> Self {
        Self {
            xpath,
            text,
            weight,
            exact,
            segments: Vec::new(),
            gap: 0,
        }
    }

    pub fn flattened(
        xpath: XPathId,
        segments: Vec<String>,
        weight: WeightInterval,
        exact: bool,
        gap: u32,
    ) -> Self {
        Self {
            xpath,
            text: String::new(),
            weight,
            exact,
            segments,
            gap,
        }
    }

    pub fn is_flattened(&self) -> bool {
        !self.segments.is_empty()
    }
}
