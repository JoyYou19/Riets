use std::io;
use std::sync::atomic::AtomicU64;
use std::{path::PathBuf, sync::Arc, sync::atomic::AtomicBool};

use core_storage::binary_store::{DEFAULT_DOC_CACHE_CAPACITY, DocLocation, read_document_at_path};
use core_timing::timed;
use dashmap::DashMap;

use core_index::lsm::snapshot::SharedIndexSnapshot;
use core_index::types::DocId;
use core_storage::document_store::StoredDocument;
use moka::sync::Cache;
use tokio::task;

pub struct SharedShardState {
    pub snapshot: SharedIndexSnapshot,
    pub docs: Cache<String, StoredDocument>,
    pub locations: Arc<DashMap<String, DocLocation>>,
    pub internal_to_external: Arc<DashMap<DocId, String>>,
    pub is_running: AtomicBool,
    pub is_clearing: AtomicBool,
    pub is_backing_up: AtomicBool,
    pub is_restoring: AtomicBool,
    pub last_backup_at: AtomicU64,
    pub last_backup_id: std::sync::RwLock<Option<String>>,
    pub root: PathBuf,
}

impl SharedShardState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            snapshot: SharedIndexSnapshot::empty(),
            docs: Cache::builder()
                .max_capacity(DEFAULT_DOC_CACHE_CAPACITY)
                .build(),
            locations: Arc::new(DashMap::new()),
            internal_to_external: Arc::new(DashMap::new()),
            is_running: AtomicBool::new(false),
            is_clearing: AtomicBool::new(false),
            is_backing_up: AtomicBool::new(false),
            is_restoring: AtomicBool::new(false),
            last_backup_at: AtomicU64::new(0),
            last_backup_id: std::sync::RwLock::new(None),
            root,
        }
    }

    #[timed(database_lifecycle)]
    pub fn release(&self) {
        //INFO: release EVERYTHING RAAAH: -Normunds nevis Kristians
        self.snapshot.clear();

        self.docs.invalidate_all();
        self.docs.run_pending_tasks();

        self.locations.clear();
        self.locations.shrink_to_fit();

        self.internal_to_external.clear();
        self.internal_to_external.shrink_to_fit();
    }

    #[timed(retrieve_opps)]
    pub async fn get_document(&self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        if let Some(doc) = self.docs.get(external_id) {
            return Ok(Some(doc));
        }

        let Some(loc) = self.locations.get(external_id).map(|r| *r.value()) else {
            return Ok(None);
        };

        //WARN: bellow is a todo comment lmao
        //TODO: pass this to the shared state in a pretty way, this is a placeholder
        let path = self.root.join("documents");
        let doc =
            task::spawn_blocking(move || read_document_at_path(&path, loc.segment, loc.offset))
                .await
                .map_err(|e| io::Error::other(format!("disk read task panicked: {e}")))??;

        self.docs.insert(external_id.to_string(), doc.clone());
        Ok(Some(doc))
    }
    ///VECTOR THINGS
        /// Synchronous read by internal id for background work. Unlike `get_document` it does not
    /// fill the document cache, so bulk reads do not evict documents that searches use.
    /// Ok(None) means the document is gone (deleted, or moved by store compaction).
    pub fn read_document_by_doc_id(&self, doc_id: DocId) -> io::Result<Option<StoredDocument>> {
        let Some(external_id) = self
            .internal_to_external
            .get(&doc_id)
            .map(|entry| entry.value().clone())
        else {
            return Ok(None);
        };
        if let Some(doc) = self.docs.get(&external_id) {
            return Ok(Some(doc));
        }
        let Some(loc) = self.locations.get(&external_id).map(|entry| *entry.value()) else {
            return Ok(None);
        };
        match read_document_at_path(&self.root.join("documents"), loc.segment, loc.offset) {
            Ok(doc) => Ok(Some(doc)),
            // The store may have compacted that file away since the location was read.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}
