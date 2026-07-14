use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use core_protocol::format::Format;

use crate::document_store::{DocumentStore, StoredDocument};

const MAGIC: &[u8; 8] = b"CDOCLOG4";

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

#[derive(Debug)]
pub struct BinaryDocumentStore {
    path: PathBuf,
    docs: BTreeMap<String, StoredDocument>,
    internal_to_external: BTreeMap<u64, String>,
}

impl BinaryDocumentStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if !path.exists() {
            let mut file = File::create(&path)?;
            file.write_all(MAGIC)?;
        }

        let mut store = Self {
            path,
            docs: BTreeMap::new(),
            internal_to_external: BTreeMap::new(),
        };

        store.load()?;

        Ok(store)
    }

    fn load(&mut self) -> io::Result<()> {
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);

        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;

        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad document store magic",
            ));
        }

        loop {
            match read_u8(&mut reader) {
                Ok(OP_PUT) => {
                    let doc = read_document(&mut reader)?;

                    self.internal_to_external
                        .insert(doc.internal_id, doc.external_id.clone());

                    self.docs.insert(doc.external_id.clone(), doc);
                }
                Ok(OP_DELETE) => {
                    let external_id = read_string(&mut reader)?;

                    if let Some(doc) = self.docs.remove(&external_id) {
                        self.internal_to_external.remove(&doc.internal_id);
                    }
                }
                Ok(other) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown document op {other}"),
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }

    fn append_put(&self, doc: &StoredDocument) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let mut writer = BufWriter::new(file);

        write_u8(&mut writer, OP_PUT)?;
        write_document(&mut writer, doc)?;
        writer.flush()?;

        Ok(())
    }

    fn append_delete(&self, external_id: &str) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let mut writer = BufWriter::new(file);

        write_u8(&mut writer, OP_DELETE)?;
        write_string(&mut writer, external_id)?;
        writer.flush()?;

        Ok(())
    }
}

impl DocumentStore for BinaryDocumentStore {
    fn put(&mut self, doc: StoredDocument) -> io::Result<()> {
        self.append_put(&doc)?;

        self.internal_to_external
            .insert(doc.internal_id, doc.external_id.clone());

        self.docs.insert(doc.external_id.clone(), doc);

        Ok(())
    }

    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let mut writer = BufWriter::new(file);

        for doc in docs {
            write_u8(&mut writer, OP_PUT)?;
            write_document(&mut writer, &doc)?;

            self.internal_to_external
                .insert(doc.internal_id, doc.external_id.clone());

            self.docs.insert(doc.external_id.clone(), doc);
        }

        writer.flush()?;

        Ok(())
    }

    fn contains(&self, external_id: &str) -> io::Result<bool> {
        Ok(self.docs.contains_key(external_id))
    }

    fn get(&mut self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        Ok(self.docs.get(external_id).cloned())
    }

    fn delete(&mut self, external_id: &str) -> io::Result<()> {
        self.append_delete(external_id)?;

        if let Some(doc) = self.docs.remove(external_id) {
            self.internal_to_external.remove(&doc.internal_id);
        }

        Ok(())
    }

    fn max_internal_id(&self) -> u64 {
        self.docs
            .values()
            .map(|doc| doc.internal_id)
            .max()
            .unwrap_or(0)
    }

    fn get_by_internal_id(&mut self, internal_id: u64) -> io::Result<Option<StoredDocument>> {
        let Some(external_id) = self.internal_to_external.get(&internal_id) else {
            return Ok(None);
        };

        Ok(self.docs.get(external_id).cloned())
    }

    fn document_count(&self) -> usize {
        self.docs.len()
    }

    fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()> {
        for doc in self.docs.values() {
            f(doc)?;
        }

        Ok(())
    }

    fn all_documents(&self) -> io::Result<Vec<StoredDocument>> {
        let mut docs = Vec::new();

        self.for_each_document(&mut |doc| {
            docs.push(doc.clone());
            Ok(())
        })?;

        Ok(docs)
    }
}

fn write_document(writer: &mut impl Write, doc: &StoredDocument) -> io::Result<()> {
    write_string(writer, &doc.external_id)?;
    write_u64(writer, doc.internal_id)?;

    write_u8(writer, doc.format.into())?;

    write_bytes(writer, &doc.source)?;

    write_u32(writer, doc.fields.len() as u32)?;

    for (name, value) in &doc.fields {
        write_string(writer, name)?;
        write_string(writer, value)?;
    }

    Ok(())
}

fn read_document(reader: &mut impl Read) -> io::Result<StoredDocument> {
    let external_id = read_string(reader)?;
    let internal_id = read_u64(reader)?;

    let format = Format::try_from(read_u8(reader)?).map_err(io::Error::from)?;

    let source = read_bytes(reader)?;

    let field_count = read_u32(reader)? as usize;
    let mut fields = BTreeMap::new();

    for _ in 0..field_count {
        let name = read_string(reader)?;
        let value = read_string(reader)?;
        fields.insert(name, value);
    }

    Ok(StoredDocument {
        external_id,
        internal_id,
        source,
        fields,
        format,
    })
}

fn write_bytes(writer: &mut impl Write, value: &[u8]) -> io::Result<()> {
    write_u32(writer, value.len() as u32)?;
    writer.write_all(value)
}

fn read_bytes(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let len = read_u32(reader)? as usize;
    let mut bytes = vec![0u8; len];

    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn write_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    write_u32(writer, bytes.len() as u32)?;
    writer.write_all(bytes)
}

fn read_string(reader: &mut impl Read) -> io::Result<String> {
    let len = read_u32(reader)? as usize;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;

    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf8 string"))
}

fn write_u8(writer: &mut impl Write, value: u8) -> io::Result<()> {
    writer.write_all(&[value])
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut bytes = [0u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
