use std::{collections::HashMap, io, path::Path};

use core_core::CorelamoDatabase;
use slog::{error, info};

pub fn load_saved_databases(databases_dir: &Path) -> io::Result<HashMap<String, CorelamoDatabase>> {
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

        let mut db = match CorelamoDatabase::load(&path) {
            Ok(db) => db,
            Err(e) => {
                error!(log,"database failed to load";"name"=>%name,"error"=>%e);
                continue;
            }
        };

        if db.options().bootable {
            match db.start() {
                Ok(()) => info!(log,"started database";"name"=>%name),
                Err(e) => {
                    error!(log,"database loaded but failed to start:";"name"=>%name, "error"=>%e)
                }
            }
        } else {
            info!(log,"loaded database";"name"=>%name);
        }

        databases.insert(name, db);
    }

    Ok(databases)
}
