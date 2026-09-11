use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::Path,
};

use core_index::types::DocId;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TestDocument {
    pub id: DocId,
    pub title: String,
    pub body: String,
}

pub fn load_jsonl(path: impl AsRef<Path>) -> io::Result<Vec<TestDocument>> {
    let file = File::open(path)?;

    let reader = BufReader::new(file);

    let mut docs = Vec::new();

    for line in reader.lines() {
        let mut line = line?;

        if line.trim().is_empty() {
            continue;
        }

        let doc: TestDocument = unsafe { simd_json::from_str(&mut line) }
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        docs.push(doc);
    }

    Ok(docs)
}
