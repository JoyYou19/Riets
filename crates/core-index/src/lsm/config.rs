use crate::lsm::compaction::CompactionConfig;

#[derive(Debug, Clone)]
pub struct IndexRuntimeConfig {
    pub flush_threshold: usize,
    pub indexing_batch_size: usize,
    pub compaction: CompactionConfig,
}

impl Default for IndexRuntimeConfig {
    fn default() -> Self {
        Self {
            flush_threshold: 100_000,
            indexing_batch_size: 100_000,
            compaction: CompactionConfig::default(),
        }
    }
}
