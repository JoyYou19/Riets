use core_core::{CorelamoDatabase, DatabaseOptions, errors::CorelamoError};

use std::{collections::HashMap, path::PathBuf};

pub fn create_database(
    db_name: String,
    databases_dir: &PathBuf,
    databases: &HashMap<String, CorelamoDatabase>,
) -> Result<CorelamoDatabase, CorelamoError> {
    let path_to_database = databases_dir.join(&db_name);

    if databases.contains_key(&db_name) {
        return Err(CorelamoError::AlreadyExists(format!(
            "database '{db_name}' is already loaded"
        )));
    }

    if path_to_database.exists() {
        return Err(CorelamoError::AlreadyExists(format!(
            "database '{db_name}' already exists on disk"
        )));
    }

    CorelamoDatabase::open(&path_to_database, DatabaseOptions::default())
        .map(|db| {
            println!("created database: {db_name}");
            db
        })
        //TODO: replace when open returns CorelamoError
        .map_err(|e| CorelamoError::Internal(format!("failed to create database '{db_name}': {e}")))
}

pub fn load_saved_databases(
    databases_dir: &PathBuf,
) -> Result<HashMap<String, CorelamoDatabase>, CorelamoError> {
    if !databases_dir.exists() {
        println!("databases dir not found, creating...");
        std::fs::create_dir_all(databases_dir).map_err(CorelamoError::from)?;
        println!("no databases found");
        return Ok(HashMap::new());
    }

    let mut databases: HashMap<String, CorelamoDatabase> = HashMap::new();

    for entry in std::fs::read_dir(databases_dir).map_err(CorelamoError::from)? {
        let entry = entry.map_err(CorelamoError::from)?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let db_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();

        //TODO: save database settings to disk and not use default
        match CorelamoDatabase::open(&path, DatabaseOptions::default()) {
            Ok(db) => {
                println!("opened database: {db_name}");
                databases.insert(db_name, db);
            }
            Err(e) => {
                // non-fatal — log and skip, don't abort entire startup
                eprintln!("warning: failed to open database '{db_name}': {e}");
            }
        }
    }

    Ok(databases)
}
