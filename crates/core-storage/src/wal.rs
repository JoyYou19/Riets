use crate::search_database::DocumentInput;
use bincode::{ Decode, Encode };
use core_protocol::format::Format;
use core_timing::timed;
use serde::{ Deserialize, Serialize };
use std::fs::{ self, File, OpenOptions };
use std::io::{ self, Read, Seek, SeekFrom, Write };
use std::path::{ Path, PathBuf };
use std::sync::{ Mutex, MutexGuard, PoisonError };

/// Largest payload of a single record (1 GiB).
const MAX_ENTRY_SIZE: u32 = 1024 * 1024 * 1024;
/// Record header: payload length (u32) and CRC (u32).
const HEADER_SIZE: u64 = 8;
const HEADER_LEN: usize = 8;
const MIN_RECORD_SIZE: u64 = HEADER_SIZE + 1;
/// File extension of WAL segments.
const SEGMENT_EXTENSION: &str = "wal";
const _: () = assert!(HEADER_LEN == 8 && HEADER_SIZE == 8);

/// `(payload length, crc)` from a record header.
const fn parse_header(header: [u8; HEADER_LEN]) -> (u32, u32) {
    let [l0, l1, l2, l3, c0, c1, c2, c3] = header;
    (u32::from_le_bytes([l0, l1, l2, l3]), u32::from_le_bytes([c0, c1, c2, c3]))
}

fn record_crc(offset: u64, len: u32, payload: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&offset.to_le_bytes());
    crc.update(&len.to_le_bytes());
    crc.update(payload);
    crc.finalize()
}

/// One record: length, CRC over (offset, length, payload), payload.
fn encode_record(offset: u64, len: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN.saturating_add(payload.len()));
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&record_crc(offset, len, payload).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum SyncMode {
    /// fsync before every append returns, once per write request. An acknowledged write
    /// survives a crash or a power loss.
    // #[default] PRODCTION
    SyncEach,

    #[default]
    Manual,
}

#[derive(Debug, Encode, Decode)]
pub struct WalDocument {
    pub external_id: String,
    pub source: Vec<u8>,
    pub format: Format,
}

#[derive(Encode)]
pub struct WalDocumentRef<'a> {
    pub external_id: &'a str,
    pub source: &'a [u8],
    pub format: Format,
}

impl<'a> From<&'a DocumentInput> for WalDocumentRef<'a> {
    fn from(input: &'a DocumentInput) -> Self {
        Self { external_id: &input.external_id, source: &input.source, format: input.format }
    }
}

#[derive(Encode, Decode)]
pub enum WalRecord {
    Create(Vec<WalDocument>),
    Upsert(Vec<WalDocument>),
    Replace(Vec<WalDocument>),
    Delete(Vec<String>),
    Clear,
}

#[derive(Encode)]
pub enum WalRecordRef<'a> {
    Create(&'a [WalDocumentRef<'a>]),
    Upsert(&'a [WalDocumentRef<'a>]),
    Replace(&'a [WalDocumentRef<'a>]),
    Delete(&'a [String]),
    Clear,
}
struct Inner {
    file: File,
    segment: u64,
    durable_offset: u64,
    written_offset: u64,
    poisoned: bool,
}
/// Iterator returned by `Wal::records`.
pub struct WalRecords {
    dir: PathBuf,
    segments: std::vec::IntoIter<u64>,
    active: u64,
    active_end: u64,
    current: Option<SegmentRecords>,
    done: bool,
}

struct SegmentRecords {
    id: u64,
    reader: io::BufReader<File>,
    pos: u64,
    end: u64,
    n: usize,
}

impl WalRecords {
    fn next_record(&mut self) -> io::Result<Option<(u64, usize, Vec<u8>)>> {
        loop {
            if self.current.is_none() {
                let Some(id) = self.segments.next() else {
                    return Ok(None);
                };
                let file = File::open(segment_path(&self.dir, id))?;
                let end = if id == self.active { self.active_end } else { file.metadata()?.len() };
                self.current = Some(SegmentRecords {
                    id,
                    reader: io::BufReader::with_capacity(1 << 20, file),
                    pos: 0,
                    end,
                    n: 0,
                });
            }
            let Some(segment) = self.current.as_mut() else {
                continue;
            };
            if segment.pos >= segment.end {
                self.current = None;
                continue;
            }
            let id = segment.id;
            return segment
                .read_record()
                .map(|payload| Some((id, payload.0, payload.1)))
                .map_err(|e| io::Error::new(e.kind(), format!("WAL segment {id}: {e}")));
        }
    }
}

impl SegmentRecords {
    /// The record at `pos`. Every byte up to `end` must belong to a valid record.
    fn read_record(&mut self) -> io::Result<(usize, Vec<u8>)> {
        let truncated = || io::Error::new(io::ErrorKind::InvalidData, "truncated replay");
        let header_end = self.pos
            .checked_add(HEADER_SIZE)
            .filter(|&end| end <= self.end)
            .ok_or_else(truncated)?;
        let mut header = [0u8; HEADER_LEN];
        self.reader.read_exact(&mut header)?;
        let (len, crc) = parse_header(header);
        let stop = header_end.checked_add(u64::from(len)).ok_or_else(truncated)?;
        if len == 0 || len > MAX_ENTRY_SIZE || stop > self.end {
            return Err(truncated());
        }
        let payload_len = usize
            ::try_from(len)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut payload = vec![0u8; payload_len];
        self.reader.read_exact(&mut payload)?;
        // the CRC covers the record's offset in its segment
        if record_crc(self.pos, len, &payload) != crc {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch in replay"));
        }
        let n = self.n;
        self.n = n.saturating_add(1);
        self.pos = stop;
        Ok((n, payload))
    }
}

impl Iterator for WalRecords {
    type Item = io::Result<(u64, usize, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_record() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                // a reader in the middle of a damaged record can't resync
                self.done = true;
                Some(Err(e))
            }
        }
    }
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
            return Err(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is a file, expected the WAL folder", dir.display())
                )
            );
        }
        fs::create_dir_all(&dir)?;
        let segment = if let Some(&newest) = list_segments(&dir)?.last() {
            newest
        } else {
            File::create_new(segment_path(&dir, 1))?.sync_all()?;
            sync_dir(&dir)?;
            1
        };
        let mut file = OpenOptions::new().read(true).write(true).open(segment_path(&dir, segment))?;
        let valid_end = scan_and_truncate(&mut file)?;
        // Makes the truncation durable.
        file.sync_data()?;
        Ok(Self {
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

    #[timed(wal)]
    pub fn append(&self, payload: &[u8]) -> io::Result<u64> {
        if payload.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty entry"));
        }
        let len = u32
            ::try_from(payload.len())
            .ok()
            .filter(|&len| len <= MAX_ENTRY_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry too large"))?;
        let record_len = HEADER_SIZE.saturating_add(u64::from(len));
        let offset = {
            let mut inner = self.lock();

            if inner.poisoned {
                return Err(io::Error::other("wal poisoned by prior fsync failure"));
            }
            let offset = inner.written_offset;

            let new_end = offset
                .checked_add(record_len)
                .ok_or_else(|| io::Error::other("wal offset overflow"))?;
            let record = encode_record(offset, len, payload);
            inner.file.seek(SeekFrom::Start(offset))?;
            inner.file.write_all(&record)?;
            inner.written_offset = new_end;
            if self.mode == SyncMode::SyncEach {
                sync_or_poison(&mut inner)?;
            }
            drop(inner);
            offset
        };
        core_timing::add_bytes!("wal", "append", record_len);
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
        drop(inner);
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

    #[timed(wal)]
    pub fn replay_all(&self) -> io::Result<Vec<(u64, usize, Vec<u8>)>> {
        self.records()?.collect()
    }
    pub fn records(&self) -> io::Result<WalRecords> {
        let inner = self.lock();
        Ok(WalRecords {
            dir: self.dir.clone(),
            segments: list_segments(&self.dir)?.into_iter(),
            active: inner.segment,
            active_end: inner.written_offset,
            current: None,
            done: false,
        })
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
        drop(inner);
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
fn has_room(pos: u64, file_len: u64) -> bool {
    file_len.checked_sub(MIN_RECORD_SIZE).is_some_and(|last_start| pos <= last_start)
}

fn valid_end(file: &mut File) -> io::Result<u64> {
    let file_len = file.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    while has_room(pos, file_len) {
        let Some(next) = read_record_at(file, pos, file_len)? else {
            break;
        };
        pos = next;
    }
    if pos < file_len {
        let mut probe = pos.saturating_add(1);
        while has_room(probe, file_len) {
            if read_record_at(file, probe, file_len)?.is_some() {
                return Err(
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "wal corruption at offset {pos}: valid record found at {probe}, \
                         refusing to truncate {} bytes",
                            file_len.saturating_sub(pos)
                        )
                    )
                );
            }
            probe = probe.saturating_add(1);
        }
    }
    Ok(pos)
}

/// `Some(end)` when a valid record starts at `pos`.
fn read_record_at(file: &mut File, pos: u64, file_len: u64) -> io::Result<Option<u64>> {
    file.seek(SeekFrom::Start(pos))?;
    let mut header = [0u8; HEADER_LEN];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let (len, stored_crc) = parse_header(header);
    let Some(end) = pos
        .checked_add(HEADER_SIZE)
        .and_then(|header_end| header_end.checked_add(u64::from(len))) else {
        return Ok(None);
    };
    if len == 0 || len > MAX_ENTRY_SIZE || end > file_len {
        return Ok(None);
    }
    let Ok(payload_len) = usize::try_from(len) else {
        return Ok(None);
    };
    let mut payload = vec![0u8; payload_len];
    if file.read_exact(&mut payload).is_err() || record_crc(pos, len, &payload) != stored_crc {
        return Ok(None);
    }
    Ok(Some(end))
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
        if
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
        {
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
