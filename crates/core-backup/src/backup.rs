use core_storage::wal::Wal;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::fs::{self, File};
use serde::{Serialize, Deserialize};
use brotlic::{BrotliEncoderOptions, CompressorWriter, Quality};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub wal_offset: u64,
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

#[derive(Debug)]
pub enum BackupError {
    IoError(std::io::Error),
    SerdeError(serde_json::Error),
    BincodeError(String),
    WalError(String),
}

impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        BackupError::IoError(error)
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(error: serde_json::Error) -> Self {
        BackupError::SerdeError(error)
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

fn compress_file(src: &Path, dst: &Path) -> io::Result<()> {
    let src_file = File::open(src)?;
    let mut reader = BufReader::new(src_file);

    let dst_file = File::create(dst)?;
    let mut compressor = CompressorWriter::with_encoder(
        BrotliEncoderOptions::new()
            .quality(Quality::new(11).unwrap())
            .build()
            .unwrap(),
        BufWriter::new(dst_file),
    );

    let mut buffer = vec![0u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        compressor.write_all(&buffer[..bytes_read])?;
    }
    compressor.flush()?;
    Ok(())
}

impl BackupManager {
    pub fn new(backup_dir: PathBuf, wal: Wal) -> Self {
        let state_path = backup_dir.join("backup_state.json");
        let (last_backup_offset, last_backup_id) = if let Ok(state) = fs::read_to_string(&state_path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&state) {
                let offset = parsed["last_offset"].as_u64().unwrap_or(0);
                let id = parsed["last_backup_id"].as_str().map(|s| s.to_string());
                (offset, id)
            } else {
                (0, None)
            }
        } else {
            (0, None)
        };

        Self {
            backup_dir,
            wal,
            last_backup_id,
            last_backup_offset,
        }
    }

    pub fn create_full_backup(&self, db_root: &Path) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("full_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        compress_file(
            &db_root.join("documents.bin"),
            &backup_path.join("documents.bin.br"),
        )?;

        compress_file(
            &db_root.join("index"),
            &backup_path.join("index.br"),
        )?;

        let wal_offset = self.wal.durable_offset();

        let manifest = BackupManifest {
            backup_id,
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Full,
            wal_offset,
            document_count: 0,
        };

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        Ok(manifest)
    }

    pub fn create_incremental_backup(&mut self) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        let records = self.wal.replay_from(self.last_backup_offset)
            .map_err(|e| BackupError::WalError(e.to_string()))?;

        let wal_path = backup_path.join("wal_records.bin");
        let mut file = File::create(&wal_path)?;

        for (offset, payload) in records {
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

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        self.last_backup_offset = manifest.wal_offset;

        Ok(manifest)
    }

    pub fn restore(&self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        let backup_path = self.backup_dir.join(backup_id);

        let manifest: BackupManifest = serde_json::from_str(
            &fs::read_to_string(backup_path.join("manifest.json"))?
        )?;

        match manifest.backup_type {
            BackupType::Full => {
                // decompress .br files back to target
                fs::copy(
                    backup_path.join("documents.bin.br"),
                    target_dir.join("documents.bin.br"),
                )?;
                // TODO: decompress .br files at target
            }
            BackupType::Incremental => {
                let wal_records = fs::read(backup_path.join("wal_records.bin"))?;
                // TODO: parse and apply records
            }
        }

        Ok(())
    }
}