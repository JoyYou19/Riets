use core_storage::wal::Wal;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::fs::{self, File};
use serde::{Serialize, Deserialize};
use brotlic::{BrotliEncoderOptions, CompressorWriter, DecompressorReader, Quality};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub wal_offset: u64,
    pub document_count: u64,
    pub parent_backup_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Incremental,
}

pub struct BackupManager {
    backup_dir: PathBuf,
    last_backup_offset: u64,
    last_backup_id: Option<String>,
}

#[derive(Debug)]
pub enum BackupError {
    IoError(std::io::Error),
    SerdeError(serde_json::Error),
    BincodeError(String),
    WalError(String),
    CorruptRecord(String),
    NoBackupChain,
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
            BackupError::CorruptRecord(e) => write!(f, "Corrupt WAL record in backup: {}",e),
            BackupError::NoBackupChain => write!(f,"Could not resolve a full backup at the root of this chain"),
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
fn decompress_file(src: &Path, dst:&Path) -> io::Result<()>{
    let src_file=File::open(dst)?;
    let mut decompressor =DecompressorReader::new(BufReader::new(src_file));
    let dst_file=File::create(dst)?;
    let mut writer = BufWriter::new(dst_file);
    io::copy(&mut decompressor, &mut writer)?;
    writer.flush()?;
    Ok(())
}
 //Parses the (offset: u64 LE, len: u32 LE, payload) framing that
// create_incremental_backup writes. Kept separate from restore() so it can
// be unit-tested against a hand-built byte buffer without touching disk.
fn parse_wal_records(bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>, BackupError> {
    let mut records = Vec::new();
    let mut cursor = 0usize;
 
    while cursor < bytes.len() {
        if cursor + 12 > bytes.len() {
            return Err(BackupError::CorruptRecord(
                "truncated record header".to_string(),
            ));
        }
 
        let offset = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;
        let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
 
        if cursor + len > bytes.len() {
            return Err(BackupError::CorruptRecord(format!(
                "record at offset {} claims {} bytes but only {} remain",
                offset,
                len,
                bytes.len() - cursor
            )));
        }
 
        let payload = bytes[cursor..cursor + len].to_vec();
        cursor += len;
        records.push((offset, payload));
    }
 
    Ok(records)
}
impl BackupManager {
    pub fn new(backup_dir: PathBuf) -> Self {
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
            last_backup_id,
            last_backup_offset,
        }
    }
     fn save_state(&self) -> Result<(), BackupError> {
        let state_path = self.backup_dir.join("backup_state.json");
        let state = serde_json::json!({
            "last_offset": self.last_backup_offset,
            "last_backup_id": self.last_backup_id,
        });
        fs::write(&state_path, serde_json::to_string(&state)?)?;
        Ok(())
    }
    pub fn create_full_backup(&self, db_root: &Path,wal:&Wal) -> Result<BackupManifest, BackupError> {
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
        let wal_offset = wal.durable_offset();
        let manifest = BackupManifest {
            backup_id:backup_id.clone(),
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Full,
            wal_offset,
            document_count: 0,//nepareizi
            parent_backup_id: None,
        };

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        Ok(manifest)
    }

    pub fn create_incremental_backup(&mut self,wal:&Wal) -> Result<BackupManifest, BackupError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().timestamp());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_path)?;

        let records = wal.replay_from(self.last_backup_offset)
            .map_err(|e| BackupError::WalError(e.to_string()))?;

        let wal_path = backup_path.join("wal_records.bin");
        let mut file = File::create(&wal_path)?;

        for (offset, payload) in records {
            file.write_all(&offset.to_le_bytes())?;
            file.write_all(&(payload.len() as u32).to_le_bytes())?;
            file.write_all(&payload)?;
        }

        let manifest = BackupManifest {
            backup_id:backup_id.clone(),
            created_at: chrono::Utc::now().timestamp() as u64,
            backup_type: BackupType::Incremental,
            wal_offset: wal.durable_offset(),
            document_count: 0,
            parent_backup_id:self.last_backup_id.clone(),
        };

        let manifest_path = backup_path.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string(&manifest)?)?;

        self.last_backup_offset = manifest.wal_offset;

        Ok(manifest)
    }
    fn load_manifest(&self, backup_id: &str) -> Result<BackupManifest, BackupError> {
        let path = self.backup_dir.join(backup_id).join("manifest.json");
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
    // Restores a single backup in isolation. For a Full backup this is a
    // complete restore on its own. For an Incremental backup this only
    // applies that increment's records - it assumes target_dir already
    // holds the state from its parent chain. Use restore_chain() unless
    // you specifically need this lower-level behavior (e.g. re-applying
    // one increment during testing).
    pub fn restore(&self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        let backup_path = self.backup_dir.join(backup_id);
        let manifest = self.load_manifest(backup_id)?;
 
        match manifest.backup_type {
            BackupType::Full => {
                fs::create_dir_all(target_dir)?;
                decompress_file(
                    &backup_path.join("documents.bin.br"),
                    &target_dir.join("documents.bin"),
                )?;
                decompress_file(
                    &backup_path.join("index.br"),
                    &target_dir.join("index"),
                )?;
            }
            BackupType::Incremental => {
                let raw = fs::read(backup_path.join("wal_records.bin"))?;
                let records = parse_wal_records(&raw)?;
 // NOTE: applying a record here means re-inserting it into
                // the live WAL at its original offset so the normal replay
                // path picks it up, not appending it as a new write. That
                // needs a method on Wal along the lines of:
                //
                //     fn write_raw_record(&mut self, offset: u64, payload: &[u8]) -> io::Result<()>
                //
                // which doesn't exist yet - this is the integration point
                // for hooking restore up to db_actor.rs (item 2 on the
                // priority list). Until that lands, records are parsed and
                // validated here but not yet applied.
                for (offset, payload) in &records {
                    let _ = (offset, payload); // silence unused warnings until write_raw_record exists
                }
            }
        }
 
        Ok(())
    }
    pub fn restore_chain(&self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        let mut chain = vec![self.load_manifest(backup_id)?];
 
        while chain.last().unwrap().backup_type == BackupType::Incremental {
            let parent_id = chain
                .last()
                .unwrap()
                .parent_backup_id
                .clone()
                .ok_or(BackupError::NoBackupChain)?;
            chain.push(self.load_manifest(&parent_id)?);
        }
 
        // chain was built newest-to-oldest; apply oldest (the Full) first.
        for manifest in chain.iter().rev() {
            self.restore(&manifest.backup_id, target_dir)?;
        }
 
        Ok(())
    }
    pub fn latest_backup_id(&self) -> Option<&str> {
    self.last_backup_id.as_deref()
    }
}