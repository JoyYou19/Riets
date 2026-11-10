use crate::progress::BackupProgress;
use core_storage::binary_store::BinaryDocumentStore;
use core_timing::timed;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{ Deserialize, Serialize };
use simd_json::prelude::*;
use slog::{ Logger, error, info };
use std::fs::{ self, File, OpenOptions };
use std::io::{ self, BufReader, BufWriter, Read, Seek, SeekFrom, Write };
use std::path::{ Path, PathBuf };
use std::time::SystemTime;
const COPY_BUF_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub backup_id: String,
    pub created_at: u64,
    pub backup_type: BackupType,
    pub start_offset: u64,
    pub document_count: usize,
    pub record_count: usize,
    pub parent_backup_id: Option<String>,
    pub last_backup_segment: u32,
    pub last_backup_offset: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Incremental,
}
pub struct BackupManager {
    backup_dir: PathBuf,
    shard_name: String,
    last_backup_id: Option<String>,
    last_segment_id: u32,
    last_segment_offset: u64,
    log: Logger,
}
#[derive(Debug)]
pub enum BackupError {
    IoError(io::Error),
    SerdeError(simd_json::Error),
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
struct SegmentDiff {
    id: u32,
    start: u64,
    end: u64,
}
struct ProgressWriter<'a, W> {
    inner: W,
    progress: Option<&'a BackupProgress>,
}
impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if let Some(progress) = self.progress {
            progress.add(n as u64);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl From<io::Error> for BackupError {
    fn from(error: io::Error) -> Self {
        BackupError::IoError(error)
    }
}

impl From<simd_json::Error> for BackupError {
    fn from(error: simd_json::Error) -> Self {
        BackupError::SerdeError(error)
    }
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::IoError(e) => write!(f, "IO error: {e}"),
            BackupError::SerdeError(e) => write!(f, "Serde error: {e}"),
            BackupError::BincodeError(e) => write!(f, "Bincode error: {e}"),
            BackupError::WalError(e) => write!(f, "WAL error: {e}"),
            BackupError::CorruptRecord(e) => write!(f, "Corrupt WAL record: {e}"),
            BackupError::NoBackupChain => {
                write!(f, "Could not resolve a full backup at chain root")
            }
            BackupError::NoBaseBackup => write!(f, "No full backup exists to base an increment on"),
            BackupError::ChainGap { parent_id, parent_end, child_id, child_start } =>
                write!(
                    f,
                    "Gap in chain: {parent_id} ends at {parent_end} but {child_id} starts at {child_start}"
                ),
        }
    }
}

impl BackupManager {
    fn shard_backup_path(&self, backup_id: &str) -> PathBuf {
        self.backup_dir.join(backup_id).join(&self.shard_name)
    }
    #[timed(database_lifecycle)]
    pub fn new(
        backup_dir: PathBuf,
        shard_name: String,
        last_segment_id: u32,
        last_segment_offset: u64,
        log:Logger
    ) -> Self {
        let state_path = backup_dir.join(format!("backup_state_{shard_name}.json"));
      

        let (last_segment_id, last_segment_offset, last_backup_id) = if
            let Ok(state) = fs::read_to_string(&state_path)
        {
            let mut buf = state.into_bytes();
            if let Ok(parsed) = simd_json::from_slice::<simd_json::OwnedValue>(&mut buf) {
                let segment = parsed["last_segment_id"]
                    .as_u64()
                    .unwrap_or(u64::from(last_segment_id)) as u32;
                let offset = parsed["last_segment_offset"].as_u64().unwrap_or(0);
                let id = parsed["last_backup_id"].as_str().map(ToString::to_string);
                (segment, offset, id)
            } else {
                (last_segment_id, last_segment_offset, None)
            }
        } else {
            // State file doesn't exist; scan disk for latest backup
            let best = fs
                ::read_dir(&backup_dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|e| {
                    let manifest_path = e.path().join(&shard_name).join("manifest.json");
                    let text = fs::read_to_string(&manifest_path).ok()?;
                    let mut buf = text.into_bytes();
                    simd_json::from_slice::<BackupManifest>(&mut buf).ok()
                })
                .max_by_key(|m| m.created_at);

            match best {
                Some(m) => (m.last_backup_segment, m.last_backup_offset, Some(m.backup_id)),
                None => (last_segment_id, last_segment_offset, None),
            }
        };

        Self {
            backup_dir,
            shard_name,
            last_backup_id,
            last_segment_id,
            last_segment_offset,
            log,
        }
    }

    #[timed(writing_files)]
    fn save_state(&self) -> Result<(), BackupError> {
        let dst = self.backup_dir.join(format!("backup_state_{}.json", self.shard_name));
        let tmp = dst.with_extension("json.tmp");
        let state =
            simd_json::json!({
            "last_backup_id": self.last_backup_id,
            "last_segment_id": self.last_segment_id,
            "last_segment_offset": &self.last_segment_offset,
        });
        fs::write(&tmp, simd_json::to_string(&state)?)?;
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

        progress: &BackupProgress,
        document_count: usize,
        record_count: usize
    ) -> Result<BackupManifest, BackupError> {
        let store_dir = shard_root.join("documents");
        let ids = BinaryDocumentStore::list_segment_ids(&store_dir)?;
        let Some(&last_segment) = ids.last() else {
            return Err(BackupError::IoError(io::Error::other("no document segments to back up")));
        };
        let last_segment_offset = fs
            ::metadata(store_dir.join(BinaryDocumentStore::segment_filename(last_segment)))?
            .len();

        fs::create_dir_all(backup_path)?;

        let total = dir_size(&store_dir)? + dir_size(&shard_root.join("index"))?;
        progress.grow_total(total);

        let result = (|| -> Result<BackupManifest, BackupError> {
            let (idx_result, doc_result) = std::thread::scope(|s| {
                let idx = s.spawn(|| {
                    tar_dir(
                        &shard_root.join("index"),
                        &backup_path.join("index.tar.gz"),
                        "index",
                        Some(progress)
                    )
                });
                let doc = s.spawn(|| {
                    tar_dir(
                        &store_dir,
                        &backup_path.join("documents.tar.gz"),
                        "documents",
                        Some(progress)
                    )
                });
                (idx.join(), doc.join())
            });
            idx_result.map_err(|_| {
                BackupError::IoError(io::Error::other("index tar thread panicked"))
            })??;
            doc_result.map_err(|_| {
                BackupError::IoError(io::Error::other("documents tar thread panicked"))
            })??;

            let manifest = BackupManifest {
                backup_id: backup_id.to_string(),
                created_at: chrono::Utc::now().timestamp_millis() as u64,
                backup_type: BackupType::Full,
                start_offset: 0,
                document_count,
                record_count,
                parent_backup_id: None,
                last_backup_segment: last_segment,
                last_backup_offset: last_segment_offset,
            };
            write_manifest_atomic(backup_path, &manifest)?;
            Ok(manifest)
        })();

        match result {
            Ok(manifest) => {
                let old_id = self.last_backup_id.clone();
                self.last_backup_id = Some(backup_id.to_string());
                // in create_full_backup, where last_backup_offset is set
                self.last_segment_offset = last_segment_offset;
                self.last_segment_id = last_segment;
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

    fn read_segment_diff(
        store_dir: &Path,
        last_segment_id: u32,
        last_segment_offset: u64
    ) -> io::Result<Vec<SegmentDiff>> {
        let mut deltas = Vec::new();
        for id in BinaryDocumentStore::list_segment_ids(store_dir)? {
            if id < last_segment_id {
                continue; // fully covered by an earlier backup
            }
            let seg_path = store_dir.join(BinaryDocumentStore::segment_filename(id));
            let end = fs::metadata(&seg_path)?.len();
            let start = if id == last_segment_id { last_segment_offset } else { 0 };
            if end > start {
                deltas.push(SegmentDiff { id, start, end });
            }
        }
        Ok(deltas)
    }

    pub fn create_incremental_backup(
        &mut self,
        backup_path: &Path,
        backup_id: &str,
        segment_dir: &Path,
        document_count: usize,
        progress: &BackupProgress
    ) -> Result<Option<BackupManifest>, BackupError> {
        let parent_id = self.last_backup_id.clone().ok_or(BackupError::NoBaseBackup)?;

        let diff = Self::read_segment_diff(
            segment_dir,
            self.last_segment_id,
            self.last_segment_offset
        )?;
        if diff.is_empty() {
            return Ok(None);
        }

        let Some(last_delta) = diff.last() else {
            return Err(BackupError::IoError(io::Error::other("no segment diff to commit")));
        };
        let (new_segment_id, new_segment_offset) = (last_delta.id, last_delta.end);

        let inner = || -> Result<BackupManifest, BackupError> {
            let dst_dir = backup_path.join("documents");
            fs::create_dir_all(&dst_dir)?;

            let mut total: u64 = 0;
            for d in &diff {
                let seg_name = BinaryDocumentStore::segment_filename(d.id);
                let mut src = File::open(segment_dir.join(&seg_name))?;
                src.seek(SeekFrom::Start(d.start))?;
                let mut src = src.take(d.end - d.start);

                let dst = File::create(dst_dir.join(&seg_name))?;
                let mut dst = BufWriter::new(dst);
                let copied = io::copy(&mut src, &mut dst)?;
                total += copied;
                dst.flush()?;
                dst
                    .into_inner()
                    .map_err(|e| BackupError::IoError(e.into_error()))?
                    .sync_all()?;
            }
            progress.grow_total(total);

            let manifest = BackupManifest {
                backup_id: backup_id.to_string(),
                created_at: chrono::Utc::now().timestamp() as u64,
                backup_type: BackupType::Incremental,
                start_offset: self.last_segment_offset,
                document_count,
                record_count: 0,
                parent_backup_id: Some(parent_id),
                last_backup_segment: new_segment_id,
                last_backup_offset: new_segment_offset,
            };
            write_manifest_atomic(backup_path, &manifest)?;
            Ok(manifest)
        };

        match inner() {
            Ok(manifest) => {
                self.last_segment_id = new_segment_id;
                self.last_segment_offset = new_segment_offset;
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
        let mut buf = fs::read_to_string(path)?.into_bytes();
        Ok(simd_json::from_slice(&mut buf)?)
    }

    #[timed(restore)]
    pub fn restore(&self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        let backup_path = self.shard_backup_path(backup_id);
        let manifest = self.load_manifest(backup_id)?;
        match manifest.backup_type {
            BackupType::Full => {
                fs::create_dir_all(target_dir)?;
                let (idx_result, doc_result) = std::thread::scope(|s| {
                    let idx = s.spawn(|| {
                        tar::Archive
                            ::new(GzDecoder::new(File::open(backup_path.join("index.tar.gz"))?))
                            .unpack(target_dir)
                    });
                    let doc = s.spawn(|| {
                        tar::Archive
                            ::new(GzDecoder::new(File::open(backup_path.join("documents.tar.gz"))?))
                            .unpack(target_dir)
                    });
                    (idx.join(), doc.join())
                });

                if
                    let Err(e) = idx_result.map_err(|_|
                        io::Error::other("index tar thread panicked")
                    )?
                {
                    error!(self.log, "restore: failed to unpack index.tar.gz"; "target" => %target_dir.display(), "error" => %e);
                    return Err(BackupError::IoError(e));
                }
                if
                    let Err(e) = doc_result.map_err(|_|
                        io::Error::other("documents tar thread panicked")
                    )?
                {
                    error!(self.log, "restore: failed to unpack documents.tar.gz"; "target" => %target_dir.display(), "error" => %e);
                    return Err(BackupError::IoError(e));
                }

                let maps_path = target_dir.join("documents.maps.bin");
                if maps_path.exists() {
                    let _ = fs::remove_file(&maps_path);
                }

                info!(self.log, "restore: unpacked full backup"; "target" => %target_dir.display());
            }
            BackupType::Incremental => {
                let src_dir = backup_path.join("documents");
                let dst_dir = target_dir.join("documents");
                fs::create_dir_all(&dst_dir)?;
                let parent_id = manifest.parent_backup_id
                    .as_ref()
                    .ok_or_else(|| {
                        BackupError::IoError(io::Error::other("incremental backup missing parent"))
                    })?;
                let parent_manifest = self.load_manifest(parent_id)?;

                if src_dir.exists() {
                    for id in BinaryDocumentStore::list_segment_ids(&src_dir)? {
                        let file_name = BinaryDocumentStore::segment_filename(id);
                        let src_path = src_dir.join(&file_name);
                        let dst_path = dst_dir.join(&file_name);

                        let start_offset = if id == parent_manifest.last_backup_segment {
                            parent_manifest.last_backup_offset
                        } else {
                            0
                        };

                        let mut src = File::open(&src_path)?;
                        let mut dst = OpenOptions::new().create(true).write(true).open(&dst_path)?;
                        dst.seek(SeekFrom::Start(start_offset))?;
                        io::copy(&mut src, &mut dst)?;
                    }
                }

                let maps_path = target_dir.join("documents.maps.bin");
                if maps_path.exists() {
                    let _ = fs::remove_file(&maps_path);
                }

                info!(self.log, "restore: applied incremental backup"; "target" => %target_dir.display());
            }
        }
        Ok(())
    }
    #[timed(restore)]
    pub fn restore_chain(&mut self, backup_id: &str, target_dir: &Path) -> Result<(), BackupError> {
        if !is_valid_backup_id(backup_id) {
            return Err(
                BackupError::IoError(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid backup id '{backup_id}'")
                    )
                )
            );
        }
        let mut chain = vec![self.load_manifest(backup_id)?];
        while let Some(last) = chain.last() {
            if last.backup_type != BackupType::Incremental {
                break;
            }
            let parent_id = last.parent_backup_id.clone().ok_or(BackupError::NoBackupChain)?;
            chain.push(self.load_manifest(&parent_id)?);
        }
        chain.reverse();
        // Verify contiguity before touching target_dir. A gap found mid-apply
        // would leave the shard in an undefined state.
       for window in chain.windows(2) {
            let parent = &window[0];
            let child = &window[1];
            if child.last_backup_segment < parent.last_backup_segment
                || (child.last_backup_segment == parent.last_backup_segment
                    && child.start_offset < parent.last_backup_offset)
            {
                return Err(BackupError::ChainGap {
                    parent_id: parent.backup_id.clone(),
                    parent_end: parent.last_backup_offset,
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

    #[timed(restore)]
    pub fn cleanup_after_restore(&mut self, restored_backup_id: &str) -> Result<(), BackupError> {
        let restored_manifest = self.load_manifest(restored_backup_id)?;

        // Collect every backup on disk.
        let all_manifests: Vec<BackupManifest> = fs
            ::read_dir(&self.backup_dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let manifest_path = e.path().join(&self.shard_name).join("manifest.json");
                let text = fs::read_to_string(&manifest_path).ok()?;
                let mut buf = text.into_bytes();
                simd_json::from_slice(&mut buf).ok()
            })
            .collect();
        // Delete any incremental whose start_offset is >= the restored point.
        // These were built on state that no longer exists after the restore.
        for manifest in all_manifests {
            if
                manifest.backup_type == BackupType::Incremental &&
                manifest.start_offset >= restored_manifest.last_backup_offset
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
        self.last_segment_id = restored_manifest.last_backup_segment;
        self.last_segment_offset = restored_manifest.last_backup_offset;
        self.save_state()?;

        Ok(())
    }
    #[timed(backup)]
    pub fn list_backups(&self) -> Result<Vec<BackupManifest>, BackupError> {
        let mut all: Vec<BackupManifest> = fs
            ::read_dir(&self.backup_dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let text = fs
                    ::read_to_string(e.path().join(&self.shard_name).join("manifest.json"))
                    .ok()?;
                let mut buf = text.into_bytes();
                simd_json::from_slice(&mut buf).ok()
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
                fs::remove_dir_all(self.shard_backup_path(&backup.backup_id))?;
            }
        }
        Ok(())
    }
    //when segments rewritten 
    pub fn require_full_backup(&mut self) -> Result<(),BackupError>{
        self.last_backup_id = None;
        self.save_state()
    }
    #[timed(backup)]
    pub fn delete_backup(&self, backup_id: &str) -> Result<(), BackupError> {
        if !is_valid_backup_id(backup_id) {
            return Err(
                BackupError::IoError(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid backup id '{backup_id}'")
                    )
                )
            );
        }
        fs::remove_dir_all(self.shard_backup_path(backup_id))?;
        Ok(())
    }
}

#[timed(writing_files)]
fn copy_with_progress<R: Read, W: Write>(
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
pub fn is_valid_backup_id(id: &str) -> bool {
    !id.is_empty() &&
        id.len() <= 128 &&
        id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
//directory zipping
#[timed(writing_files)]
fn tar_dir(
    src: &Path,
    dst: &Path,
    entry_name: &str,
    progress: Option<&BackupProgress>
) -> io::Result<()> {
    let encoder = GzEncoder::new(File::create(dst)?, Compression::fast());
    let writer = ProgressWriter {
        inner: BufWriter::with_capacity(COPY_BUF_SIZE, encoder),
        progress,
    };
    let mut builder = tar::Builder::new(writer);
    builder.append_dir_all(entry_name, src)?;
    let writer = builder.into_inner()?; // tar end-of-archive blocks
    let encoder = writer.inner.into_inner().map_err(io::IntoInnerError::into_error)?; // buffered bytes
    let file = encoder.finish()?; // gzip trailer
    file.sync_all()
}

#[timed(writing_files)]
pub fn compress_file(src: &Path, dst: &Path, progress: &BackupProgress) -> io::Result<()> {
    let reader = BufReader::new(File::open(src)?);
    let mut encoder = GzEncoder::new(File::create(dst)?, Compression::fast());
    copy_with_progress(reader, &mut encoder, progress)?;
    encoder.finish()?.sync_all()
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    // Windows cannot open a directory as a file; NTFS journals the entries itself
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
#[timed(writing_files)]
fn write_manifest_atomic(backup_path: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    // the archives and their directory entries must be on disk before a manifest points at them
    let documents = backup_path.join("documents");
    if documents.is_dir() {
        sync_dir(&documents)?;
    }
    sync_dir(backup_path)?;
    if let Some(parent) = backup_path.parent() {
        sync_dir(parent)?;
    }

    let tmp = backup_path.join("manifest.json.tmp");
    let dst = backup_path.join("manifest.json");
    fs::write(&tmp, simd_json::to_string(manifest)?)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &dst)?;
    sync_dir(backup_path)?; // the rename itself
    Ok(())
}
