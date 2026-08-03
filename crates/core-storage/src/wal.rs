//sis ir logging kas saglabajas pec crash WAL -Write ahead logging
use crate::search_database::DocumentInput;
use std::fs::{ File, OpenOptions };
use std::io::{ self, Read, Seek, SeekFrom, Write };
use std::path::{ Path, PathBuf };
use std::sync::Mutex;
use serde::{ Serialize, Deserialize };
use bincode::{ Encode, Decode };
const MAX_ENTRY_SIZE: u32 = 64 * 1024 * 1024;
const HEADER_SIZE: u64 = 8;
const MIN_RECORD_SIZE: u64 = HEADER_SIZE + 1;

fn record_crc(offset: u64, len: u32, payload: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&offset.to_le_bytes());
    crc.update(&len.to_le_bytes());
    crc.update(payload);
    crc.finalize()
}
#[derive(Clone, Copy, PartialEq)]
pub enum SyncMode {
    SyncEach, // fsync before every acknowledgment (default, safe)
    Manual, // caller must invoke flush(); appends are not durable until then
}

struct Inner {
    file: File,
    durable_offset: u64, // end of last fsynced record; readers may not pass this
    written_offset: u64, // end of last written (possibly not yet durable) record
    poisoned: bool, // set on fsync failure; all further writes refused
}

#[derive(Serialize, Deserialize, Encode, Decode, Debug, Clone, PartialEq)]
pub enum WalRecord {
    Create(Vec<DocumentInput>),
    Upsert(DocumentInput),
    Modify {
        external_id: String,
        payload: Vec<DocumentInput>,
    },
    Delete {
        external_id: String,
    },
    Clear,
}

pub struct Wal {
    inner: Mutex<Inner>,
    mode: SyncMode,
    path: PathBuf,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>, mode: SyncMode) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().create(true).read(true).write(true).open(&path)?;
        let mut inner = Inner {
            file,
            durable_offset: 0,
            written_offset: 0,
            poisoned: false,
        };
        let valid_end = Self::scan_and_truncate(&mut inner.file)?;
        // Make the truncation itself durable: fsync file, then parent dir.
        inner.file.sync_data()?;
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        inner.durable_offset = valid_end;
        inner.written_offset = valid_end;
        Ok(Wal {
            inner: Mutex::new(inner),
            mode,
            path,
        })
    }

    /// Returns the byte offset of the record. In SyncEach mode the record is
    /// durable when this returns Ok. In Manual mode it is NOT durable until flush().
    pub fn append(&self, payload: &[u8]) -> io::Result<u64> {
        if (payload.len() as u64) > (MAX_ENTRY_SIZE as u64) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry too large"));
        }
        let mut g = self.inner.lock().unwrap();
        if g.poisoned {
            return Err(io::Error::other("wal poisoned by prior fsync failure"));
        }
        let offset = g.written_offset;
        // Build the record in one buffer -> one write_all -> one syscall.
        if payload.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty entry"));
        }
        let len = payload.len() as u32;
        let mut buf = Vec::with_capacity((HEADER_SIZE as usize) + payload.len());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&record_crc(offset, len, payload).to_le_bytes());
        buf.extend_from_slice(payload);
        g.file.seek(SeekFrom::Start(offset))?;
        g.file.write_all(&buf)?;
        g.written_offset = offset + (buf.len() as u64);
        if self.mode == SyncMode::SyncEach {
            if let Err(e) = g.file.sync_data() {
                g.poisoned = true; // never retry fsync; state on disk is unknown
                return Err(e);
            }
            g.durable_offset = g.written_offset;
        }
        Ok(offset)
    }

    /// Manual mode: make all pending appends durable (group commit point).
    pub fn flush(&self) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        if g.poisoned {
            return Err(io::Error::other("wal poisoned"));
        }
        if let Err(e) = g.file.sync_data() {
            g.poisoned = true;
            return Err(e);
        }
        g.durable_offset = g.written_offset;
        Ok(())
    }

    /// Readers can never observe a record that was not confirmed durable.
    pub fn read_at(&self, offset: u64) -> io::Result<Vec<u8>> {
        let mut g = self.inner.lock().unwrap();
        if offset >= g.durable_offset {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "offset beyond durable data"));
        }
        g.file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        g.file.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if len == 0 || len > MAX_ENTRY_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "impossible length"));
        }

        let mut payload = vec![0u8; len as usize];
        g.file.read_exact(&mut payload)?;
        if record_crc(offset, len, &payload) != stored_crc {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch"));
        }
        Ok(payload)
    }

    pub fn durable_offset(&self) -> u64 {
        self.inner.lock().unwrap().durable_offset
    }

    /// Scan from byte 0, validate every record, truncate at first invalid one.
    // fn scan_and_truncate(file: &mut File) -> io::Result<u64> {
    //     let file_len = file.seek(SeekFrom::End(0))?;
    //     let mut pos = 0u64;
    //     let mut last_valid_end = 0u64;
    //     while pos + HEADER_SIZE <= file_len {
    //         file.seek(SeekFrom::Start(pos))?;
    //         let mut header = [0u8; 8];
    //         if file.read_exact(&mut header).is_err() {
    //             break;
    //         }
    //         let len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as u64;
    //         let stored_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
    //         if len > (MAX_ENTRY_SIZE as u64) || pos + HEADER_SIZE + len > file_len {
    //             break;
    //         }
    //         let mut payload = vec![0u8; len as usize];
    //         if file.read_exact(&mut payload).is_err() {
    //             break;
    //         }
    //         if crc32fast::hash(&payload) != stored_crc {
    //             break;
    //         }
    //         pos += HEADER_SIZE + len;
    //         last_valid_end = pos;
    //     }
    //     if last_valid_end < file_len {
    //         file.set_len(last_valid_end)?;
    //     }
    //     Ok(last_valid_end)
    // }
    fn scan_and_truncate(file: &mut File) -> io::Result<u64> {
        let file_len = file.seek(SeekFrom::End(0))?;
        let mut pos = 0u64;

        while pos + MIN_RECORD_SIZE <= file_len {
            match Self::read_record_at(file, pos, file_len)? {
                Some(next) => {
                    pos = next;
                }
                None => {
                    break;
                }
            }
        }

        if pos < file_len {
            // Before discarding the tail, check it is genuinely garbage and not
            // one damaged record sitting in front of good data.
            let mut probe = pos + 1;
            while probe + MIN_RECORD_SIZE <= file_len {
                if Self::read_record_at(file, probe, file_len)?.is_some() {
                    return Err(
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "wal corruption at offset {pos}: valid record found at \
                             {probe}, refusing to truncate {} bytes",
                                file_len - pos
                            )
                        )
                    );
                }
                probe += 1;
            }
            file.set_len(pos)?;
        }
        Ok(pos)
    }

    /// Ok(Some(end_offset)) if a valid record starts at `pos`, Ok(None) otherwise.
    fn read_record_at(file: &mut File, pos: u64, file_len: u64) -> io::Result<Option<u64>> {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return Ok(None);
        }
        let len = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if len == 0 || len > MAX_ENTRY_SIZE || pos + HEADER_SIZE + (len as u64) > file_len {
            return Ok(None);
        }
        let mut payload = vec![0u8; len as usize];
        if file.read_exact(&mut payload).is_err() {
            return Ok(None);
        }
        if record_crc(pos, len, &payload) != stored_crc {
            return Ok(None);
        }
        Ok(Some(pos + HEADER_SIZE + (len as u64)))
    }
    fn checkpoint_path(&self) -> PathBuf {
        self.path.with_extension("checkpoint")
    }

    pub fn read_checkpoint(&self) -> io::Result<u64> {
        match std::fs::read(self.checkpoint_path()) {
            Ok(b) if b.len() == 8 => Ok(u64::from_le_bytes(b.try_into().unwrap())),
            Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "bad checkpoint")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub fn write_checkpoint(&self, offset: u64) -> io::Result<()> {
        let final_path = self.checkpoint_path();
        let tmp = final_path.with_extension("checkpoint.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&offset.to_le_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &final_path)?;
        if let Some(dir) = final_path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    pub fn replay_from(&self, from: u64) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let mut g = self.inner.lock().unwrap();
        let end = g.durable_offset;
        if from >= end {
            return Ok(Vec::new());
        }
        g.file.seek(SeekFrom::Start(from))?;
        let mut raw = vec![0u8; (end - from) as usize];
        g.file.read_exact(&mut raw)?;
        drop(g);

        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + (HEADER_SIZE as usize) <= raw.len() {
            let abs = from + (pos as u64);
            let len = u32::from_le_bytes(raw[pos..pos + 4].try_into().unwrap());
            let crc = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap());
            let start = pos + (HEADER_SIZE as usize);
            let stop = start + (len as usize);
            if len == 0 || len > MAX_ENTRY_SIZE || stop > raw.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated replay"));
            }
            let payload = &raw[start..stop];
            if record_crc(abs, len, payload) != crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "crc mismatch in replay"));
            }
            out.push((abs + HEADER_SIZE + (len as u64), payload.to_vec()));
            pos = stop;
        }
        Ok(out)
    }
    pub fn reset(&self) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.file.set_len(0)?;
        g.file.seek(SeekFrom::Start(0))?;
        g.durable_offset = 0;
        g.written_offset = 0;
        Ok(())
    }
}
