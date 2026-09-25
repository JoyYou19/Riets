use serde::{Deserialize, Serialize};

use crate::lsm::compaction::CompactionConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub struct IndexRuntimeConfig {
    pub flush_threshold: usize,

    // Number of documents per segment
    pub indexing_batch_size: usize,

    // Numver of batches built in parallel before we publish it
    pub indexing_window_size: usize,

    pub compaction: CompactionConfig,
}

impl Default for IndexRuntimeConfig {
    fn default() -> Self {
        Self {
            flush_threshold:128 * 1024 * 1024, //TAGAD TIE IR MB NEVIS TERMS, labaks memory control
            indexing_window_size: 5,
            indexing_batch_size: 100_000,
            compaction: CompactionConfig::default(),
        }
    }
}
