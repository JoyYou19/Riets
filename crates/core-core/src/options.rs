use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use core_index::lsm::config::IndexRuntimeConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseOptions {
    pub runtime: IndexRuntimeConfig,
    pub enable_background_compaction: bool,
    pub compaction_interval: Duration,
    pub bootable: bool,
}

impl DatabaseOptions {
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let toml_string =
            toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, toml_string)
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let toml_string = fs::read_to_string(path)?;
        toml::from_str(&toml_string).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        match Self::load_from_file(&path) {
            Ok(options) => options,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    //WARN: user for invalid config
                   /*  tracing::warn!(
                        path=%path.as_ref().display(),
                        error=%e,
                        "Could not load options from the path. Using defaults.",
                    );
                    */
                }
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
            bootable: false,
        }
    }
}
