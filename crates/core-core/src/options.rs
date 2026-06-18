use std::time::Duration;

use core_index::lsm::config::IndexRuntimeConfig;

#[derive(Debug, Clone)]
pub struct DatabaseOptions {
    pub runtime: IndexRuntimeConfig,
    pub enable_background_compaction: bool,
    pub compaction_interval: Duration,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            runtime: IndexRuntimeConfig::default(),
            enable_background_compaction: true,
            compaction_interval: Duration::from_secs(1),
        }
    }
}
