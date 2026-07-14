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
                eprintln!("skipping database '{name}': failed to load: {e}");
                continue;
            }
        };

        if db.options().bootable {
            match db.start() {
                Ok(()) => println!("started database '{name}'"),
                Err(e) => eprintln!("loaded '{name}' but failed to start: {e} (left stopped)"),
            }
        } else {
            println!("loaded database '{name}'");
        }

        databases.insert(name, db);
    }

    Ok(databases)
}
