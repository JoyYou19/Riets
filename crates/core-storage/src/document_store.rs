use core_protocol::format::Format;
//logging
use core_logs::logger;
use slog::{info, Logger};
use std::{collections::BTreeMap, io};

use serde::{Deserialize, Serialize};

pub type ExternalDocId = String;
pub type InternalDocId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDocument {
    pub external_id: ExternalDocId,
    pub internal_id: InternalDocId,

    // Format of the original document, JSON/XML
    pub format: Format,
    // Storing the original document as bytes from any of the formats
    pub source: Vec<u8>,

    pub fields: BTreeMap<String, String>,
}

pub trait DocumentStore {
    fn put(&mut self, doc: StoredDocument) -> std::io::Result<()>;
    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> std::io::Result<()> {
        for doc in docs {
            self.put(doc)?;
        }

        Ok(())
    }
    fn get(&mut self, external_id: &str) -> std::io::Result<Option<StoredDocument>>;
    fn delete(&mut self, external_id: &str) -> std::io::Result<()>;
    fn max_internal_id(&self) -> u64;
    fn get_by_internal_id(&mut self, internal_id: u64) -> std::io::Result<Option<StoredDocument>>;

    fn document_count(&self) -> usize;
    fn contains(&self, external_id: &str) -> io::Result<bool>;

    // WARN: For now we can load all docs, in the future we will need a for each function
    fn all_documents(&self) -> io::Result<Vec<StoredDocument>>;

    fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()>;
}

#[derive(Debug)]
pub struct MemoryDocumentStore {
    docs: BTreeMap<String, StoredDocument>,
    internal_to_external: BTreeMap<u64, String>,
}

impl MemoryDocumentStore {
    pub fn new(name: &str) -> Self {
        //te bija self::Default()
        Self {
            docs: BTreeMap::default(),
            internal_to_external:BTreeMap::default(),
        }
    }
}

impl DocumentStore for MemoryDocumentStore {
    fn put(&mut self, doc: StoredDocument) -> std::io::Result<()> {
       
        self.internal_to_external
            .insert(doc.internal_id, doc.external_id.clone());

        self.docs.insert(doc.external_id.clone(), doc);

        Ok(())
    }

    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> std::io::Result<()> {
       
        for doc in docs {
            self.internal_to_external
                .insert(doc.internal_id, doc.external_id.clone());

            self.docs.insert(doc.external_id.clone(), doc);
        }

        Ok(())
    }

    fn get(&mut self, external_id: &str) -> std::io::Result<Option<StoredDocument>> {
        Ok(self.docs.get(external_id).cloned())
    }

    fn contains(&self, external_id: &str) -> io::Result<bool> {
        Ok(self.docs.contains_key(external_id))
    }

    fn delete(&mut self, external_id: &str) -> std::io::Result<()> {
       
        if let Some(doc) = self.docs.remove(external_id) {
            self.internal_to_external.remove(&doc.internal_id);
        }

        Ok(())
    }

    fn max_internal_id(&self) -> u64 {
        self.docs
            .values()
            .map(|doc| doc.internal_id)
            .max()
            .unwrap_or(0)
    }

    fn get_by_internal_id(&mut self, internal_id: u64) -> std::io::Result<Option<StoredDocument>> {
        let Some(external_id) = self.internal_to_external.get(&internal_id) else {
            return Ok(None);
        };

        Ok(self.docs.get(external_id).cloned())
    }

    fn document_count(&self) -> usize {
        self.docs.len()
    }

    fn all_documents(&self) -> io::Result<Vec<StoredDocument>> {
        let mut docs = Vec::new();

        self.for_each_document(&mut |doc| {
            docs.push(doc.clone());
            Ok(())
        })?;

        Ok(docs)
    }

    fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()> {
        for doc in self.docs.values() {
            f(doc)?;
        }

        Ok(())
    }
}
