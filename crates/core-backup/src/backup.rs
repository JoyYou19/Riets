use core_storage::wal::{Wal, WalRecord};
use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::process::Output;
use serde::{Serialize, Deserialize};
use bincode::{Encode, Decode};
use brotli::enc::BrotliEncoderOptions;

use crate::backup;




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
    last_backup_id: Option<String>,
}
//viss ar errors
#[derive(Debug)]
pub enum BackupError {
    IoError(std::io::Error),
    SerdeError(std::io::Error),
    BincodeError(String),
    WalError(String),
}
impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        BackupError::IoError(error)
    }
}
impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::IoError(e) => write!(f, "IO Error: {}", e),
            BackupError::SerdeError(e) => write!(f, "Serde Error: {}", e),
            BackupError::BincodeError(e) => write!(f, "Bincode Error: {}", e),
            BackupError::WalError(e) => write!(f, "WAL Error: {}", e),
        }
    }
}

//helper functions
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

fn compression_specs() -> BrotliEncoderOptions {
    let mut options = BrotliEncoderOptions::new();
    options.quality = 11; // Maximum quality (0-11)
    options.lgwin = 22; // Window size (10-24)
    options
}


impl BackupManager {
    pub fn new(backup_dir: PathBuf, wal: Wal) -> Self {
        let mut encoder = BrotliEncoderOptions::new(Vector::new(), compression_specs());
        let path = backup_dir.join("backup_state.json");
        let (last_backup_offset, last_bakcup_id) = if let Ok 
        (state) = fs::read_to_string(&state_path){
            (state.last_offsetmstate.last_backup_id)
        }else{
            (0,None)
        };
        Self {
            backup_dir,
            wal,
            last_backup_id,
            last_backup_offset,
        }
    }

    /// Create a full backup (copy documents.bin + index + WAL)
    pub fn create_full_backup(&self, db_root: &Path) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("full_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        // Copy document store

        let existing_documents = File::open(db_root.join("documents.bin"))?;
        let existing_index= File::open(db_root.join("index"))?;
        let mut doc_reader= BufReader::new(backup_documents);
        let mut index_reader= BufReader::new(existing_index);
        let mut buffer =vec![0; 8192]; //random lielums
        let backup_documents = File::create(backup_path.join("documents.bin"))?;
        let backup_index = File::create(backup_path.join("index"))?;
        loop{
            let bytes_read = doc_reader.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            encoder.copy(&buffer[..bytes_read], &mut backup_documents)?;
            let mut output = encoder.take_output()?;
            backup_index.write_all(&output)?;
        }
        
        // fs::copy(
        //     db_root.join("documents.bin"),
        //     backup_path.join("documents.bin")
        // )?;
        // // Copy index directory
        // copy_dir_recursive(
        //     &db_root.join("index"),
        //     &backup_path.join("index")
        // )?;

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
                fs::copy(// partaisit lai zipped nokope tad atver 
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




