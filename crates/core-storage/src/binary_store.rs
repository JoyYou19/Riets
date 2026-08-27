//WARN: Valter, luudzu piedod ja es generationally sapisu visu ko raskstiji - Kristians (nevis
//Normunds)

//TODO: make the DEFAULT_DOC_CACHE_CAPACITY configurable per database, then persist the locations on

//disk periodically + on shutdown so that we dont have to build it each time + the

//external_id->internal could be saved too, this would massivly improve the speed of startup

//+ check what happens on movies * 10000 + status + search something off about searching

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use core_index::types::DocId;
use core_protocol::format::Format;
use core_timing::timed;
use dashmap::DashMap;
use moka::sync::Cache;

use crate::document_store::{DocumentStore, StoredDocument};

const MAGIC: &[u8; 8] = b"CDOCLOG4";

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

//TODO: make configurable per-database
const DEFAULT_DOC_CACHE_CAPACITY: u64 = 10000;

#[derive(Debug)]
pub struct BinaryDocumentStore {
    path: PathBuf,
    docs: Cache<String, StoredDocument>,
    internal_to_external: Arc<DashMap<DocId, String>>,
    locations: Arc<DashMap<String, DocLocation>>,
}

impl BinaryDocumentStore {
    #[timed(database_lifecycle)]
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
            docs: Cache::builder()
                .max_capacity(DEFAULT_DOC_CACHE_CAPACITY)
                .build(),
            internal_to_external: Arc::new(DashMap::new()),
            locations: Arc::new(DashMap::new()),
        };

        store.load()?;

        Ok(store)
    }

    #[timed(database_lifecycle)]
    pub fn open_with_maps(
        path: impl AsRef<Path>,
        docs: Cache<String, StoredDocument>,
        internal_to_external: Arc<DashMap<DocId, String>>,
        locations: Arc<DashMap<String, DocLocation>>,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            let mut file = File::create(&path)?;
            file.write_all(MAGIC)?;
        }

        docs.invalidate_all();
        internal_to_external.clear();
        locations.clear();

        let mut store = Self {
            path,
            docs,
            internal_to_external,
            locations,
        };
        store.load()?;
        Ok(store)
    }

    #[timed(database_lifecycle)]
    fn load(&mut self) -> io::Result<()> {
        let file = File::open(&self.path)?;
        let mut reader = CountingReader::new(BufReader::new(file));

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
                    let doc_offset = reader.position();
                    let doc = read_document(&mut reader)?;

                    self.internal_to_external
                        .insert(doc.internal_id, doc.external_id.clone());
                    self.locations.insert(
                        doc.external_id.clone(),
                        DocLocation {
                            internal_id: doc.internal_id,
                            offset: doc_offset,
                        },
                    );
                    //IET?
                    //self.docs.insert(doc.external_id.clone(), doc);
                }
                //TODO: periodic cleanup for deleted documents
                Ok(OP_DELETE) => {
                    let external_id = read_string(&mut reader)?;
                    if let Some((_, loc)) = self.locations.remove(&external_id) {
                        self.internal_to_external.remove(&loc.internal_id);
                    }
                    self.docs.invalidate(&external_id);
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

    fn read_document_at(&self, offset: u64) -> io::Result<StoredDocument> {
        read_document_at_path(&self.path, offset)
    }

    #[timed(writing_files)]
    fn append_put(&self, doc: &StoredDocument) -> io::Result<u64> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);

        write_u8(&mut writer, OP_PUT)?;
        let doc_offset = writer.position();
        write_document(&mut writer, doc)?;
        writer.flush()?;

        Ok(doc_offset)
    }

    #[timed(writing_files)]
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
    #[timed(inserting)]
    fn put(&mut self, doc: StoredDocument) -> io::Result<()> {
        let offset = self.append_put(&doc)?;

        self.internal_to_external
            .insert(doc.internal_id, doc.external_id.clone());

        self.locations.insert(
            doc.external_id.clone(),
            DocLocation {
                internal_id: doc.internal_id,
                offset,
            },
        );
        self.docs.insert(doc.external_id.clone(), doc);

        Ok(())
    }

    #[timed(inserting)]
    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.path)?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);

        for doc in docs {
            write_u8(&mut writer, OP_PUT)?;
            let doc_offset = writer.position();
            write_document(&mut writer, &doc)?;

            self.internal_to_external
                .insert(doc.internal_id, doc.external_id.clone());

            self.locations.insert(
                doc.external_id.clone(),
                DocLocation {
                    internal_id: doc.internal_id,
                    offset: doc_offset,
                },
            );

            self.docs.insert(doc.external_id.clone(), doc);
        }

        writer.flush()?;

        Ok(())
    }

    fn contains(&self, external_id: &str) -> io::Result<bool> {
        Ok(self.locations.contains_key(external_id))
    }

    //either read from ram else read the exact document from the file
    #[timed(retrieve_opps)]
    fn get(&self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        if let Some(doc) = self.docs.get(external_id) {
            return Ok(Some(doc));
        }

        let Some(loc) = self.locations.get(external_id).map(|r| *r.value()) else {
            return Ok(None);
        };

        let doc = self.read_document_at(loc.offset)?;
        self.docs.insert(external_id.to_string(), doc.clone());
        Ok(Some(doc))
    }

    #[timed(modifying_documents)]
    fn delete(&mut self, external_id: &str) -> io::Result<()> {
        self.append_delete(external_id)?;
        if let Some((_, loc)) = self.locations.remove(external_id) {
            self.internal_to_external.remove(&loc.internal_id);
        }
        self.locations.remove(external_id);
        self.docs.invalidate(external_id);

        Ok(())
    }

    fn max_internal_id(&self) -> DocId {
        self.locations
            .iter()
            .map(|entry| entry.value().internal_id)
            .max()
            .unwrap_or(0)
    }

    #[timed(retrieve_opps)]
    fn get_by_internal_id(&self, internal_id: DocId) -> io::Result<Option<StoredDocument>> {
        let Some(external_id) = self
            .internal_to_external
            .get(&internal_id)
            .map(|r| r.value().clone())
        else {
            return Ok(None);
        };
        self.get(&external_id)
    }

    fn document_count(&self) -> usize {
        self.locations.len()
    }

    //INFO: karoc sis ir jauns foreach ko chatins rakstija no clue, bet nu taa kaa vairs viss nestaav
    //ramaa sii funkcija sanaak daudz kompleksaaka, nav ko dariit
    #[timed(reindex)]
    fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()> {
        let file = File::open(&self.path)?;
        let mut reader = CountingReader::new(BufReader::new(file));

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
                    let doc_offset = reader.position();
                    let doc = read_document(&mut reader)?;

                    let is_current = self
                        .locations
                        .get(&doc.external_id)
                        .map(|loc| loc.offset == doc_offset)
                        .unwrap_or(false);

                    if is_current {
                        f(&doc)?;
                    }
                }
                Ok(OP_DELETE) => {
                    let _external_id = read_string(&mut reader)?;
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

    //WARN: reindex uses this, fills up the RAM again
    #[timed(reindex)]
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

//helper so that rust compiler isnt angry at me cause this will be called from the ShardHandle not
//ShardDb
#[timed(disk_io)]
pub fn read_document_at_path(path: &Path, offset: u64) -> io::Result<StoredDocument> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    read_document(&mut reader)
}

//a map containing internal_id -> byte position in the documents.bin for a really fast retrieval
//TODO: make this persistend and generate on each load
#[derive(Debug, Clone, Copy)]
pub struct DocLocation {
    pub internal_id: DocId,
    pub offset: u64,
}

//helper 1
struct CountingReader<R> {
    inner: R,
    pos: u64,
}

impl<R: Read> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, pos: 0 }
    }

    fn position(&self) -> u64 {
        self.pos
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

//helper 2
struct CountingWriter<W> {
    inner: W,
    pos: u64,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W, start: u64) -> Self {
        Self { inner, pos: start }
    }

    fn position(&self) -> u64 {
        self.pos
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

//chatins uztaisija teica lai paskatos
#[cfg(test)]
mod position_check {
    use super::*;

    #[test]
    fn offsets_match_stored_docs() {
        let store = BinaryDocumentStore::open("/tmp/test_corelamo").unwrap();

        for entry in store.locations.iter() {
            let external_id = entry.key();
            let loc = entry.value();

            let read_back = store.read_document_at(loc.offset).unwrap();
            let cached = store.docs.get(external_id).unwrap();

            assert_eq!(read_back.external_id, cached.external_id);
            assert_eq!(read_back.internal_id, loc.internal_id);
            assert_eq!(read_back.internal_id, cached.internal_id);
        }
    }
}
