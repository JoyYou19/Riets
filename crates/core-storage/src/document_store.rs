use core_index::types::DocId;
use core_protocol::format::Format;
use std::{io, sync::Arc};

pub type ExternalDocId = String;
pub type InternalDocId = DocId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDocument {
    pub external_id: ExternalDocId,
    pub internal_id: InternalDocId,

    // Format of the original document, JSON/XML
    pub format: Format,
    // Storing the original document as bytes from any of the formats
    pub source: Arc<[u8]>,
}

pub trait DocumentStore {
    fn put(&mut self, doc: StoredDocument) -> std::io::Result<()>;
    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> std::io::Result<()> {
        for doc in docs {
            self.put(doc)?;
        }

        Ok(())
    }
    fn get(&self, external_id: &str) -> std::io::Result<Option<StoredDocument>>;
    fn delete(&mut self, external_id: &str) -> std::io::Result<()>;
    fn max_internal_id(&self) -> DocId;

    fn document_count(&self) -> usize;
    fn contains(&self, external_id: &str) -> io::Result<bool>;
    fn sync(&self) -> std::io::Result<()>;

    // WARN: For now we can load all docs, in the future we will need a for each function
    fn all_documents(&self) -> io::Result<Vec<StoredDocument>>;

    fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()>;
}
