use std::{
    fs::{File, OpenOptions}, io::{self, Write}, path::Path,
};

use core_timing::timed;

use crate::{posting::DeleteSet, types::DocId};

const DELETES_FILE: &str = "deletes.txt";

#[timed(database_lifecycle)]
pub fn read_deletes(root: impl AsRef<Path>) -> io::Result<DeleteSet> {
    let path = root.as_ref().join(DELETES_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DeleteSet::new()),
        Err(e) => return Err(e),
    };

    // everything after the last newline was cut off by a crash mid-append
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |pos| pos + 1);
    if complete < bytes.len() {
        OpenOptions::new().write(true).open(&path)?.set_len(complete as u64)?;
    }

    let text = std::str::from_utf8(&bytes[..complete])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
    let mut deleted = DeleteSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let doc_id: DocId = line.parse().map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: bad delete entry {line:?}: {e}", path.display()))
        })?;
        deleted.delete(doc_id);
    }
    Ok(deleted)
}

#[timed(modifying_documents)]
pub fn append_delete(root: impl AsRef<Path>, doc_id: DocId) -> io::Result<()> {
    let path = root.as_ref().join(DELETES_FILE);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    file.write_all(format!("{doc_id}\n").as_bytes())
}

#[timed(compaction)]
pub fn clear_deletes(root: impl AsRef<Path>) -> io::Result<()> {
    let path = root.as_ref().join(DELETES_FILE);

    if path.exists() {
        std::fs::remove_file(path)?;
    }

    Ok(())
}
pub fn sync_deletes(root: impl AsRef<Path>) -> io::Result<()> {
    let path = root.as_ref().join(DELETES_FILE);
    match File::open(&path) {
        Ok(file) => file.sync_data(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
