use crate::{
    document::policy::WeightInterval,
    numeric_columns::NumericValue,
    types::{DocId, XPathId},
};

// Represents one entry in the database documents
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDocument {
    pub doc_id: DocId,
    pub parts: Vec<DocumentPart>,
    pub columns: Vec<ColumnPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPart {
    pub xpath: XPathId,
    pub value: NumericValue,
}

impl IndexedDocument {
    pub fn new(doc_id: DocId) -> Self {
        Self {
            doc_id,
            parts: Vec::new(),
            columns: Vec::new(),
        }
    }

    pub fn with_column(mut self, xpath: XPathId, value: NumericValue) -> Self {
        self.columns.push(ColumnPart { xpath, value });
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
