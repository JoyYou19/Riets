use crate::progress::BackupProgress;
use core_logs::logger;
use core_storage::wal::Wal;
use core_timing::timed;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{ Deserialize, Serialize };
use slog::{ Logger, error };
use std::fs::{ self, File };
use std::io::{ self, BufReader, BufWriter, Write };
use std::path::{ Path, PathBuf };
use std::time::SystemTime;
const COPY_BUF_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub start_offset: u64,
    pub wal_offset: u64,
    pub document_count: usize,
    pub record_count: usize,
    pub parent_backup_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Incremental,
}

pub struct BackupManager {
    backup_dir: PathBuf,
    shard_name: String,
    last_backup_offset: u64,
    last_backup_id: Option<String>,
    log: Logger,
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
            BackupError::ChainGap { parent_id, parent_end, child_id, child_start } =>
                write!(
                    f,
                    "Gap in chain: {} ends at {} but {} starts at {}",
                    parent_id,
                    parent_end,
                    child_id,
                    child_start
                ),
        }
    }
}

#[timed(writing_files)]
fn copy_with_progress<R: io::Read, W: io::Write>(
    mut reader: R,
    mut writer: W,
    progress: &BackupProgress
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

#[timed(backup)]
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

//directory zipping
#[timed(writing_files)]
fn tar_dir(src: &Path, dst: &Path, progress: Option<&BackupProgress>) -> io::Result<()> {
    let enc = GzEncoder::new(File::create(dst)?, Compression::default());
    let mut builder = tar::Builder::new(enc);
    builder.append_dir_all("index", src)?;
    builder.into_inner()?.finish()?;
    if let Some(p) = progress {
        p.add(dir_size(src)?);
    }
    Ok(())
}

#[timed(writing_files)]
pub fn compress_file(src: &Path, dst: &Path, progress: &BackupProgress) -> io::Result<()> {
    let reader = BufReader::new(File::open(src)?);
    let mut encoder = GzEncoder::new(File::create(dst)?, Compression::default());
    copy_with_progress(reader, &mut encoder, progress)?;
    encoder.finish()?;
    Ok(())
}

#[timed(restore)]
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
#[timed(restore)]
fn parse_wal_records(bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>, BackupError> {
    let mut records = Vec::new();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if cursor + 12 > bytes.len() {
            return Err(BackupError::CorruptRecord("truncated record header".to_string()));
        }

        let offset = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;
        let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;

        if cursor + len > bytes.len() {
            return Err(
                BackupError::CorruptRecord(
                    format!(
                        "record at offset {} claims {} bytes but only {} remain",
                        offset,
                        len,
                        bytes.len() - cursor
                    )
                )
            );
        }

        let payload = bytes[cursor..cursor + len].to_vec();
        cursor += len;
        records.push((offset, payload));
    }

    Ok(records)
}
#[timed(writing_files)]
fn write_manifest_atomic(backup_path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    let tmp = backup_path.join("manifest.json.tmp");
    let dst = backup_path.join("manifest.json");
    fs::write(&tmp, serde_json::to_string(manifest)?)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &dst)?;
    Ok(())
}

impl BackupManager {
    fn shard_backup_path(&self, backup_id: &str) -> PathBuf {
        self.backup_dir.join(backup_id).join(&self.shard_name)
    }
    #[timed(database_lifecycle)]
    pub fn new(shard_root: &Path, backup_dir: PathBuf, shard_name: String) -> Self {
        let state_path = backup_dir.join(format!("backup_state_{shard_name}.json"));
        let name = shard_name.clone();
        let log = logger::shard_logger(shard_root, &name);
        let (last_backup_offset, last_backup_id) = if
            let Ok(state) = fs::read_to_string(&state_path)
        {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&state) {
                let offset = parsed["last_offset"].as_u64().unwrap_or(0);
                let id = parsed["last_backup_id"].as_str().map(|s| s.to_string());
                (offset, id)
            } else {
                (0, None)
            }
        } else {
            // State file doesn't exist; scan disk for latest backup
            let best = fs
                ::read_dir(&backup_dir)
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
            shard_name,
            last_backup_id,
            last_backup_offset,
            log,
        }
    }
    #[timed(writing_files)]
    fn save_state(&self) -> Result<(), BackupError> {
        let dst = self.backup_dir.join(format!("backup_state_{}.json", self.shard_name));
        let tmp = dst.with_extension("json.tmp");
        let state =
            serde_json::json!({
            "last_offset": self.last_backup_offset,
            "last_backup_id": self.last_backup_id,
        });
        fs::write(&tmp, serde_json::to_string(&state)?)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &dst)?;
        Ok(())
    }
    #[timed(backup)]
    pub fn create_full_backup(
        &mut self,
        shard_root: &Path,
        backup_path: &Path,
        backup_id: &str,
        wal: &Wal,
        progress: &BackupProgress,
        document_count: usize,
        record_count: usize
    ) -> Result<BackupManifest, BackupError> {
        //read wal offset before touching files so new things dont catch up with backup
        let wal_offset = wal.durable_offset();

        fs::create_dir_all(backup_path)?;

        let total =
            shard_root.join("documents.bin").metadata()?.len() +
            dir_size(&shard_root.join("index"))?;
        progress.grow_total(total);

        let result = (|| -> Result<BackupManifest, BackupError> {
            compress_file(
                &shard_root.join("documents.bin"),
                &backup_path.join("documents.bin.gz"),
                progress
            )?;
            tar_dir(&shard_root.join("index"), &backup_path.join("index.tar.gz"), Some(progress))?;

            let manifest = BackupManifest {
                backup_id: backup_id.to_string(),
                created_at: chrono::Utc::now().timestamp_millis() as u64,
                backup_type: BackupType::Full,
                start_offset: 0,
                wal_offset, // the value snapshotted above
                document_count,
                record_count,
                parent_backup_id: None,
            };

            write_manifest_atomic(&backup_path, &manifest)?;
            Ok(manifest)
        })();

        match result {
            Ok(manifest) => {
                let old_id = self.last_backup_id.clone();
                self.last_backup_id = Some(backup_id.to_string());
                // in create_full_backup, where last_backup_offset is set
                self.last_backup_offset = wal_offset;
                self.save_state()?;
                if let Some(old_id) = old_id {
                    self.delete_incremental_chain(&old_id);
                }

                Ok(manifest)
            }
            Err(e) => {
                let _ = fs::remove_dir_all(backup_path);
                Err(e)
            }
        }
    }

    #[timed(backup)]
    pub fn create_incremental_backup(
        &mut self,
        backup_path: &Path, // pre-created by shard manager: backups/incr_123/shard-0/
        backup_id: &str, // shared id from shard manager: "incr_123"
        wal: &Wal,
        document_count: usize,
        progress: &BackupProgress
    ) -> Result<Option<BackupManifest>, BackupError> {
        let parent_id = self.last_backup_id.clone().ok_or(BackupError::NoBaseBackup)?;

        let start_offset = self.last_backup_offset;
        let end_offset = wal.durable_offset();

        if end_offset <= start_offset {
            return Ok(None);
        }

        fs::create_dir_all(backup_path)?;

        let inner = || -> Result<BackupManifest, BackupError> {
            let records = wal
                .replay_from(start_offset)
                .map_err(|e| BackupError::WalError(e.to_string()))?;
            let total: u64 = records
                .iter()
                .map(|(_, payload)| payload.len() as u64)
                .sum();
            progress.grow_total(total);

            let file = File::create(backup_path.join("wal_records.bin"))?;
            let mut w = BufWriter::new(file);
            let mut record_count = 0;

            for (offset, payload) in records {
                if offset <= start_offset || offset > end_offset {
                    continue;
                }
                w.write_all(&offset.to_le_bytes())?;
                w.write_all(&(payload.len() as u32).to_le_bytes())?;
                w.write_all(&payload)?;
                record_count += 1;
            }

            w.flush()?;
            w
                .into_inner()
                .map_err(|e| BackupError::IoError(e.into_error()))?
                .sync_all()?;

            let manifest = BackupManifest {
                backup_id: backup_id.to_string(),
                created_at: chrono::Utc::now().timestamp() as u64,
                backup_type: BackupType::Incremental,
                start_offset,
                wal_offset: end_offset,
                document_count,
                record_count,
                parent_backup_id: Some(parent_id),
            };

            write_manifest_atomic(backup_path, &manifest)?;
            Ok(manifest)
        };

        match inner() {
            Ok(manifest) => {
                self.last_backup_offset = end_offset;
                self.last_backup_id = Some(backup_id.to_string());
                self.save_state()?;
                Ok(Some(manifest))
            }
            Err(e) => {
                let _ = fs::remove_dir_all(backup_path);
                Err(e)
            }
        }
    }
    #[timed(backup)]
    fn delete_incremental_chain(&self, from_id: &str) {
        let mut current_id = from_id.to_string();
        loop {
            let manifest = match self.load_manifest(&current_id) {
                Ok(m) => m,
                Err(_) => {
                    break;
                }
            };

            match manifest.backup_type {
                BackupType::Full => {
                    break;
                }
                BackupType::Incremental => {
                    let path = self.shard_backup_path(&current_id);
                    if let Err(e) = fs::remove_dir_all(&path) {
                        error!(self.log, "failed to delete old incremental {current_id}: {e}");
                    }
                    match manifest.parent_backup_id {
                        Some(parent_id) => {
                            current_id = parent_id;
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }
    }
    #[timed(backup)]
    fn load_manifest(&self, backup_id: &str) -> Result<BackupManifest, BackupError> {
        let path = self.shard_backup_path(backup_id).join("manifest.json");
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    #[timed(restore)]
    pub fn restore(
        &self,
        backup_id: &str,
        target_dir: &Path,
        wal: &mut Wal
    ) -> Result<(), BackupError> {
        let backup_path = self.shard_backup_path(backup_id);
        let manifest = self.load_manifest(backup_id)?;

        match manifest.backup_type {
            BackupType::Full => {
                fs::create_dir_all(target_dir)?;
                decompress_file(
                    &backup_path.join("documents.bin.gz"),
                    &target_dir.join("documents.bin")
                )?;
                let dec = GzDecoder::new(File::open(backup_path.join("index.tar.gz"))?);
                tar::Archive::new(dec).unpack(target_dir)?;
            }
            BackupType::Incremental => {
                let raw = fs::read(backup_path.join("wal_records.bin"))?;
                for (_offset, payload) in parse_wal_records(&raw)? {
                    wal.append(&payload).map_err(|e| BackupError::WalError(e.to_string()))?;
                }
            }
        }

        Ok(())
    }
    #[timed(restore)]
    pub fn restore_chain(
        &mut self,
        backup_id: &str,
        target_dir: &Path,
        wal: &mut Wal
    ) -> Result<(), BackupError> {
        let mut chain = vec![self.load_manifest(backup_id)?];

        while chain.last().unwrap().backup_type == BackupType::Incremental {
            let parent_id = chain
                .last()
                .ok_or(BackupError::NoBackupChain)?
                .parent_backup_id.clone()
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
            self.restore(&manifest.backup_id, target_dir, wal)?;
        }
        self.cleanup_after_restore(backup_id, wal)?;
        Ok(())
    }
    pub fn latest_backup_id(&self) -> Option<&str> {
        self.last_backup_id.as_deref()
    }
    #[timed(restore)]
    pub fn cleanup_after_restore(
        &mut self,
        restored_backup_id: &str,
        wal: &Wal
    ) -> Result<(), BackupError> {
        let restored_manifest = self.load_manifest(restored_backup_id)?;

        // Collect every backup on disk.
        let all_manifests: Vec<BackupManifest> = fs
            ::read_dir(&self.backup_dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let manifest_path = e.path().join(&self.shard_name).join("manifest.json");
                let text = fs::read_to_string(&manifest_path).ok()?;
                serde_json::from_str(&text).ok()
            })
            .collect();

        // Delete any incremental whose start_offset is >= the restored point.
        // These were built on state that no longer exists after the restore.
        for manifest in all_manifests {
            if
                manifest.backup_type == BackupType::Incremental &&
                manifest.start_offset >= restored_manifest.wal_offset
            {
                let path = self.shard_backup_path(&manifest.backup_id);
                if let Err(e) = fs::remove_dir_all(&path) {
                    error!(
                        self.log,
                        "failed to delete stale incremental {}: {}",
                        manifest.backup_id,
                        e //gnj nepareizi
                    );
                }
            }
        }

        // Reset manager state to the restored point so the next incremental
        // starts from the correct offset.
        self.last_backup_id = Some(restored_backup_id.to_string());
        self.last_backup_offset = wal.durable_offset();
        self.save_state()?;

        Ok(())
    }
    #[timed(backup)]
    pub fn list_backups(&self) -> Result<Vec<BackupManifest>, BackupError> {
        let mut all: Vec<BackupManifest> = fs
            ::read_dir(&self.backup_dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let text = fs
                    ::read_to_string(e.path().join(&self.shard_name).join("manifest.json"))
                    .ok()?;
                serde_json::from_str(&text).ok()
            })
            .collect();
        all.sort_by_key(|m| m.created_at);
        Ok(all)
    }
    #[timed(backup)]
    pub fn delete_backups_old(&self, cutoff: SystemTime) -> Result<(), BackupError> {
        let cutoff_ms = cutoff
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        for backup in self.list_backups()? {
            if backup.created_at < cutoff_ms {
                std::fs::remove_dir_all(self.shard_backup_path(&backup.backup_id))?;
            }
        }
        Ok(())
    }

    #[timed(backup)]
    pub fn delete_backup(&self, backup_id: &str) -> Result<(), BackupError> {
        std::fs::remove_dir_all(self.shard_backup_path(backup_id))?;
        Ok(())
    }
}
