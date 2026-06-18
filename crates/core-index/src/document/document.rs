use crate::{
    document::policy::WeightInterval,
    types::{DocId, XPathId},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDocument {
    pub doc_id: DocId,
    pub parts: Vec<DocumentPart>,
}

impl IndexedDocument {
    pub fn new(doc_id: DocId) -> Self {
        Self {
            doc_id,
            parts: Vec::new(),
        }
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
        });

        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPart {
    pub xpath: XPathId,
    pub text: String,
    pub weight: WeightInterval,
}
