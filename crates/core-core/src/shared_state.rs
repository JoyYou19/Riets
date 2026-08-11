use std::{path::PathBuf, sync::Arc, sync::atomic::AtomicBool};

use dashmap::DashMap;

use core_index::lsm::snapshot::SharedIndexSnapshot;
use core_index::types::DocId;
use core_storage::document_store::StoredDocument;

pub struct SharedShardState {
    pub snapshot: SharedIndexSnapshot,
    pub docs: Arc<DashMap<String, StoredDocument>>,
    pub internal_to_external: Arc<DashMap<DocId, String>>,
    pub is_running: AtomicBool,
    pub is_clearing: AtomicBool,
    pub root: PathBuf,
}

impl SharedShardState {
    pub fn new(root: PathBuf) -> Self {
        Self {
            snapshot: SharedIndexSnapshot::empty(),
            docs: Arc::new(DashMap::new()),
            internal_to_external: Arc::new(DashMap::new()),
            is_running: AtomicBool::new(false),
            is_clearing: AtomicBool::new(false),
            root,
        }
    }
}
