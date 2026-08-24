use std::{collections::HashMap, io, path::Path};

use core_core::shard_manager::ShardManager;
use core_protocol::errors::CorelamoError;
use slog::error;

pub fn load_saved_shard_managers(
    databases_dir: &Path,
) -> io::Result<HashMap<String, ShardManager>> {
    let mut databases = HashMap::new();
    let log = slog_scope::logger();

    if !databases_dir.exists() {
        std::fs::create_dir_all(databases_dir)?;
        return Ok(databases);
    }

    for entry in std::fs::read_dir(databases_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();

        let manager = match ShardManager::load(path, true) {
            Ok(mgr) => mgr,
            Err(e) => {
                error!(log,"database failed to load";"name"=>%name,"error"=>%e);
                continue;
            }
        };

        databases.insert(name, manager);
    }

    Ok(databases)
}

pub fn validate_db_name(name: &str) -> Result<(), CorelamoError> {
    if name.is_empty() {
        return Err(CorelamoError::InvalidData(
            "database name cannot be empty".to_string(),
        ));
    }
    if name.len() > 30 {
        return Err(CorelamoError::InvalidData(format!(
            "database name '{}' exceeds 30 characters",
            name
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CorelamoError::InvalidData(format!(
            "database name '{}' contains invalid characters",
            name
        )));
    }
    Ok(())
}
