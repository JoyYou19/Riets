use core_index::lsm::config::IndexRuntimeConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseOptions {
    pub runtime: IndexRuntimeConfig,
    pub enable_background_compaction: bool,
    pub compaction_interval: Duration,
    pub bootable: bool,
    pub shard_count: u16,
    pub backup_interval: Duration,
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
        let toml_string = fs::read_to_string(Self::config_path(root.as_ref()))?;
        toml::from_str(&toml_string).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    pub fn load_or_default(root: impl AsRef<Path>) -> Self {
        match Self::load_from_file(root.as_ref()) {
            Ok(options) => options,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {}
                //if not found its ok, use defaults either way we use defaults
                Self::default()
            }
        }
    }
}
impl Default for DatabaseOptions {
    fn default() -> Self {
        Self {
            runtime: IndexRuntimeConfig::default(),
            enable_background_compaction: true,
            compaction_interval: Duration::from_secs(1),

            //INFO: gnjau default jabut false
            //katru stundu
            backup_interval: Duration::from_secs(3600),
            shard_count: 4,
            bootable: true,
        }
    }
}
