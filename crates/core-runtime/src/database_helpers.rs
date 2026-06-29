use core_core::{CorelamoDatabase, DatabaseOptions};
use std::{collections::HashMap, io, path::PathBuf};

pub fn create_database(
    db_name: String,
    databases_dir: &PathBuf,
    databases: &HashMap<String, CorelamoDatabase>,
) -> io::Result<CorelamoDatabase> {
    let path_to_database = databases_dir.join(&db_name);

    if databases.contains_key(&db_name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("database '{db_name}' is already loaded"),
        ));
    }

    if path_to_database.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("database '{db_name}' already exists on disk"),
        ));
    }

    match CorelamoDatabase::open(&path_to_database, DatabaseOptions::default()) {
        Ok(db) => {
            println!("created database: {db_name}");
            Ok(db)
        }
        Err(e) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("failed to create database '{db_name}': {e}"),
        )),
    }
}

pub fn load_saved_databases(
    databases_dir: &PathBuf,
) -> io::Result<HashMap<String, CorelamoDatabase>> {
    if !databases_dir.exists() {
        println!("databases dir not found, creating...");
        std::fs::create_dir_all(databases_dir)?;
        println!("no databases found");
        return Ok(HashMap::new());
    }

    let mut databases: HashMap<String, CorelamoDatabase>;
    databases = HashMap::new();

    for entry in std::fs::read_dir(databases_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let db_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();

        //TODO: we need to save the database setting to disk and not use default
        //TODO + for policy we would need to apply the parents rule unless different specified
        //others do this too
        match CorelamoDatabase::open(&path, DatabaseOptions::default()) {
            Ok(db) => {
                println!("opened database: {db_name}");
                databases.insert(db_name, db);
            }
            Err(e) => {
                eprintln!("warning: failed to open database '{db_name}': {e}");
            }
        }
    }

    Ok(databases)
}
