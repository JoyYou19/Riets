use crate::{
    document::policy::WeightInterval,
    numeric_values::NumericValue,
    types::{DocId, XPathId},
};

// Represents one entry in the database documents
#[derive(Debug, Clone)]
pub struct IndexedDocument {
    pub doc_id: DocId,
    pub parts: Vec<DocumentPart>,
    pub numeric_points: Vec<NumericPoint>,
}

#[derive(Debug, Clone)]
pub struct NumericPoint {
    pub xpath: XPathId,
    pub value: NumericValue,
}

impl IndexedDocument {
    pub fn new(doc_id: DocId) -> Self {
        Self {
            doc_id,
            parts: Vec::new(),
            numeric_points: Vec::new(),
        }
    }

    pub fn with_numeric_point(mut self, xpath: XPathId, value: NumericValue) -> Self {
        self.numeric_points.push(NumericPoint { xpath, value });
        self
    }

    pub fn with_part(
        mut self,
        xpath: XPathId,
        text: impl Into<String>,
        weight: WeightInterval,
    ) -> Self {
        self.parts.push(DocumentPart {
            xpath,
            text: text.into(),
            weight,
            exact: false,
        });
        self
    }

    pub fn with_exact(
        mut self,
        xpath: XPathId,
        text: impl Into<String>,
        weight: WeightInterval,
    ) -> Self {
        self.parts.push(DocumentPart {
            xpath,
            text: text.into(),
            weight,
            exact: true,
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPart {
    pub xpath: XPathId,
    pub text: String,
    pub weight: WeightInterval,
    pub exact: bool,
}
