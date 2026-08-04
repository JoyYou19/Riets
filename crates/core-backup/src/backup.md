
Looking at your codebase, I can see you have a WAL (Write-Ahead Log) system in place and a `core-backup` crate that's currently empty. Let me help you understand how to implement incremental backups using the WAL.

## Current Architecture Overview

Your WAL system (`crates/core-storage/src/wal.rs`) provides:
- **Append-only log** with records for Create, Upsert, Modify, Delete, Clear operations
- **Checkpoint system** to track the last durable offset
- **Replay capability** to recover from a specific offset
- **Sync modes** (SyncEach for durability, Manual for batching)

## Key Components for Backup

### 1. WAL Records (already defined)
```rust
pub enum WalRecord {
    Create(Vec<DocumentInput>),
    Upsert(DocumentInput),
    Modify { external_id: String, payload: Vec<DocumentInput> },
    Delete { external_id: String },
    Clear,
}
```

### 2. Backup Strategy

For incremental backups, you'll want to:

1. **Full backup**: Copy the document store (`documents.bin`) and index files
2. **Incremental backup**: Copy only the WAL records since the last backup

### 3. Implementation Plan

Here's how you could structure the backup system in `core-backup`:

```rust
// crates/core-backup/src/backup.rs

use core_storage::wal::{Wal, WalRecord};
use std::path::{Path, PathBuf};
use std::fs;
use serde::{Serialize, Deserialize};
use bincode::{Encode, Decode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub wal_offset: u64,  // WAL offset at backup time
    pub document_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Incremental,
}

pub struct BackupManager {
    backup_dir: PathBuf,
    wal: Wal,
    last_backup_offset: u64,
}

impl BackupManager {
    pub fn new(backup_dir: PathBuf, wal: Wal) -> Self {
        Self {
            backup_dir,
            wal,
            last_backup_offset: 0,
        }
    }

    /// Create a full backup (copy documents.bin + index + WAL)
    pub fn create_full_backup(&self, db_root: &Path) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("full_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        // Copy document store
        fs::copy(
            db_root.join("documents.bin"),
            backup_path.join("documents.bin")
        )?;

        // Copy index directory
        copy_dir_recursive(
            &db_root.join("index"),
            &backup_path.join("index")
        )?;

        // Record WAL offset
        let wal_offset = self.wal.durable_offset();

        let manifest = BackupManifest {
            backup_id,
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Full,
            wal_offset,
            document_count: 0, // You'd get this from the database
        };

        // Save manifest
        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        Ok(manifest)
    }

    /// Create incremental backup (only WAL records since last backup)
    pub fn create_incremental_backup(&self) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        // Get WAL records since last backup
        let records = self.wal.replay_from(self.last_backup_offset)?;

        // Save WAL records
        let wal_path = backup_path.join("wal_records.bin");
        let mut file = fs::File::create(&wal_path)?;

        for (offset, payload) in records {
            // Write offset and payload
            file.write_all(&offset.to_le_bytes())?;
            file.write_all(&(payload.len() as u32).to_le_bytes())?;
            file.write_all(&payload)?;
        }

        let manifest = BackupManifest {
            backup_id,
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Incremental,
            wal_offset: self.wal.durable_offset(),
            document_count: 0,
        };

        // Save manifest
        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        // Update last backup offset
        self.last_backup_offset = manifest.wal_offset;

        Ok(manifest)
    }

    /// Restore from backups
    pub fn restore(&self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        let backup_path = self.backup_dir.join(backup_id);

        // Read manifest
        let manifest: BackupManifest = serde_json::from_str(
            &fs::read_to_string(backup_path.join("manifest.json"))?
        )?;

        match manifest.backup_type {
            BackupType::Full => {
                // Restore full backup
                fs::copy(
                    backup_path.join("documents.bin"),
                    target_dir.join("documents.bin")
                )?;
                copy_dir_recursive(
                    &backup_path.join("index"),
                    &target_dir.join("index")
                )?;
            }
            BackupType::Incremental => {
                // Apply WAL records to existing database
                // This would need to be integrated with your database restore logic
                let wal_records = fs::read(backup_path.join("wal_records.bin"))?;
                // Parse and apply records...
            }
        }

        Ok(())
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
```

### 4. Backup Thread Integration

In your `db_actor.rs`, you could add a backup thread that periodically creates incremental backups:

```rust
// In db_actor.rs or a new backup_actor.rs

pub struct BackupHandle {
    tx: mpsc::Sender<BackupCommand>,
}

enum BackupCommand {
    CreateFullBackup,
    CreateIncrementalBackup,
    Restore { backup_id: String },
    Shutdown,
}

pub fn spawn_backup_actor(
    backup_dir: PathBuf,
    wal: Wal,
    db_root: PathBuf,
) -> (BackupHandle, thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<BackupCommand>(64);

    let join = thread::Builder::new()
        .name("backup-actor".to_string())
        .spawn(move || {
            let manager = BackupManager::new(backup_dir, wal);

            while let Some(cmd) = rx.blocking_recv() {
                match cmd {
                    BackupCommand::CreateFullBackup => {
                        if let Err(e) = manager.create_full_backup(&db_root) {
                            error!("Full backup failed: {}", e);
                        }
                    }
                    BackupCommand::CreateIncrementalBackup => {
                        if let Err(e) = manager.create_incremental_backup() {
                            error!("Incremental backup failed: {}", e);
                        }
                    }
                    BackupCommand::Restore { backup_id } => {
                        if let Err(e) = manager.restore(&backup_id, &db_root) {
                            error!("Restore failed: {}", e);
                        }
                    }
                    BackupCommand::Shutdown => break,
                }
            }
        })
        .expect("failed to spawn backup thread");

    (BackupHandle { tx }, join)
}
```

### 5. Key Considerations

1. **WAL Checkpoint**: After a successful full backup, you should write a checkpoint so the WAL can be truncated
2. **Atomicity**: Ensure backups are atomic - write to temp directory then rename
3. **Compression**: Consider compressing backup files to save space
4. **Retention**: Implement a policy to keep only N recent backups
5. **Consistency**: The backup thread should coordinate with the database actor to ensure consistent snapshots

Would you like me to elaborate on any specific part of this implementation?

I'll help you implement the full backup system integrated into your program. Let me create a complete implementation that ties everything together.











### 1. First, update `crates/core-backup/src/backup.rs` to be complete:

```rust
use core_storage::wal::{Wal, WalRecord};
use std::path::{Path, PathBuf};
use std::fs;
use std::io::{self, Write};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub wal_offset: u64,
    pub document_count: u64,
    pub parent_backup: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Incremental,
}

#[derive(Debug)]
pub enum BackupError {
    Io(io::Error),
    Serialization(String),
    WalError(String),
    NotFound(String),
}

impl From<io::Error> for BackupError {
    fn from(e: io::Error) -> Self {
        BackupError::Io(e)
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(e: serde_json::Error) -> Self {
        BackupError::Serialization(e.to_string())
    }
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::Io(e) => write!(f, "IO error: {}", e),
            BackupError::Serialization(e) => write!(f, "Serialization error: {}", e),
            BackupError::WalError(e) => write!(f, "WAL error: {}", e),
            BackupError::NotFound(e) => write!(f, "Not found: {}", e),
        }
    }
}

impl std::error::Error for BackupError {}

#[derive(Serialize, Deserialize)]
struct BackupState {
    last_offset: u64,
    last_backup_id: Option<String>,
}

pub struct BackupManager {
    backup_dir: PathBuf,
    wal: Wal,
    last_backup_offset: u64,
    last_backup_id: Option<String>,
}

impl BackupManager {
    pub fn new(backup_dir: PathBuf, wal: Wal) -> Self {
        let state_path = backup_dir.join("backup_state.json");
        let (last_backup_offset, last_backup_id) = if let Ok(state) = fs::read_to_string(&state_path) {
            if let Ok(state) = serde_json::from_str::<BackupState>(&state) {
                (state.last_offset, state.last_backup_id)
            } else {
                (0, None)
            }
        } else {
            (0, None)
        };

        Self {
            backup_dir,
            wal,
            last_backup_offset,
            last_backup_id,
        }
    }

    pub fn create_full_backup(&self, db_root: &Path) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("full_{}", chrono::Utc::now().timestamp_millis());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        // Copy document store
        let docs_source = db_root.join("documents.bin");
        if docs_source.exists() {
            fs::copy(&docs_source, backup_path.join("documents.bin"))?;
        }

        // Copy index directory if it exists
        let index_source = db_root.join("index");
        if index_source.exists() {
            copy_dir_recursive(&index_source, &backup_path.join("index"))?;
        }

        // Copy WAL file
        let wal_source = db_root.join("wal.log");
        if wal_source.exists() {
            fs::copy(&wal_source, backup_path.join("wal.log"))?;
        }

        let wal_offset = self.wal.durable_offset();

        let manifest = BackupManifest {
            backup_id: backup_id.clone(),
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Full,
            wal_offset,
            document_count: 0,
            parent_backup: None,
        };

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

        self.save_state(wal_offset, Some(backup_id))?;

        Ok(manifest)
    }

    pub fn create_incremental_backup(&self) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().timestamp_millis());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        let records = self.wal.replay_from(self.last_backup_offset)
            .map_err(|e| BackupError::WalError(e.to_string()))?;

        let wal_path = backup_path.join("wal_records.bin");
        let mut file = fs::File::create(&wal_path)?;

        for (offset, payload) in records {
            file.write_all(&offset.to_le_bytes())?;
            file.write_all(&(payload.len() as u32).to_le_bytes())?;
            file.write_all(&payload)?;
        }

        let manifest = BackupManifest {
            backup_id: backup_id.clone(),
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Incremental,
            wal_offset: self.wal.durable_offset(),
            document_count: 0,
            parent_backup: self.last_backup_id.clone(),
        };

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

        self.save_state(manifest.wal_offset, Some(backup_id))?;

        Ok(manifest)
    }

    fn save_state(&self, offset: u64, backup_id: Option<String>) -> Result<(), BackupError> {
        let state = BackupState {
            last_offset: offset,
            last_backup_id: backup_id,
        };
        let state_path = self.backup_dir.join("backup_state.json");
        fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
        Ok(())
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
```

### 2. Create a backup actor in `crates/core-runtime/src/backup_actor.rs`:

```rust
use core_backup::backup::BackupManager;
use core_storage::wal::Wal;
use slog::{error, info};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;

pub enum BackupCommand {
    CreateFullBackup,
    CreateIncrementalBackup,
    Shutdown,
}

#[derive(Clone)]
pub struct BackupHandle {
    tx: mpsc::Sender<BackupCommand>,
}

impl BackupHandle {
    pub async fn create_full_backup(&self) -> Result<(), String> {
        self.tx.send(BackupCommand::CreateFullBackup).await
            .map_err(|e| format!("failed to send backup command: {}", e))
    }

    pub async fn create_incremental_backup(&self) -> Result<(), String> {
        self.tx.send(BackupCommand::CreateIncrementalBackup).await
            .map_err(|e| format!("failed to send backup command: {}", e))
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.tx.send(BackupCommand::Shutdown).await
            .map_err(|e| format!("failed to send shutdown command: {}", e))
    }
}

pub fn spawn_backup_actor(
    backup_dir: PathBuf,
    db_root: PathBuf,
    wal: Wal,
) -> (BackupHandle, thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<BackupCommand>(64);
    let log = slog_scope::logger();

    let join = thread::Builder::new()
        .name("backup-actor".to_string())
        .spawn(move || {
            // Create backup directory if it doesn't exist
            std::fs::create_dir_all(&backup_dir).expect("failed to create backup dir");

            let manager = BackupManager::new(backup_dir.clone(), wal);

            info!(log, "Backup actor started"; "backup_dir" => %back

