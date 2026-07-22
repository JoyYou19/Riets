use std::{collections::HashMap, io, path::Path};

use core_core::CorelamoDatabase;

pub fn load_saved_databases(databases_dir: &Path) -> io::Result<HashMap<String, CorelamoDatabase>> {
    let mut databases = HashMap::new();

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
                tracing::error!(name=%name,error=%e,"database failed to load");
                continue;
            }
        };

        if db.options().bootable {
            match db.start() {
                Ok(()) => tracing::info!(name=%name,"started database"),
                Err(e) => tracing::error!(name=%name, error=%e,"database loaded but failed to start:"),
            }
        } else {
            tracing::info!(name=%name,"loaded database");
        }

        databases.insert(name, db);
    }

    Ok(databases)
}
