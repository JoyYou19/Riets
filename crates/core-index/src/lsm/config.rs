use serde::{Deserialize, Serialize};

use crate::lsm::compaction::CompactionConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub struct IndexRuntimeConfig {
    pub flush_threshold: usize,

    // Number of documents per segment
    pub indexing_batch_size: usize,


    pub compaction: CompactionConfig,
}

impl Default for IndexRuntimeConfig {
    fn default() -> Self {
        Self {
            flush_threshold: 128 * 1024 * 1024, //TAGAD TIE IR MB NEVIS TERMS, labaks memory control

            indexing_batch_size: 100_000,
            compaction: CompactionConfig::default(),
        }
    }
}
