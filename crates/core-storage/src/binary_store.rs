//WARN: Valter, luudzu piedod ja es generationally sapisu visu ko raskstiji - Kristians (nevis
//Normunds)

//TODO: make the DEFAULT_DOC_CACHE_CAPACITY configurable per database, then persist the locations on

//disk periodically + on shutdown so that we dont have to build it each time + the

//external_id->internal could be saved too, this would massivly improve the speed of startup

//+ check what happens on movies * 10000 + status + search something off about searching

use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use crate::document_store::{DocumentStore, StoredDocument};
use core_index::types::DocId;
use core_protocol::format::Format;
use core_timing::timed;
use dashmap::DashMap;
use moka::sync::Cache;

//                  hihi haha part 2
const MAGIC: &[u8; 8] = b"BANANA_E";
/// The header of a segment written by store compaction. Until the maps file lists it, such a
/// segment is an unfinished install: its documents are copies, and the old segments still hold them.
const COMPACTED_MAGIC: &[u8; 8] = b"BANANA_C";
/// The header of the maps file. A file without it is from before segment end offsets were
/// recorded, and is ignored: the store is scanned instead.
const MAPS_MAGIC: &[u8; 8] = b"CLMAPS02";
const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

//TODO: make configurable per-database
pub const DEFAULT_DOC_CACHE_CAPACITY: u64 = 10000;
pub const DEFAULT_SEGMENT_SIZE: u64 = 128 * 1024 * 1024;

//JAUNS COMPACTIONS KONCEPTS
const MAPS_EXTENTION: &str = "maps.bin";
pub const MAPS_TMP_EXTENTION: &str = "maps.bin.tmp";
const COMPACTION_IO_BUFFER: usize = 1 << 20;
const COMPACTION_MAX_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;
const MAX_CONCURRENT_COMPACTIONS: usize = 2;

static COMPACTION_SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
struct CompactionPermit;

impl CompactionPermit {
    fn acquire() -> Self {
        let (lock, available) = &COMPACTION_SLOTS;
        let mut running = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *running >= MAX_CONCURRENT_COMPACTIONS {
            running = available
                .wait(running)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *running += 1;
        CompactionPermit
    }
}

impl Drop for CompactionPermit {
    fn drop(&mut self) {
        let (lock, available) = &COMPACTION_SLOTS;
        let mut running = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *running -= 1;
        available.notify_one();
    }
}

struct Throttle {
    started: std::time::Instant,
    bytes: u64,
    bytes_per_sec: u64,
}

impl Throttle {
    fn new(bytes_per_sec: u64) -> Self {
        Self {
            started: std::time::Instant::now(),
            bytes: 0,
            bytes_per_sec,
        }
    }

    fn consume(&mut self, bytes: u64) {
        self.bytes += bytes;
        let due = std::time::Duration::from_secs_f64(self.bytes as f64 / self.bytes_per_sec as f64);
        let elapsed = self.started.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
    }
}

fn with_path(err: io::Error, path: &Path) -> io::Error {
    io::Error::new(err.kind(), format!("{}: {}", path.display(), err))
}

//VISS IR TRIVIALI
#[derive(Debug)]
pub struct BinaryDocumentStore {
    path: PathBuf,
    docs: Cache<String, StoredDocument>,
    internal_to_external: Arc<DashMap<DocId, String>>,
    locations: Arc<DashMap<String, DocLocation>>,
    current_segment: AtomicU32,
    next_segment_id: AtomicU32,
    unsynced_from:AtomicU32
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
//TODO: make this persistend and generate on each load
#[derive(Debug, Clone, Copy, bincode::Encode, bincode::Decode)]
pub struct DocLocation {
    pub internal_id: DocId,
    pub offset: u64,
    pub segment: u32,
    pub len: u32,
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
        let existing_max = Self::list_segment_ids(&path)?
            .iter()
            .max()
            .copied()
            .unwrap_or(0);
        let mut store = Self {
            path,
            current_segment: AtomicU32::new(current_segment),
            docs: Cache::builder()
                .max_capacity(DEFAULT_DOC_CACHE_CAPACITY)
                .build(),
            internal_to_external: Arc::new(DashMap::new()),
            locations: Arc::new(DashMap::new()),
            next_segment_id: AtomicU32::new(existing_max + 1),
            unsynced_from: AtomicU32::new(current_segment),
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
            if let Some(rest) = name
                .strip_prefix("seg_")
                .and_then(|r| r.strip_suffix(".bin"))
            {
                if let Ok(id) = rest.parse::<u32>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
    pub fn current_segment_path(&self) -> PathBuf {
        let id = self
            .current_segment
            .load(Ordering::Relaxed);
        self.path.join(Self::segment_filename(id))
    }

    #[timed(database_lifecycle)]
    pub fn open_with_maps(
        path: impl AsRef<Path>,
        docs: Cache<String, StoredDocument>,
        internal_to_external: Arc<DashMap<DocId, String>>,
        locations: Arc<DashMap<String, DocLocation>>,
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
            unsynced_from: AtomicU32::new(current_segment),
            docs,
            internal_to_external,
            locations,
        };
       if !store.load_from_maps()? {
            store.internal_to_external.clear();
            store.locations.clear();
            store.load()?;
        }
        Ok(store)
    }

    fn maybe_rotate_segment(&self) -> io::Result<()> {
        let seg_path = self.current_segment_path();
        let size = std::fs::metadata(&seg_path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", seg_path.display(), e)))?
            .len();
        if size < DEFAULT_SEGMENT_SIZE {
            return Ok(());
        }
        let next_id = self.next_segment_id.fetch_add(1, Ordering::Relaxed);
        let next_path = self.path.join(Self::segment_filename(next_id));
        let mut file = File::create(&next_path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", next_path.display(), e)))?;
        file.write_all(MAGIC)?;
        self.current_segment.store(next_id, Ordering::Relaxed);
        Ok(())
    }

    pub fn plan_compaction(
        &self,
        dead_ratio_threshold: f64,
        target_size: u64,
    ) -> io::Result<Option<SegmentCompactionJob>> {
        let current = self.current_segment.load(Ordering::Acquire);

        let mut live_bytes_by_segment: ahash::AHashMap<u32, u64> = ahash::AHashMap::new();
        for entry in self.locations.iter() {
            let location = entry.value();
            if location.segment < current {
                *live_bytes_by_segment.entry(location.segment).or_default() +=
                    location.len as u64 + 1;
            }
        }

        let mut candidates: Vec<(u32, u64)> = Vec::new();
        for id in Self::list_segment_ids(&self.path)? {
            if id >= current {
                continue;
            }
            let seg_path = self.path.join(Self::segment_filename(id));
            let file_len = match std::fs::metadata(&seg_path) {
                Ok(meta) => meta.len(),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(with_path(err, &seg_path)),
            };
            let total = file_len.saturating_sub(MAGIC.len() as u64);
            if total == 0 {
                continue;
            }
            let live = live_bytes_by_segment
                .get(&id)
                .copied()
                .unwrap_or(0)
                .min(total);
            let dead_ratio = 1.0 - (live as f64) / (total as f64);
            if dead_ratio >= dead_ratio_threshold {
                candidates.push((id, live));
            }
        }

        if candidates.is_empty() {
            return Ok(None);
        }
        candidates.sort_unstable_by_key(|&(_, live)| live);

        let mut selected_ids = Vec::new();
        let mut running_size = 0u64;
        for (id, live) in candidates {
            if !selected_ids.is_empty() && running_size + live > target_size {
                break;
            }
            running_size += live;
            selected_ids.push(id);
            if running_size >= target_size {
                break;
            }
        }

        let selected: ahash::AHashSet<u32> = selected_ids.iter().copied().collect();
        let mut entries: Vec<(String, DocLocation)> = self
            .locations
            .iter()
            .filter(|entry| selected.contains(&entry.value().segment))
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        entries.sort_unstable_by_key(|(_, location)| (location.segment, location.offset));

        let new_segment_id = self.next_segment_id.fetch_add(1, Ordering::AcqRel);
        Ok(Some(SegmentCompactionJob {
            segment_ids: selected_ids,
            next_segment_id: new_segment_id,
            store_dir: self.path.clone(),
            locations_snapshot: entries,
        }))
    }

    pub fn install_segment_compaction(
        &mut self,
        completed: CompletedSegmentCompaction,
    ) -> io::Result<bool> {
        let final_path = self
            .path
            .join(Self::segment_filename(completed.new_segment_id));
        std::fs::rename(&completed.tmp_path, &final_path).map_err(|e| with_path(e, &final_path))?;
        File::open(&self.path)?.sync_all()?;

        let mut installed = 0usize;
        for (external_id, new_location) in completed.new_locations {
            if let Some(mut current) = self.locations.get_mut(&external_id) {
                if completed.old_segment_ids.contains(&current.segment) {
                    *current = new_location;
                    installed += 1;
                }
            }
        }

        // The persisted map must stop referencing the old segments before they are deleted.
        save_maps(&self.path, &self.locations)?;

        for old_id in &completed.old_segment_ids {
            let old_path = self.path.join(Self::segment_filename(*old_id));
            if let Err(err) = std::fs::remove_file(&old_path) {
                if err.kind() != io::ErrorKind::NotFound {
                    return Err(with_path(err, &old_path));
                }
            }
        }

        Ok(installed > 0)
    }

    #[timed(database_lifecycle)]
        /// Builds the maps by reading every segment, oldest first.
    fn load(&mut self) -> io::Result<()> {
        let ids = Self::list_segment_ids(&self.path)?;
        let last = ids.last().copied();
        for id in ids {
            self.scan_segment(id, None, Some(id) == last)?;
        }
        Ok(())
    }

    /// Applies one segment's records to the maps, from `from` (a byte offset just past a
    /// record) or from the start. A record cut short by a crash at the end of the last segment
    /// is cut off, so later appends start on a clean boundary; anywhere else it is an error.
    fn scan_segment(&self, id: u32, from: Option<u64>, is_last: bool) -> io::Result<()> {
        let seg_path = self.path.join(Self::segment_filename(id));
        let mut file = File::open(&seg_path).map_err(|e| with_path(e, &seg_path))?;
        let start = match from {
            Some(offset) => {
                file.seek(SeekFrom::Start(offset))?;
                offset
            }
            None => {
                let mut magic = [0u8; 8];
                file.read_exact(&mut magic)?;
                if &magic != MAGIC && &magic != COMPACTED_MAGIC {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: bad document store magic", seg_path.display()),
                    ));
                }
                MAGIC.len() as u64
            }
        };
        let mut reader = CountingReader::new(BufReader::new(file));
        let mut record_start = start;
        loop {
            let op = match read_u8(&mut reader) {
                Ok(op) => op,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };
            let applied = match op {
                OP_PUT => {
                    let doc_offset = start + reader.position();
                    read_document(&mut reader).map(|doc| {
                        let len = (start + reader.position() - doc_offset) as u32;
                        if let Some(old) = self.locations.insert(
                            doc.external_id.clone(),
                            DocLocation { internal_id: doc.internal_id, offset: doc_offset, segment: id, len },
                        ) {
                            if old.internal_id != doc.internal_id {
                                self.internal_to_external.remove(&old.internal_id);
                            }
                        }
                        self.internal_to_external.insert(doc.internal_id, doc.external_id.clone());
                        self.docs.invalidate(&doc.external_id);
                    })
                }
                OP_DELETE => read_string(&mut reader).map(|external_id| {
                    if let Some((_, loc)) = self.locations.remove(&external_id) {
                        self.internal_to_external.remove(&loc.internal_id);
                    }
                    self.docs.invalidate(&external_id);
                }),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: unknown document op {other} at {record_start}", seg_path.display()),
                    ));
                }
            };
            match applied {
                Ok(()) => record_start = start + reader.position(),
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof && is_last => {
                    OpenOptions::new().write(true).open(&seg_path)?.set_len(record_start)?;
                    return Ok(());
                }
                Err(err) => return Err(with_path(err, &seg_path)),
            }
        }
    }

    /// Loads the maps file and reads only what was written after it was saved. Returns false
    /// when the file is missing, from an older version, or does not match the segments on disk;
    /// the maps are then left empty for a full scan.
    fn load_from_maps(&self) -> io::Result<bool> {
        let Some(saved) = read_maps(&self.path) else {
            return Ok(false);
        };
        let ids = Self::list_segment_ids(&self.path)?;
        let mut tails: Vec<(u32, u64)> = Vec::new();
        for &(id, end) in &saved.segment_ends {
            let seg_path = self.path.join(Self::segment_filename(id));
            match std::fs::metadata(&seg_path) {
                Ok(meta) if meta.len() >= end => tails.push((id, end)),
                // shorter than when the maps were saved, or gone: the file does not describe this store
                _ => return Ok(false),
            }
        }
        for (external_id, loc) in saved.entries {
            self.internal_to_external.insert(loc.internal_id, external_id.clone());
            self.locations.insert(external_id, loc);
        }
        let last = ids.last().copied();
        // what was appended to the segments the file knows, then segments created since, in order
        for (id, end) in tails {
            self.scan_segment(id, Some(end), Some(id) == last)?;
        }
        for id in ids {
            if saved.segment_ends.iter().any(|&(known, _)| known == id) {
                continue;
            }
            let seg_path = self.path.join(Self::segment_filename(id));
            if segment_magic(&seg_path)? == *COMPACTED_MAGIC {
                // a compaction output whose install did not finish: the old segments still hold
                // these documents, and the maps never pointed here
                std::fs::remove_file(&seg_path).map_err(|e| with_path(e, &seg_path))?;
                continue;
            }
            self.scan_segment(id, None, Some(id) == last)?;
        }
        let remaining = Self::list_segment_ids(&self.path)?;
        if let Some(&newest) = remaining.last() {
            self.current_segment.store(newest, Ordering::Relaxed);
            self.unsynced_from.store(newest, Ordering::Relaxed);
        }
        Ok(true)
    }

    fn read_document_at(&self, segment_id: u32, offset: u64) -> io::Result<StoredDocument> {
        read_document_at_path(&self.path, segment_id, offset)
    }

    #[timed(writing_files)]
    #[timed(writing_files)]
    fn append_put(&self, doc: &StoredDocument) -> io::Result<DocLocation> {
        let segment = self.current_segment.load(Ordering::Acquire);
        let seg_path = self.path.join(Self::segment_filename(segment));
        let file = OpenOptions::new()
            .append(true)
            .open(&seg_path)
            .map_err(|e| with_path(e, &seg_path))?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);

        write_u8(&mut writer, OP_PUT)?;
        let offset = writer.position();
        write_document(&mut writer, doc)?;
        let len = (writer.position() - offset) as u32;
        writer.flush()?;
        self.maybe_rotate_segment()?;

        Ok(DocLocation {
            internal_id: doc.internal_id,
            offset,
            segment,
            len,
        })
    }

    #[timed(writing_files)]
    fn append_delete(&self, external_id: &str) -> io::Result<()> {
        let file = OpenOptions::new()
            .append(true)
            .open(&self.current_segment_path())?;
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
        let location = self.append_put(&doc)?;
        self.internal_to_external
            .insert(doc.internal_id, doc.external_id.clone());
        self.locations.insert(doc.external_id.clone(), location);
        self.docs.insert(doc.external_id.clone(), doc);
        Ok(())
    }

    #[timed(inserting)]
    fn put_batch(&mut self, docs: Vec<StoredDocument>) -> io::Result<()> {
        let seg_path = self.current_segment_path();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", seg_path.display(), e)))?;
        let start = file.metadata()?.len();
        let mut writer = CountingWriter::new(BufWriter::new(file), start);
        let current_segment = self.current_segment.load(Ordering::Relaxed);
        for doc in docs {
            write_u8(&mut writer, OP_PUT)?;
            let doc_offset = writer.position();
            write_document(&mut writer, &doc)?;
            let len = (writer.position() - doc_offset) as u32;
            self.internal_to_external
                .insert(doc.internal_id, doc.external_id.clone());
            self.locations.insert(
                doc.external_id.clone(),
                DocLocation {
                    internal_id: doc.internal_id,
                    offset: doc_offset,
                    segment: current_segment,
                    len,
                },
            );

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
        for id in Self::list_segment_ids(&self.path)? {
            let seg_path = self.path.join(Self::segment_filename(id));
            let file = File::open(&seg_path)?;
            let mut reader = CountingReader::new(BufReader::new(file));

            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic)?;
            if &magic != MAGIC && &magic !=COMPACTED_MAGIC {
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
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("unknown document op {other}"),
                        ));
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
            }),
        )?;

        Ok(docs)
    }
    fn sync(&self) -> io::Result<()> {
        let from = self.unsynced_from.load(Ordering::Acquire);
        let current = self.current_segment.load(Ordering::Acquire);
        for id in from..=current {
            let path = self.path.join(Self::segment_filename(id));
            match File::open(&path) {
                Ok(file) => file.sync_data().map_err(|e| with_path(e, &path))?,
                // Ids in this range can belong to compaction outputs that were
                // already synced and later removed.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(with_path(e, &path)),
            }
        }
        if current != from {
            // A rotation created at least one new file: persist its directory entry.
            File::open(&self.path)?.sync_all()?;
        }
        self.unsynced_from.store(current, Ordering::Release);
        Ok(())
    }
}

fn write_document(writer: &mut impl Write, doc: &StoredDocument) -> io::Result<()> {
    write_string(writer, &doc.external_id)?;
    write_u64(writer, doc.internal_id)?;
    write_u8(writer, doc.format.into())?;
    write_bytes(writer, &doc.source)?;

    Ok(())
}

fn read_document(reader: &mut impl Read) -> io::Result<StoredDocument> {
    let external_id = read_string(reader)?;
    let internal_id = read_u64(reader)?;
    let format = Format::try_from(read_u8(reader)?).map_err(io::Error::from)?;
    let source = read_bytes(reader)?;

    Ok(StoredDocument {
        external_id,
        internal_id,
        source: Arc::from(source),
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

pub fn run_segment_compaction(job: SegmentCompactionJob) -> io::Result<CompletedSegmentCompaction> {
    let _permit = CompactionPermit::acquire();
    let mut throttle = Throttle::new(COMPACTION_MAX_BYTES_PER_SEC);

    let tmp_path = job.store_dir.join(format!(
        "{}.tmp",
        BinaryDocumentStore::segment_filename(job.next_segment_id)
    ));
    let file = File::create(&tmp_path).map_err(|e| with_path(e, &tmp_path))?;
    let mut writer = CountingWriter::new(BufWriter::with_capacity(COMPACTION_IO_BUFFER, file), 0);
    writer.write_all(COMPACTED_MAGIC)?;

    let mut new_locations = Vec::with_capacity(job.locations_snapshot.len());
    let mut record: Vec<u8> = Vec::new();
    let mut source: Option<(u32, BufReader<File>, u64)> = None;

    for (external_id, location) in &job.locations_snapshot {
        let reuse = matches!(&source, Some((segment, _, _)) if *segment == location.segment);
        if !reuse {
            let seg_path = job
                .store_dir
                .join(BinaryDocumentStore::segment_filename(location.segment));
            let file = File::open(&seg_path).map_err(|e| with_path(e, &seg_path))?;
            source = Some((
                location.segment,
                BufReader::with_capacity(COMPACTION_IO_BUFFER, file),
                0,
            ));
        }
        let (_, reader, position) = source.as_mut().expect("source opened above");

        if *position != location.offset {
            reader.seek_relative(location.offset as i64 - *position as i64)?;
            *position = location.offset;
        }
        record.resize(location.len as usize, 0);
        reader.read_exact(&mut record)?;
        *position += location.len as u64;

        let id_bytes = external_id.as_bytes();
        let stored_id_len = record
            .get(0..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        if stored_id_len != Some(id_bytes.len())
            || record.get(4..4 + id_bytes.len()) != Some(id_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "segment {} offset {}: record does not belong to '{}'",
                    location.segment, location.offset, external_id
                ),
            ));
        }

        write_u8(&mut writer, OP_PUT)?;
        let offset = writer.position();
        writer.write_all(&record)?;
        throttle.consume(location.len as u64 + 1);

        new_locations.push((
            external_id.clone(),
            DocLocation {
                internal_id: location.internal_id,
                offset,
                segment: job.next_segment_id,
                len: location.len,
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
//ShardDb
#[timed(disk_io)]
pub fn read_document_at_path(
    dir: &Path,
    segment_id: u32,
    offset: u64,
) -> io::Result<StoredDocument> {
    let seg_path = dir.join(format!("seg_{segment_id:05}.bin"));
    let mut file = File::open(seg_path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    read_document(&mut reader)
}
#[derive(bincode::Encode, bincode::Decode)]
struct MapsFile {
    /// Every segment's length when the maps were saved: anything past it was written later.
    segment_ends: Vec<(u32, u64)>,
    entries: Vec<(String, DocLocation)>,
}
/// Saves the id → location maps with each segment's current length. Call it only while no
/// document is being written, so the lengths and the maps describe the same moment.
#[timed(database_lifecycle)]
pub fn save_maps(path: &Path, locations: &DashMap<String, DocLocation>) -> io::Result<()> {
    let mut segment_ends = Vec::new();
    for id in BinaryDocumentStore::list_segment_ids(path)? {
        let seg_path = path.join(BinaryDocumentStore::segment_filename(id));
        segment_ends.push((id, std::fs::metadata(&seg_path).map_err(|e| with_path(e, &seg_path))?.len()));
    }
    let file_contents = MapsFile {
        segment_ends,
        entries: locations.iter().map(|e| (e.key().clone(), *e.value())).collect(),
    };
    let mut bytes = MAPS_MAGIC.to_vec();
    bincode::encode_into_std_write(&file_contents, &mut bytes, bincode::config::standard())
        .map_err(|e| io::Error::other(format!("failed to encode maps: {e}")))?;

    let map_tmp = path.with_extension(MAPS_TMP_EXTENTION);
    let map_dst = path.with_extension(MAPS_EXTENTION);
    let mut file = File::create(&map_tmp).map_err(|e| with_path(e, &map_tmp))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&map_tmp, &map_dst).map_err(|e| with_path(e, &map_dst))?;
    if let Some(parent) = map_dst.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_maps(path: &Path) -> Option<MapsFile> {
    let bytes = std::fs::read(path.with_extension(MAPS_EXTENTION)).ok()?;
    let body = bytes.strip_prefix(MAPS_MAGIC.as_slice())?;
    bincode::decode_from_slice(body, bincode::config::standard()).ok().map(|(maps, _)| maps)
}

fn segment_magic(seg_path: &Path) -> io::Result<[u8; 8]> {
    let mut magic = [0u8; 8];
    File::open(seg_path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| with_path(e, seg_path))?;
    Ok(magic)
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
