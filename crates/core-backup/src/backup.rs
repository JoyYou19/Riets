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
            BackupType::Incremental => { //kkada huina
                
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