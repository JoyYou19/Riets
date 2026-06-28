use core_core::{CorelamoDatabase, DatabaseOptions};
use std::{collections::HashMap, io, path::PathBuf};

pub fn create_database(db_name: String, path: &PathBuf) -> io::Result<CorelamoDatabase> {
    //TODO "DATABASE ALREADY EXISTS CHECK ERROR"
    match CorelamoDatabase::open(&path, DatabaseOptions::default()) {
        Ok(db) => {
            println!("opened database: {db_name}");
            return Ok(db);
        }
        Err(e) => {
            eprintln!("warning: failed to create database '{db_name}': {e}");
            todo!();
        }
    }
}

pub fn load_databases(databases_dir: &PathBuf) -> io::Result<HashMap<String, CorelamoDatabase>> {
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
