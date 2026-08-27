use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Write},
    path::Path,
};

use core_timing::timed;

use crate::{posting::DeleteSet, types::DocId};

const DELETES_FILE: &str = "deletes.txt";

#[timed(database_lifecycle)]
pub fn read_deletes(root: impl AsRef<Path>) -> io::Result<DeleteSet> {
    let path = root.as_ref().join(DELETES_FILE);

    if !path.exists() {
        return Ok(DeleteSet::new());
    }

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut deleted = DeleteSet::new();

    for line in reader.lines() {
        let line = line?;

        if line.trim().is_empty() {
            continue;
        }

        let doc_id: DocId = line
            .trim()
            .parse()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        deleted.delete(doc_id);
    }

    Ok(deleted)
}

#[timed(modifying_documents)]
pub fn append_delete(root: impl AsRef<Path>, doc_id: DocId) -> io::Result<()> {
    let path = root.as_ref().join(DELETES_FILE);

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;

    writeln!(file, "{doc_id}")?;

    Ok(())
}

#[timed(compaction)]
pub fn clear_deletes(root: impl AsRef<Path>) -> io::Result<()> {
    let path = root.as_ref().join(DELETES_FILE);

    if path.exists() {
        std::fs::remove_file(path)?;
    }

    Ok(())
}
