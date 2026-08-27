use std::{collections::HashMap, io, path::Path};

use core_core::shard_manager::ShardManager;
use core_protocol::errors::CorelamoError;
use core_timing::timed;
use serde_json::json;
use slog::error;

#[timed(database_lifecycle)]
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

//bellow are functions for determining what takes up how much disk space for a database
fn file_size_opt(path: &std::path::Path) -> Result<Option<u64>, CorelamoError> {
    if !path.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(path)
        .map_err(|e| CorelamoError::Internal(format!("failed to stat {}: {e}", path.display())))?;
    Ok(Some(meta.len()))
}

fn dir_total_size(path: &std::path::Path) -> Result<u64, CorelamoError> {
    let mut total = 0u64;
    let entries = std::fs::read_dir(path).map_err(|e| {
        CorelamoError::Internal(format!("failed to read dir {}: {e}", path.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            CorelamoError::Internal(format!("failed to read entry in {}: {e}", path.display()))
        })?;
        let meta = entry.metadata().map_err(|e| {
            CorelamoError::Internal(format!("failed to stat {}: {e}", entry.path().display()))
        })?;
        if meta.is_dir() {
            total += dir_total_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

fn dir_total_size_opt(path: &std::path::Path) -> Result<Option<u64>, CorelamoError> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(dir_total_size(path)?))
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.2} KB", b / KB)
    } else if b < GB {
        format!("{:.2} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

fn format_size_opt(bytes: Option<u64>) -> serde_json::Value {
    match bytes {
        Some(b) => json!(format_size(b)),
        None => serde_json::Value::Null,
    }
}

pub fn compute_disk_usage(
    db_root: &std::path::Path,
    db_name: &str,
) -> Result<serde_json::Value, CorelamoError> {
    if !db_root.exists() {
        return Err(CorelamoError::NotFound(format!(
            "database '{db_name}' not found on disk"
        )));
    }

    // --- root-level config/toml files ---
    let config_files = [
        ("config.toml", "config_toml"),
        ("policy.toml", "policy_toml"),
        ("xpath_registry.toml", "xpath_registry_toml"),
        ("all_fields.toml", "all_fields_toml"),
    ];
    let mut config_total: u64 = 0;
    let mut config_json = serde_json::Map::new();
    for (filename, key) in config_files {
        let size = file_size_opt(&db_root.join(filename))?;
        if let Some(s) = size {
            config_total += s;
        }
        config_json.insert(key.to_string(), format_size_opt(size));
    }
    config_json.insert("total".to_string(), json!(format_size(config_total)));

    // --- backups: single total, not split by shard ---
    let backups_bytes = dir_total_size_opt(&db_root.join("backups"))?.unwrap_or(0);

    // --- discover shards on disk ---
    let shards_dir = db_root.join("shards");
    let mut shard_names: Vec<String> = Vec::new();
    if shards_dir.exists() {
        for entry in std::fs::read_dir(&shards_dir)
            .map_err(|e| CorelamoError::Internal(format!("failed to read shards dir: {e}")))?
        {
            let entry = entry
                .map_err(|e| CorelamoError::Internal(format!("failed to read shard entry: {e}")))?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    shard_names.push(name.to_string());
                }
            }
        }
    }
    shard_names.sort_by_key(|name| {
        name.strip_prefix("shard-")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });

    let mut documents_total: u64 = 0;
    let mut wal_total: u64 = 0;
    let mut logs_total: u64 = 0;
    let mut index_total: u64 = 0;

    let mut documents_per_shard = serde_json::Map::new();
    let mut wal_per_shard = serde_json::Map::new();
    let mut logs_per_shard = serde_json::Map::new();
    let mut index_per_shard = serde_json::Map::new();

    for shard_name in &shard_names {
        let shard_root = shards_dir.join(shard_name);

        let doc_size = file_size_opt(&shard_root.join("documents.bin"))?.unwrap_or(0);
        documents_total += doc_size;
        documents_per_shard.insert(shard_name.clone(), json!(format_size(doc_size)));

        let wal_log = file_size_opt(&shard_root.join("wal.log"))?.unwrap_or(0);
        let wal_checkpoint = file_size_opt(&shard_root.join("wal.checkpoint"))?.unwrap_or(0);
        let wal_shard_total = wal_log + wal_checkpoint;
        wal_total += wal_shard_total;
        wal_per_shard.insert(shard_name.clone(), json!(format_size(wal_shard_total)));

        let logs_size = dir_total_size_opt(&shard_root.join("logs"))?.unwrap_or(0);
        logs_total += logs_size;
        logs_per_shard.insert(shard_name.clone(), json!(format_size(logs_size)));

        let index_size = dir_total_size_opt(&shard_root.join("index"))?;
        let index_new_size = dir_total_size_opt(&shard_root.join("index.new"))?;
        let index_old_size = dir_total_size_opt(&shard_root.join("index.old"))?;
        let shard_index_bytes =
            index_size.unwrap_or(0) + index_new_size.unwrap_or(0) + index_old_size.unwrap_or(0);
        index_total += shard_index_bytes;

        index_per_shard.insert(
            shard_name.clone(),
            json!({
                "index": format_size_opt(index_size),
                "index_new": format_size_opt(index_new_size),
                "index_old": format_size_opt(index_old_size),
                "total": format_size(shard_index_bytes),
            }),
        );
    }

    documents_per_shard.insert("total".to_string(), json!(format_size(documents_total)));
    wal_per_shard.insert("total".to_string(), json!(format_size(wal_total)));
    logs_per_shard.insert("total".to_string(), json!(format_size(logs_total)));
    index_per_shard.insert("total".to_string(), json!(format_size(index_total)));

    let grand_total_bytes =
        config_total + backups_bytes + documents_total + wal_total + logs_total + index_total;

    Ok(json!({
        "database": db_name,
        "config": config_json,
        "backups": format_size(backups_bytes),
        "documents": documents_per_shard,
        "wal": wal_per_shard,
        "logs": logs_per_shard,
        "index": index_per_shard,
        "total": format_size(grand_total_bytes),
    }))
}
