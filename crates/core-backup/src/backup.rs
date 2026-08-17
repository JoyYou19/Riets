use core_storage::wal::Wal;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub start_offset: u64,
    pub wal_offset: u64,
    pub document_count: u64,
    pub record_count: u64,
    pub parent_backup_id: Option<String>,
}

use crate::progress::BackupProgress;
const COPY_BUF_SIZE: usize = 256 * 1024;

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
    NoBaseBackup,
    ChainGap {
        parent_id: String,
        parent_end: u64,
        child_id: String,
        child_start: u64,
    },
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
            BackupError::IoError(e) => write!(f, "IO error: {}", e),
            BackupError::SerdeError(e) => write!(f, "Serde error: {}", e),
            BackupError::BincodeError(e) => write!(f, "Bincode error: {}", e),
            BackupError::WalError(e) => write!(f, "WAL error: {}", e),
            BackupError::CorruptRecord(e) => write!(f, "Corrupt WAL record: {}", e),
            BackupError::NoBackupChain => {
                write!(f, "Could not resolve a full backup at chain root")
            }
            BackupError::NoBaseBackup => write!(f, "No full backup exists to base an increment on"),
            BackupError::ChainGap {
                parent_id,
                parent_end,
                child_id,
                child_start,
            } => write!(
                f,
                "Gap in chain: {} ends at {} but {} starts at {}",
                parent_id, parent_end, child_id, child_start
            ),
        }
    }
}

fn copy_with_progress<R: io::Read, W: io::Write>(
    mut reader: R,
    mut writer: W,
    progress: &BackupProgress,
) -> io::Result<()> {
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        progress.add(n as u64);
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path, progress: Option<&BackupProgress>) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path, progress)?;
        } else {
            let bytes = fs::copy(&src_path, &dst_path)?;
            if let Some(p) = progress {
                p.add(bytes);
            }
        }
    }
    Ok(())
}

fn compress_file(src: &Path, dst: &Path, progress: &BackupProgress) -> io::Result<()> {
    let reader = BufReader::new(File::open(src)?);
    let mut encoder = GzEncoder::new(File::create(dst)?, Compression::default());
    copy_with_progress(reader, &mut encoder, progress)?;
    encoder.finish()?;
    Ok(())
}

fn decompress_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(GzDecoder::new(File::open(src)?));
    let mut writer = BufWriter::new(File::create(dst)?);
    io::copy(&mut reader, &mut writer)?;
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

fn write_manifest_atomic(backup_path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let tmp = backup_path.join("manifest.json.tmp");
    let dst = backup_path.join("manifest.json");
    fs::write(&tmp, serde_json::to_string(manifest)?)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &dst)?;
    Ok(())
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

impl BackupManager {
    pub fn new(backup_dir: PathBuf) -> Self {
        let state_path = backup_dir.join("backup_state.json");

        let (last_backup_offset, last_backup_id) =
            if let Ok(state) = fs::read_to_string(&state_path) {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&state) {
                    let offset = parsed["last_offset"].as_u64().unwrap_or(0);
                    let id = parsed["last_backup_id"].as_str().map(|s| s.to_string());
                    (offset, id)
                } else {
                    (0, None)
                }
            } else {
                // State file doesn't exist; scan disk for latest backup
                let best = fs::read_dir(&backup_dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let manifest_path = e.path().join("manifest.json");
                        let text = fs::read_to_string(&manifest_path).ok()?;
                        let m: BackupManifest = serde_json::from_str(&text).ok()?;
                        Some(m)
                    })
                    .max_by_key(|m| m.created_at);

                match best {
                    Some(m) => (m.wal_offset, Some(m.backup_id)),
                    None => (0, None),
                }
            };

        Self {
            backup_dir,
            last_backup_id,
            last_backup_offset,
        }
    }

    fn save_state(&self) -> Result<(), BackupError> {
        let tmp = self.backup_dir.join("backup_state.json.tmp");
        let dst = self.backup_dir.join("backup_state.json");
        let state = serde_json::json!({
            "last_offset": self.last_backup_offset,
            "last_backup_id": self.last_backup_id,
        });
        fs::write(&tmp, serde_json::to_string(&state)?)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }

    pub fn create_full_backup(
        &mut self,
        db_root: &Path,
        wal: &Wal,
        progress: &BackupProgress,
    ) -> Result<BackupManifest, BackupError> {
        //read wal offset before touching files so new things dont catch up with backup
        let wal_offset = wal.durable_offset();
        let backup_id = format!("full_{}", chrono::Utc::now().timestamp_millis());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir(&backup_path)?;

        let total =
            db_root.join("documents.bin").metadata()?.len() + dir_size(&db_root.join("index"))?;
        progress.grow_total(total);

        let result = (|| -> Result<BackupManifest, BackupError> {
            compress_file(
                &db_root.join("documents.bin"),
                &backup_path.join("documents.bin.br"),
                progress,
            )?;
            copy_dir_recursive(
                &db_root.join("index"),
                &backup_path.join("index"),
                Some(progress),
            )?;

            let manifest = BackupManifest {
                backup_id: backup_id.clone(),
                created_at: chrono::Utc::now().timestamp() as u64,
                backup_type: BackupType::Full,
                start_offset: 0,
                wal_offset,        // the value snapshotted above
                document_count: 0, // populate once ShardCut is wired up
                record_count: 0,
                parent_backup_id: None,
            };

            write_manifest_atomic(&backup_path, &manifest)?;
            Ok(manifest)
        })();

        match result {
            Ok(manifest) => {
                self.last_backup_id = Some(backup_id);
                self.last_backup_offset = wal_offset;
                self.save_state()?;
                Ok(manifest)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&backup_path);
                Err(e)
            }
        }
    }

    pub fn create_incremental_backup(
        &mut self,
        wal: &Wal,
    ) -> Result<Option<BackupManifest>, BackupError> {
        let parent_id = self
            .last_backup_id
            .clone()
            .ok_or(BackupError::NoBackupChain)?;

        let start_offset = self.last_backup_offset;
        let end_offset = wal.durable_offset();
        if end_offset <= start_offset {
            return Ok(None);
        }
        let backup_id = format!("incr_{}", chrono::Utc::now().timestamp_millis());
        let backup_path = self.backup_dir.join(&backup_id);
        fs::create_dir(&backup_path)?;

        // Anything that returns Err after this point hits the cleanup at the bottom.
        let inner = || -> Result<BackupManifest, BackupError> {
            let records = wal
                .replay_from(start_offset)
                .map_err(|e| BackupError::WalError(e.to_string()))?;

            let file = File::create(backup_path.join("wal_records.bin"))?;
            let mut w = BufWriter::new(file);
            let mut record_count: u64 = 0;

            for (offset, payload) in records {
                if offset < start_offset || offset >= end_offset {
                    continue;
                }
                w.write_all(&offset.to_le_bytes())?;
                w.write_all(&(payload.len() as u32).to_le_bytes())?;
                w.write_all(&payload)?;
                record_count += 1;
            }

            w.flush()?;
            w.into_inner()
                .map_err(|e| BackupError::IoError(e.into_error()))?
                .sync_all()?;

            let manifest = BackupManifest {
                backup_id: backup_id.clone(),
                created_at: chrono::Utc::now().timestamp() as u64,
                backup_type: BackupType::Incremental,
                start_offset,
                wal_offset: end_offset,
                document_count: 0,
                record_count,
                parent_backup_id: Some(parent_id),
            };

            write_manifest_atomic(&backup_path, &manifest)?;
            Ok(manifest)
        };
        match inner() {
            Ok(manifest) => {
                self.last_backup_offset = end_offset;
                self.last_backup_id = Some(backup_id);
                self.save_state()?;
                Ok(Some(manifest))
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&backup_path);
                Err(e)
            }
        }
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
                copy_dir_recursive(&backup_path.join("index"), &target_dir.join("index"), None)?;
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
        chain.reverse();
        // Verify contiguity before touching target_dir. A gap found mid-apply
        // would leave the shard in an undefined state.
        for window in chain.windows(2) {
            let parent = &window[0];
            let child = &window[1];
            if child.start_offset != parent.wal_offset {
                return Err(BackupError::ChainGap {
                    parent_id: parent.backup_id.clone(),
                    parent_end: parent.wal_offset,
                    child_id: child.backup_id.clone(),
                    child_start: child.start_offset,
                });
            }
        }

        for manifest in &chain {
            self.restore(&manifest.backup_id, target_dir)?;
        }
        Ok(())
    }

    pub fn latest_backup_id(&self) -> Option<&str> {
        self.last_backup_id.as_deref()
    }
}
