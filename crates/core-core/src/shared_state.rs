use std::io;
use std::sync::atomic::AtomicU64;
use std::{path::PathBuf, sync::Arc, sync::atomic::AtomicBool};

use core_storage::binary_store::{DocLocation, read_document_at_path};
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
            docs: Cache::builder().max_capacity(10).build(),
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
        let path = self.root.join("documents.bin");
        let doc = task::spawn_blocking(move || read_document_at_path(&path, loc.offset))
            .await
            .map_err(|e| io::Error::other(format!("disk read task panicked: {e}")))??;

        self.docs.insert(external_id.to_string(), doc.clone());
        Ok(Some(doc))
    }
}
