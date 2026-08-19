use std::{collections::HashMap, io, path::Path};

use core_core::shard_manager::ShardManager;
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

        let manager = match ShardManager::load(path) {
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
