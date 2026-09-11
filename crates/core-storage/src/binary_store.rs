//WARN: Valter, luudzu piedod ja es generationally sapisu visu ko raskstiji - Kristians (nevis
//Normunds)

//TODO: make the DEFAULT_DOC_CACHE_CAPACITY configurable per database, then persist the locations on

//disk periodically + on shutdown so that we dont have to build it each time + the

//external_id->internal could be saved too, this would massivly improve the speed of startup

//+ check what happens on movies * 10000 + status + search something off about searching

use std::{
    collections::BTreeMap,
    fs::{ File, OpenOptions },
    io::{ self, BufReader, BufWriter, Read, Seek, SeekFrom, Write },
    path::{ Path, PathBuf },
    sync::{ Arc, atomic::{ AtomicU32, Ordering } },
};

use crate::document_store::{ DocumentStore, StoredDocument };
use core_index::types::DocId;
use core_protocol::format::Format;
use core_timing::timed;
use dashmap::DashMap;
use moka::sync::Cache;

const MAGIC: &[u8; 8] = b"CDOCLOG4";

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

//TODO: make configurable per-database
pub const DEFAULT_DOC_CACHE_CAPACITY: u64 = 10000;
pub const DEFAULT_SEGMENT_SIZE: u64 = 128 * 1024 * 1024;
#[derive(Debug)]
pub struct BinaryDocumentStore {
    path: PathBuf,
    docs: Cache<String, StoredDocument>,
    internal_to_external: Arc<DashMap<DocId, String>>,
    locations: Arc<DashMap<String, DocLocation>>,
    current_segment: AtomicU32,
    next_segment_id: AtomicU32,
}
pub struct SegmentCompactionJob {
    pub segment_ids: Vec<u32>,
    pub next_segment_id: u32,
    pub store_dir: PathBuf,
    pub locations_snapshot: Vec<(String, DocLocation)>, // this segment's live entries at plan time
}
#[derive(Debug)]
pub struct CompletedSegmentCompaction {
    pub old_segment_ids: Vec<u32>,
    pub new_segment_id: u32,
    pub tmp_path: PathBuf,
    pub new_locations: Vec<(String, DocLocation)>,
}

impl BinaryDocumentStore {
    #[timed(database_lifecycle)]
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;
        let mut segment_ids = Self::list_segment_ids(&path)?;
        if segment_ids.is_empty() {
            let seg_path = path.join(Self::segment_filename(0));
            let mut file = File::create(&seg_path)?;
            file.write_all(MAGIC)?;
            segment_ids.push(0);
        }
        let current_segment = *segment_ids.last().unwrap();
        let existing_max = Self::list_segment_ids(&path)?.iter().max().copied().unwrap_or(0);
        let mut store = Self {
            path,
            current_segment: AtomicU32::new(current_segment),
            docs: Cache::builder().max_capacity(DEFAULT_DOC_CACHE_CAPACITY).build(),
            internal_to_external: Arc::new(DashMap::new()),
            locations: Arc::new(DashMap::new()),
            next_segment_id: AtomicU32::new(existing_max + 1),
        };

        store.load()?;

        Ok(store)
    }
    pub fn segment_filename(id: u32) -> String {
        format!("seg_{id:05}.bin")
    }
    #[timed(database_lifecycle)]
    pub fn list_segment_ids(root: &Path) -> io::Result<Vec<u32>> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("seg_").and_then(|r| r.strip_suffix(".bin")) {
                if let Ok(id) = rest.parse::<u32>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
    pub fn current_segment_path(&self) -> PathBuf {
        let id = self.current_segment.load(std::sync::atomic::Ordering::Relaxed);
        self.path.join(Self::segment_filename(id))
    }

    #[timed(database_lifecycle)]
    pub fn open_with_maps(
        path: impl AsRef<Path>,
        docs: Cache<String, StoredDocument>,
        internal_to_external: Arc<DashMap<DocId, String>>,
        locations: Arc<DashMap<String, DocLocation>>
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)?;

        let mut segment_ids = Self::list_segment_ids(&path)?;
        if segment_ids.is_empty() {
            let seg_path = path.join(Self::segment_filename(0));
            let mut file = File::create(&seg_path)?;
            file.write_all(MAGIC)?;
            segment_ids.push(0);
        }
        let current_segment = *segment_ids.last().unwrap();
        let next_segment_id = segment_ids.iter().max().copied().unwrap_or(0) + 1;
        docs.invalidate_all();
        internal_to_external.clear();
        locations.clear();

        let mut store = Self {
            path,
            current_segment: AtomicU32::new(current_segment),
            next_segment_id: AtomicU32::new(next_segment_id),
            docs,
            internal_to_external,
            locations,
        };
        if !try_load_maps(&store.path, &store.internal_to_external, &store.locations) {
            store.load()?;
        }
        Ok(store)
    }

    fn maybe_rotate_segment(&self) -> io::Result<()> {
        let seg_path = self.current_segment_path();
        let size = std::fs::metadata(&seg_path)?.len();
        if size < DEFAULT_SEGMENT_SIZE {
            return Ok(());
        }
        let next_id = self.next_segment_id.fetch_add(1, Ordering::Relaxed);
        let next_path = self.path.join(Self::segment_filename(next_id));
        let mut file = File::create(&next_path)?;
        file.write_all(MAGIC)?;
        self.current_segment.store(next_id, Ordering::Relaxed);
        Ok(())
    }
    pub fn plan_compaction(
        &self,
        dead_ratio_threshold: f64,
        target_size: u64
    ) -> io::Result<Option<SegmentCompactionJob>> {
        let current = self.current_segment.load(Ordering::Relaxed);
        // eprintln!("[plan] current_segment={current}");
        let mut candidates = Vec::new();
        for id in Self::list_segment_ids(&self.path)? {
            if id >= current {
                continue;
            }

            let seg_path = self.path.join(Self::segment_filename(id));
            let file = File::open(&seg_path)?;
            let mut reader = CountingReader::new(BufReader::new(file));
            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic)?;
            if &magic != MAGIC {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidData, "bad document store magic").into()
                );
            }
            let mut total = 0u64;
            let mut live_entries = Vec::new();
            let mut live_bytes = 0u64;
            loop {
                match read_u8(&mut reader) {
                    Ok(OP_PUT) => {
                        let start = reader.position();
                        let doc = read_document(&mut reader)?;
                        let doc_size = reader.position() - start;
                        total += doc_size;
                        if let Some(loc) = self.locations.get(&doc.external_id) {
                            if loc.segment == id && loc.offset == start {
                                live_entries.push((doc.external_id.clone(), *loc));
                                live_bytes += doc_size;
                            }
                        }
                    }
                    Ok(OP_DELETE) => {
                        let start = reader.position();
                        let _ = read_string(&mut reader)?;
                        total += reader.position() - start;
                    }
                    Ok(other) => {
                        return Err(
                            io::Error
                                ::new(
                                    io::ErrorKind::InvalidData,
                                    format!("unknown document op {other}")
                                )
                                .into()
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(err) => {
                        return Err(err.into());
                    }
                }
            }
            if total == 0 {
                continue;
            }
            let dead_ratio = 1.0 - (live_bytes as f64) / (total as f64);
            // eprintln!(
            //     "[plan] segment {id}: total={total} live={} dead_ratio={dead_ratio:.3}",
            //     live_entries.len()
            // );
            if dead_ratio >= dead_ratio_threshold {
                candidates.push((id, live_entries, live_bytes));
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }

        // worst (most dead-ratio-worthy) first — list_segment_ids is already
        // ascending by id; sort candidates by live_bytes ascending so the
        // smallest/dirtiest segments get merged first
        candidates.sort_by_key(|(_, _, bytes)| *bytes);
        // eprintln!("[plan] {} candidates found, sorted by live_bytes", candidates.len());
        let mut selected_ids = Vec::new();
        let mut selected_entries = Vec::new();
        let mut running_size = 0u64;
        for (id, entries, bytes) in candidates {
            if !selected_ids.is_empty() && running_size + bytes > target_size {
                break;
            }
            running_size += bytes;
            selected_ids.push(id);
            selected_entries.extend(entries);
            if running_size >= target_size {
                break;
            }
        }

        if selected_ids.is_empty() {
            // eprintln!(
            //     "[plan] only {} segment(s) selected, running_size={}, target_size={}",
            //     selected_ids.len(),
            //     running_size,
            //     target_size
            // );
            return Ok(None);
        }
        let new_segment_id = self.next_segment_id.fetch_add(1, Ordering::Relaxed);
        Ok(
            Some(SegmentCompactionJob {
                segment_ids: selected_ids,
                next_segment_id: new_segment_id,
                store_dir: self.path.clone(),
                locations_snapshot: selected_entries,
            })
        )
    }
    pub fn install_segment_compaction(
        &mut self,
        completed: CompletedSegmentCompaction
    ) -> io::Result<bool> {
        let still_valid = completed.new_locations.iter().all(|(id, _)| {
            self.locations
                .get(id)
                .map(|loc| completed.old_segment_ids.contains(&loc.segment))
                .unwrap_or(false)
        });
        if !still_valid {
            std::fs::remove_file(&completed.tmp_path).ok();
            return Ok(false);
        }

        for (id, loc) in completed.new_locations {
            self.locations.insert(id, loc);
        }

        let final_path = self.path.join(Self::segment_filename(completed.new_segment_id));
        std::fs::rename(&completed.tmp_path, &final_path)?;

        for old_id in &completed.old_segment_ids {
            let old_path = self.path.join(Self::segment_filename(*old_id));
            std::fs::remove_file(old_path).ok();
        }
        Ok(true)
    }
    #[timed(database_lifecycle)]
    fn load(&mut self) -> io::Result<()> {
        for id in Self::list_segment_ids(&self.path)? {
            let seg_path = self.path.join(Self::segment_filename(id));
            let file = File::open(&seg_path)?;
            let mut reader = CountingReader::new(BufReader::new(file));

            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic)?;
            if &magic != MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad document store magic"));
            }

            loop {
                match read_u8(&mut reader) {
                    Ok(OP_PUT) => {
                        let doc_offset = reader.position();
                        let doc = read_document(&mut reader)?;
                        self.internal_to_external.insert(doc.internal_id, doc.external_id.clone());
                        self.locations.insert(doc.external_id.clone(), DocLocation {
                            internal_id: doc.internal_id,
                            offset: doc_offset,
                            segment: id,
                        });
                    }
                    Ok(OP_DELETE) => {
                        let external_id = read_string(&mut reader)?;
                        if let Some((_, loc)) = self.locations.remove(&external_id) {
                            self.internal_to_external.remove(&loc.internal_id);
                        }
                        self.docs.invalidate(&external_id);
                    }
                    Ok(other) => {
                        return Err(
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("unknown document op {other}")
                            )
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(err) => {
                        return Err(err);
                    }
                }
            }
        }
        Ok(())
    }

    fn read_document_at(&self, segment_id: u32, offset: u64) -> io::Result<StoredDocument> {
        read_document_at_path(&self.path, segment_id, offset)
    }

    #[timed(writing_files)]
    fn append_put(&self, doc: &StoredDocument) -> io::Result<u64> {
        let file = OpenOptions::new().append(true).open(&self.current_segment_path())?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);

        write_u8(&mut writer, OP_PUT)?;
        let doc_offset = writer.position();
        write_document(&mut writer, doc)?;
        writer.flush()?;
        self.maybe_rotate_segment()?;
        Ok(doc_offset)
    }

    #[timed(writing_files)]
    fn append_delete(&self, external_id: &str) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.current_segment_path())?;
        let mut writer = BufWriter::new(file);

        write_u8(&mut writer, OP_DELETE)?;
        write_string(&mut writer, external_id)?;
        writer.flush()?;
        self.maybe_rotate_segment()?;
        Ok(())
    }
}

impl DocumentStore for BinaryDocumentStore {
    #[timed(inserting)]
    fn put(&mut self, doc: StoredDocument) -> io::Result<()> {
        let offset = self.append_put(&doc)?;

        self.internal_to_external.insert(doc.internal_id, doc.external_id.clone());
        let current_segment = Self::list_segment_ids(&self.path)?.last().copied().unwrap_or(0);
        self.locations.insert(doc.external_id.clone(), DocLocation {
            internal_id: doc.internal_id,
            offset,
            segment: current_segment,
        });
        self.docs.insert(doc.external_id.clone(), doc);

        Ok(())
    }

    #[timed(inserting)]
    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> io::Result<()> {
        let file = OpenOptions::new().append(true).open(&self.current_segment_path())?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);
        let current_segment = self.current_segment.load(Ordering::Relaxed);
        for doc in docs {
            write_u8(&mut writer, OP_PUT)?;
            let doc_offset = writer.position();
            write_document(&mut writer, &doc)?;

            self.internal_to_external.insert(doc.internal_id, doc.external_id.clone());
            self.locations.insert(doc.external_id.clone(), DocLocation {
                internal_id: doc.internal_id,
                offset: doc_offset,
                segment: current_segment,
            });

            self.docs.insert(doc.external_id.clone(), doc);
        }

        writer.flush()?;
        self.maybe_rotate_segment()?;
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

        let doc = self.read_document_at(loc.segment, loc.offset)?;
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
        let Some(external_id) = self.internal_to_external
            .get(&internal_id)
            .map(|r| r.value().clone()) else {
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
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>
    ) -> io::Result<()> {
        for id in Self::list_segment_ids(&self.path)? {
            let seg_path = self.path.join(Self::segment_filename(id));
            let file = File::open(&seg_path)?;
            let mut reader = CountingReader::new(BufReader::new(file));

            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic)?;
            if &magic != MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad document store magic"));
            }

            loop {
                match read_u8(&mut reader) {
                    Ok(OP_PUT) => {
                        let doc_offset = reader.position();
                        let doc = read_document(&mut reader)?;
                        let is_current = self.locations
                            .get(&doc.external_id)
                            .map(|loc| loc.segment == id && loc.offset == doc_offset)
                            .unwrap_or(false);
                        if is_current {
                            f(&doc)?;
                        }
                    }
                    Ok(OP_DELETE) => {
                        let _external_id = read_string(&mut reader)?;
                    }
                    Ok(other) => {
                        return Err(
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("unknown document op {other}")
                            )
                        );
                    }
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                        break;
                    }
                    Err(err) => {
                        return Err(err);
                    }
                }
            }
        }
        Ok(())
    }

    //WARN: reindex uses this, fills up the RAM again
    #[timed(reindex)]
    fn all_documents(&self) -> io::Result<Vec<StoredDocument>> {
        let mut docs = Vec::new();

        self.for_each_document(
            &mut (|doc| {
                docs.push(doc.clone());
                Ok(())
            })
        )?;

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

    String::from_utf8(bytes).map_err(|_|
        io::Error::new(io::ErrorKind::InvalidData, "invalid utf8 string")
    )
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
pub fn read_document_at_path(
    dir: &Path,
    segment_id: u32,
    offset: u64
) -> io::Result<StoredDocument> {
    let seg_path = dir.join(format!("seg_{segment_id:05}.bin"));
    let mut file = File::open(seg_path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    read_document(&mut reader)
}

#[timed(database_lifecycle)]
pub fn save_maps(path: &Path, locations: &DashMap<String, DocLocation>) -> io::Result<()> {
    let locations_snapshot: Vec<(String, DocLocation)> = locations
        .iter()
        .map(|e| (e.key().clone(), *e.value()))
        .collect();
    let map_tmp = path.with_extension("maps.bin.tmp");
    let map_dst = path.with_extension("maps.bin");
    let bytes = bincode
        ::encode_to_vec(&locations_snapshot, bincode::config::standard())
        .map_err(|e| io::Error::other(format!("failed to encode maps: {e}")))?;
    std::fs::write(&map_tmp, bytes)?;
    std::fs::rename(&map_tmp, &map_dst)?;
    Ok(())
}
#[timed(database_lifecycle)]
fn try_load_maps(
    path: &Path,
    internal_to_external: &DashMap<DocId, String>,
    locations: &DashMap<String, DocLocation>
) -> bool {
    let map_path = path.with_extension("maps.bin");
    let Ok(bytes) = std::fs::read(&map_path) else {
        return false;
    };
    let Ok((entries, _)): Result<
        (Vec<(String, DocLocation)>, usize),
        _
    > = bincode::decode_from_slice(&bytes, bincode::config::standard()) else {
        return false;
    };
    for (external_id, loc) in entries {
        internal_to_external.insert(loc.internal_id, external_id.clone());
        locations.insert(external_id, loc);
    }
    true
}

pub fn run_segment_compaction(job: SegmentCompactionJob) -> io::Result<CompletedSegmentCompaction> {
    let live_ids: std::collections::HashSet<&str> = job.locations_snapshot
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();

    let mut live_docs = Vec::new();
    for &seg_id in &job.segment_ids {
        let seg_path = job.store_dir.join(BinaryDocumentStore::segment_filename(seg_id));
        let file = File::open(&seg_path)?;
        let mut reader = CountingReader::new(BufReader::new(file));
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad document store magic"));
        }
        loop {
            match read_u8(&mut reader) {
                Ok(OP_PUT) => {
                    let doc = read_document(&mut reader)?;
                    if live_ids.contains(doc.external_id.as_str()) {
                        live_docs.push(doc);
                    }
                }
                Ok(OP_DELETE) => {
                    let _ = read_string(&mut reader)?;
                }
                Ok(other) => {
                    return Err(
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unknown document op {other}")
                        )
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }
    }

    let tmp_path = job.store_dir.join(
        format!("{}.tmp", BinaryDocumentStore::segment_filename(job.next_segment_id))
    );
    let file = File::create(&tmp_path)?;
    let mut writer = CountingWriter::new(BufWriter::new(file), 0);
    writer.write_all(MAGIC)?;

    let mut new_locations = Vec::with_capacity(live_docs.len());
    for doc in &live_docs {
        write_u8(&mut writer, OP_PUT)?;
        let offset = writer.position();
        write_document(&mut writer, doc)?;
        new_locations.push((
            doc.external_id.clone(),
            DocLocation {
                internal_id: doc.internal_id,
                offset,
                segment: job.next_segment_id,
            },
        ));
    }
    writer.flush()?;
    writer
        .into_inner()
        .into_inner()
        .map_err(|e| io::Error::other(format!("flush failed: {e}")))?
        .sync_all()?;

    Ok(CompletedSegmentCompaction {
        old_segment_ids: job.segment_ids,
        new_segment_id: job.next_segment_id,
        tmp_path,
        new_locations,
    })
}
//TODO: make this persistend and generate on each load
#[derive(Debug, Clone, Copy, bincode::Encode, bincode::Decode)]
pub struct DocLocation {
    pub internal_id: DocId,
    pub offset: u64,
    pub segment: u32,
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
    fn into_inner(self) -> W {
        self.inner
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

            let read_back = store.read_document_at(loc.segment, loc.offset).unwrap();
            let cached = store.docs.get(external_id).unwrap();

            assert_eq!(read_back.external_id, cached.external_id);
            assert_eq!(read_back.internal_id, loc.internal_id);
            assert_eq!(read_back.internal_id, cached.internal_id);
        }
    }
}
