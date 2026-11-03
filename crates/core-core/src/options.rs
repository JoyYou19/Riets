use core_index::lsm::config::IndexRuntimeConfig;

use core_index::write_atomically;
use core_query::planner::TierKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use crate::embed::EmbeddingConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseOptions {
    pub runtime: IndexRuntimeConfig,
    pub enable_background_compaction: bool,
    pub compaction_interval: Duration,
    pub dead_file_treshold: f64,
    pub bootable: bool,
    pub incremental_backup_interval: Duration,
    pub full_backup_interval: Duration,
    pub backup_lifetime: Duration,
    #[serde(default)]
    pub sync_mode: core_storage::wal::SyncMode,
    #[serde(default)]
    pub embed: EmbeddingConfig,
    #[serde(default)]
    pub search: SearchOptions,

}
impl DatabaseOptions {
    pub const CONFIG_FILE_NAME: &'static str = "config.toml";

    fn config_path(root: &Path) -> PathBuf {
        root.join(Self::CONFIG_FILE_NAME)
    }

     pub fn save_to_file(&self, root: impl AsRef<Path>) -> io::Result<()> {
        let toml_string = toml::to_string_pretty(self).map_err(io::Error::other)?;
        write_atomically(&Self::config_path(root.as_ref()), toml_string.as_bytes())
    }
    pub fn load_from_file(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = Self::config_path(root.as_ref());

        let contents = fs::read_to_string(&path).map_err(|e| {
            io::Error::new(e.kind(), format!("database config {}: {e}", path.display()))
        })?;

        toml::from_str(&contents).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("database config is invalid: {}", e.message()),
            )
        })
    }
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            runtime: IndexRuntimeConfig::default(),
            enable_background_compaction: true,
            compaction_interval: Duration::from_secs(30),
            dead_file_treshold: 0.5,
            incremental_backup_interval: Duration::from_secs(3600),
            full_backup_interval: Duration::from_hours(24),
            backup_lifetime: Duration::from_hours(24 * 7),
            bootable: true,
            sync_mode: core_storage::wal::SyncMode::Manual,
            embed: EmbeddingConfig::default(),
            search:SearchOptions::default()
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchOptions {
    /// Rank of each cascade tier, 1 first. A tier left out is off. Empty: the default order.
    pub cascade: std::collections::BTreeMap<TierKind, u8>,
}

impl SearchOptions {


    fn default() -> Self {
        Self {
            cascade: TierKind::DEFAULT_ORDER
                .iter()
                .enumerate()
                .map(|(index, kind)| (*kind, (index + 1) as u8))
                .collect(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for (kind, rank) in &self.cascade {
            if !seen.insert(*rank) {
                return Err(format!("search.cascade: rank {rank} is given twice (at {kind:?})"));
            }
        }
        Ok(())
    }

    /// The tiers in the order they run.
    pub fn cascade_order(&self) -> Vec<TierKind> {
        if self.cascade.is_empty() {
            return TierKind::DEFAULT_ORDER.to_vec();
        }
        let mut ranked: Vec<(u8, TierKind)> = self.cascade.iter().map(|(k, r)| (*r, *k)).collect();
        ranked.sort_unstable();
        ranked.into_iter().map(|(_, kind)| kind).collect()
    }
}