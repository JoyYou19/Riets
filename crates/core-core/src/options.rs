use core_index::lsm::config::IndexRuntimeConfig;
use serde::{ Deserialize, Serialize };
use std::path::{ Path, PathBuf };
use std::time::Duration;
use std::{ fs, io };


#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
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
}
impl DatabaseOptions {
    pub const CONFIG_FILE_NAME: &'static str = "config.toml";

    fn config_path(root: &Path) -> PathBuf {
        root.join(Self::CONFIG_FILE_NAME)
    }

    pub fn save_to_file(&self, root: impl AsRef<Path>) -> io::Result<()> {
        let toml_string = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(Self::config_path(root.as_ref()), toml_string)
    }
    pub fn load_from_file(root: impl AsRef<Path>) -> io::Result<Self> {
        let path = Self::config_path(root.as_ref());

        let contents = fs
            ::read_to_string(&path)
            .map_err(|e| {
                io::Error::new(e.kind(), format!("database config {}: {e}", path.display()))
            })?;

        toml::from_str(&contents).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("database config is invalid: {}", e.message())
            )
        })
    }
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            runtime: IndexRuntimeConfig::default(),
            enable_background_compaction: true,
            compaction_interval: Duration::from_secs(10),
            dead_file_treshold: 0.5,
            incremental_backup_interval: Duration::from_secs(3600),
            full_backup_interval: Duration::from_hours(24),
            backup_lifetime: Duration::from_hours(24 * 7),
            bootable: true,
            sync_mode: core_storage::wal::SyncMode::Manual,
        }
    }
}
