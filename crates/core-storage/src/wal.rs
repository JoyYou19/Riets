//! The WAL is a folder of numbered segment files (`00000000000000000001.wal`, ...). Records
//! are appended to the newest segment. A checkpoint `rotate`s to a fresh segment, and once the
//! store and the index hold everything in the older segments, `remove_through` deletes them.
//! Recovery replays every segment still present, oldest first.
use crate::search_database::DocumentInput;
use bincode::{Decode, Encode};
use core_timing::timed;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Largest payload of a single record (1 GiB).
const MAX_ENTRY_SIZE: u32 = 1024 * 1024 * 1024;
/// Record header: payload length (u32) and CRC (u32).
const HEADER_SIZE: u64 = 8;
const MIN_RECORD_SIZE: u64 = HEADER_SIZE + 1;
/// File extension of WAL segments.
const SEGMENT_EXTENSION: &str = "wal";

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize, Default)]
pub enum SyncMode {
    /// fsync before every append returns, once per write request. An acknowledged write
    /// survives a crash or a power loss.
    #[default]
    SyncEach,
    /// No fsync on append: records survive a process crash (they are in the OS page cache)
    /// but not a power loss. Use it only where losing the last writes is acceptable, such as
    /// benchmarks.
    Manual,
}

#[derive(Serialize, Deserialize, Encode, Decode, Debug, Clone, PartialEq)]
pub enum WalRecord {
    Create(Vec<DocumentInput>),
    Upsert(Vec<DocumentInput>),
    Replace(Vec<DocumentInput>),
    Delete(Vec<String>),
    Clear,
}

/// Borrowed mirror of `WalRecord` for encoding without cloning. The variant order must match
/// `WalRecord`, so both encode to the same bytes.
#[derive(Encode)]
pub enum WalRecordRef<'a> {
    Create(&'a [DocumentInput]),
    Upsert(&'a [DocumentInput]),
    Replace(&'a [DocumentInput]),
    Delete(&'a [String]),
    Clear,
}

struct Inner {
    file: File,
    /// Number of the segment appends go to.
    segment: u64,
    /// End of the last fsynced record in the newest segment.
    durable_offset: u64,
    /// End of the last written record in the newest segment, durable or not.
    written_offset: u64,
    /// Set when an fsync fails: the state on disk is unknown, so every later write is refused.
    poisoned: bool,
}

pub struct Wal {
    inner: Mutex<Inner>,
    mode: SyncMode,
    dir: PathBuf,
}

impl Wal {
    /// Opens the WAL in `dir`, creating the folder and a first segment when needed. A torn
    /// record at the end of the newest segment (a crash mid-append) is cut off.
    #[timed(wal)]
    pub fn open(dir: impl AsRef<Path>, mode: SyncMode) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if dir.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a file, expected the WAL folder", dir.display()),
            ));
        }
        fs::create_dir_all(&dir)?;
        let segment = match list_segments(&dir)?.last() {
            Some(&newest) => newest,
            None => {
                File::create_new(segment_path(&dir, 1))?.sync_all()?;
                sync_dir(&dir)?;
                1
            }
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment_path(&dir, segment))?;
        let valid_end = scan_and_truncate(&mut file)?;
        // Makes the truncation durable.
        file.sync_data()?;
        Ok(Wal {
            inner: Mutex::new(Inner {
                file,
                segment,
                durable_offset: valid_end,
                written_offset: valid_end,
                poisoned: false,
            }),
            mode,
            dir,
        })
    }

    /// Appends one record and returns its byte offset in the newest segment. In `SyncEach`
    /// mode the record is durable when this returns.
    #[timed(wal)]
    pub fn append(&self, payload: &[u8]) -> io::Result<u64> {
        if payload.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty entry"));
        }
        if (payload.len() as u64) > u64::from(MAX_ENTRY_SIZE) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry too large"));
        }
        let mut inner = self.lock();
        if inner.poisoned {
            return Err(io::Error::other("wal poisoned by prior fsync failure"));
        }
        let offset = inner.written_offset;
        let record = encode_record(offset, payload);
        inner.file.seek(SeekFrom::Start(offset))?;
        inner.file.write_all(&record)?;
        inner.written_offset = offset + (record.len() as u64);
        if self.mode == SyncMode::SyncEach {
            sync_or_poison(&mut inner)?;
        }
        core_timing::add_bytes("wal", "append", file!(), record.len() as u64);
        Ok(offset)
    }

    /// End of the last durable record in the newest segment.
    pub fn durable_offset(&self) -> u64 {
        self.lock().durable_offset
    }

    /// Number of the segment appends go to.
    pub fn current_segment(&self) -> u64 {
        self.lock().segment
    }

    /// Starts a new segment and returns the number of the one just closed, which is fsynced
    /// first so it never has a torn end. Every record appended before this call is in that
    /// segment or an older one.
    #[timed(wal)]
    pub fn rotate(&self) -> io::Result<u64> {
        let mut inner = self.lock();
        if inner.poisoned {
            return Err(io::Error::other("wal poisoned"));
        }
        sync_or_poison(&mut inner)?;
        let closed = inner.segment;
        let next = closed + 1;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(segment_path(&self.dir, next))?;
        file.sync_all()?;
        sync_dir(&self.dir)?;
        inner.file = file;
        inner.segment = next;
        inner.durable_offset = 0;
        inner.written_offset = 0;
        Ok(closed)
    }

    /// Deletes every segment numbered `segment` or lower. Call once the store and the index
    /// hold everything in them. The segment appends go to is never deleted.
    #[timed(wal)]
    pub fn remove_through(&self, segment: u64) -> io::Result<()> {
        let inner = self.lock();
        let mut removed = false;
        for id in list_segments(&self.dir)? {
            if id <= segment && id != inner.segment {
                fs::remove_file(segment_path(&self.dir, id))?;
                removed = true;
            }
        }
        if removed {
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// Every record payload written so far, oldest segment first.
    #[timed(wal)]
    pub fn replay_all(&self) -> io::Result<Vec<Vec<u8>>> {
        let mut inner = self.lock();
        let mut payloads = Vec::new();
        for id in list_segments(&self.dir)? {
            let raw = if id == inner.segment {
                let end = usize::try_from(inner.written_offset)
                    .map_err(|_| io::Error::other("WAL segment too large"))?;
                let mut raw = vec![0_u8; end];
                inner.file.seek(SeekFrom::Start(0))?;
                inner.file.read_exact(&mut raw)?;
                raw
            } else {
                fs::read(segment_path(&self.dir, id))?
            };
            let records = parse_records(&raw)
                .map_err(|e| io::Error::new(e.kind(), format!("WAL segment {id}: {e}")))?;
            payloads.extend(records);
        }
        Ok(payloads)
    }

    /// Deletes every older segment and empties the newest one.
    #[timed(wal)]
    pub fn reset(&self) -> io::Result<()> {
        let mut inner = self.lock();
        if inner.poisoned {
            return Err(io::Error::other("wal poisoned"));
        }
        for id in list_segments(&self.dir)? {
            if id != inner.segment {
                fs::remove_file(segment_path(&self.dir, id))?;
            }
        }
        inner.file.set_len(0)?;
        inner.file.seek(SeekFrom::Start(0))?;
        inner.durable_offset = 0;
        inner.written_offset = 0;
        if let Err(e) = inner.file.sync_all() {
            inner.poisoned = true;
            return Err(e);
        }
        sync_dir(&self.dir)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Offsets are updated only after a successful write, so they stay consistent with the
        // file even if a holder panicked.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn sync_or_poison(inner: &mut Inner) -> io::Result<()> {
    if let Err(e) = inner.file.sync_data() {
        inner.poisoned = true; // never retry an fsync: the kernel may have dropped the pages
        return Err(e);
    }
    inner.durable_offset = inner.written_offset;
    Ok(())
}

fn record_crc(offset: u64, len: u32, payload: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&offset.to_le_bytes());
    crc.update(&len.to_le_bytes());
    crc.update(payload);
    crc.finalize()
}

/// One record: length, CRC over (offset, length, payload), payload.
fn encode_record(offset: u64, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity((HEADER_SIZE as usize) + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&record_crc(offset, len, payload).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Payloads of the records in one segment's bytes. Every byte must belong to a valid record.
fn parse_records(raw: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let header = HEADER_SIZE as usize;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < raw.len() {
        if pos + header > raw.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated replay"));
        }
        let len = u32::from_le_bytes(raw[pos..pos + 4].try_into().expect("4 bytes"));
        let crc = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().expect("4 bytes"));
        let start = pos + header;
        let stop = start + (len as usize);
        if len == 0 || len > MAX_ENTRY_SIZE || stop > raw.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated replay"));
        }
        let payload = &raw[start..stop];
        if record_crc(pos as u64, len, payload) != crc {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch in replay"));
        }
        out.push(payload.to_vec());
        pos = stop;
    }
    Ok(out)
}

/// Cuts a torn final record off the file and returns the new end.
fn scan_and_truncate(file: &mut File) -> io::Result<u64> {
    let end = valid_end(file)?;
    if end < file.seek(SeekFrom::End(0))? {
        file.set_len(end)?;
    }
    Ok(end)
}

/// End of the last valid record. Errors when a damaged record sits in front of good data,
/// which is corruption rather than a torn final write.
fn valid_end(file: &mut File) -> io::Result<u64> {
    let file_len = file.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    while pos + MIN_RECORD_SIZE <= file_len {
        match read_record_at(file, pos, file_len)? {
            Some(next) => pos = next,
            None => break,
        }
    }
    if pos < file_len {
        let mut probe = pos + 1;
        while probe + MIN_RECORD_SIZE <= file_len {
            if read_record_at(file, probe, file_len)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "wal corruption at offset {pos}: valid record found at {probe}, \
                         refusing to truncate {} bytes",
                        file_len - pos
                    ),
                ));
            }
            probe += 1;
        }
    }
    Ok(pos)
}

/// `Some(end)` when a valid record starts at `pos`.
fn read_record_at(file: &mut File, pos: u64, file_len: u64) -> io::Result<Option<u64>> {
    file.seek(SeekFrom::Start(pos))?;
    let mut header = [0u8; HEADER_SIZE as usize];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let len = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let stored_crc = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if len == 0 || len > MAX_ENTRY_SIZE || pos + HEADER_SIZE + u64::from(len) > file_len {
        return Ok(None);
    }
    let mut payload = vec![0u8; len as usize];
    if file.read_exact(&mut payload).is_err() {
        return Ok(None);
    }
    if record_crc(pos, len, &payload) != stored_crc {
        return Ok(None);
    }
    Ok(Some(pos + HEADER_SIZE + u64::from(len)))
}

fn segment_path(dir: &Path, segment: u64) -> PathBuf {
    dir.join(format!("{segment:020}.{SEGMENT_EXTENSION}"))
}

/// Segment numbers in `dir`, oldest first. Other files are ignored.
fn list_segments(dir: &Path) -> io::Result<Vec<u64>> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some(SEGMENT_EXTENSION) {
            continue;
        }
        if let Some(id) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()) {
            segments.push(id);
        }
    }
    segments.sort_unstable();
    Ok(segments)
}

/// Makes created or deleted directory entries durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corelamo-wal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn wal_record_ref_matches_owned_encoding() {
        let cfg = bincode::config::standard();
        let ids = vec!["a".to_string(), "b".to_string()];
        let owned = bincode::encode_to_vec(WalRecord::Delete(ids.clone()), cfg).unwrap();
        let borrowed = bincode::encode_to_vec(WalRecordRef::Delete(&ids), cfg).unwrap();
        assert_eq!(owned, borrowed);
    }

    #[test]
    fn rotate_then_remove_keeps_only_newer_records() {
        let dir = temp_dir("rotate");
        let wal = Wal::open(dir.join("wal"), SyncMode::SyncEach).unwrap();
        wal.append(b"one").unwrap();
        wal.append(b"two").unwrap();
        let closed = wal.rotate().unwrap();
        wal.append(b"three").unwrap();
        assert_eq!(
            wal.replay_all().unwrap(),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        wal.remove_through(closed).unwrap();
        assert_eq!(wal.replay_all().unwrap(), vec![b"three".to_vec()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reopen_replays_every_segment_in_order() {
        let dir = temp_dir("reopen");
        {
            let wal = Wal::open(dir.join("wal"), SyncMode::Manual).unwrap();
            wal.append(b"a").unwrap();
            wal.rotate().unwrap();
            wal.append(b"b").unwrap();
            wal.rotate().unwrap();
            wal.append(b"c").unwrap();
        }
        let wal = Wal::open(dir.join("wal"), SyncMode::Manual).unwrap();
        assert_eq!(wal.current_segment(), 3);
        assert_eq!(
            wal.replay_all().unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        wal.append(b"d").unwrap();
        assert_eq!(wal.replay_all().unwrap().len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn torn_tail_of_newest_segment_is_cut_on_open() {
        let dir = temp_dir("torn");
        {
            let wal = Wal::open(dir.join("wal"), SyncMode::SyncEach).unwrap();
            wal.append(b"kept").unwrap();
        }
        let path = segment_path(&dir.join("wal"), 1);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[9, 0, 0, 0, 1, 2]).unwrap();
        drop(file);

        let wal = Wal::open(dir.join("wal"), SyncMode::SyncEach).unwrap();
        assert_eq!(wal.replay_all().unwrap(), vec![b"kept".to_vec()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reset_removes_all_records() {
        let dir = temp_dir("reset");
        let wal = Wal::open(dir.join("wal"), SyncMode::SyncEach).unwrap();
        wal.append(b"x").unwrap();
        wal.rotate().unwrap();
        wal.append(b"y").unwrap();
        wal.reset().unwrap();
        assert!(wal.replay_all().unwrap().is_empty());
        assert_eq!(list_segments(&dir.join("wal")).unwrap(), vec![wal.current_segment()]);
        wal.append(b"z").unwrap();
        assert_eq!(wal.replay_all().unwrap(), vec![b"z".to_vec()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_refuses_a_file_in_place_of_the_folder() {
        let dir = temp_dir("isfile");
        fs::write(dir.join("wal"), b"old").unwrap();
        assert!(Wal::open(dir.join("wal"), SyncMode::SyncEach).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}