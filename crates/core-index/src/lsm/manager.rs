use std::{
    collections::VecDeque,
    io,
    path::{ Path, PathBuf },
    sync::Arc,
    time::{ Duration, Instant },
};

use core_timing::timed;
use roaring::RoaringTreemap;

use crate::{
    disk::{ reader::DiskSegment, writer::write_segment }, fuzzy::{ FuzzyExpansion, FuzzyOptions }, lsm::{
        IndexSnapshot,
        compaction::{ CompactionConfig, CompactionJob, CompletedCompaction },
        manifest,
    }, mem::MemIndex, numeric_values::{ NumericBound, NumericValue }, posting::{ DeleteSet, PostingList, cursor::{TermCursor, UnionCursor} }, search::{ SearchIndex, SearchNumeric, SearchReader, SearchStats, TermCursorInfo }, segment::{ ImmutableSegment, SegmentHandle }, types::{ ARRAY_ROW_FLAG, ArrayRowId, DocId, XPathId }, vector::{ HnswGraph, SegmentVectors, VectorFile, find_vector_files, hnsw_file_path, vector_file_path }, wildcard::WildcardPattern,
};

pub struct LsmIndex {
    mem: MemIndex,
    generations: Vec<Arc<MemIndex>>,
    segment_handles: Vec<SegmentHandle>,
    query_segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
    flush_threshold: usize,
    //flush queue
    flush_queue: FlushQueue,
    deleted: Arc<DeleteSet>,
    delete_generation: u64,
    root: Option<PathBuf>,
    next_segment_id: u64,
    next_doc_id: DocId,
    next_compaction_job_id: u64,
    max_array_row: ArrayRowId,
    last_ingest: Option<Instant>,
    vectors: Arc<Vec<SegmentVectors>>,
    lost_segments: Vec<PathBuf>,
    removed_leftovers: Vec<PathBuf>,
}

pub struct FlushJob {
    seq: u64,
    path: PathBuf,
    generations: Arc<[Arc<MemIndex>]>,
}

impl FlushJob {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    #[timed(writing_files)]
    pub fn write(self) -> io::Result<DiskSegment> {
        let with_path = |e: io::Error|
            io::Error::new(e.kind(), format!("{}: {e}", self.path.display()));
        let segment = MemIndex::freeze_shared(&self.generations);
        write_segment(&self.path, &segment).map_err(with_path)?;
        DiskSegment::open(&self.path).map_err(with_path)
    }
}
/// Below this a generation is cheap to copy, so it is merged even while a snapshot reads it.
const SMALL_GENERATION_BYTES: usize = 1 << 20;
struct PendingFlush {
    seq: u64,
    path: PathBuf,
    generations: Arc<[Arc<MemIndex>]>,
    /// segments added (`add_immutable_segment`) since this flush was queued: they hold newer
    /// documents, so it installs before them
    added_after: usize,
}
#[derive(Default)]
struct FlushQueue {
    made_not_flushed: Vec<FlushJob>,
    pending: VecDeque<PendingFlush>,
    next_seq: u64,
    failure: Option<String>,
}
impl FlushQueue {
    fn push(&mut self, path: PathBuf, generations: Arc<[Arc<MemIndex>]>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(PendingFlush {
            seq,
            path: path.clone(),
            generations: Arc::clone(&generations),
            added_after: 0,
        });
        self.made_not_flushed.push(FlushJob {
            seq,
            path,
            generations,
        });
    }

    /// A segment was added after every flush still pending.
    fn segment_added(&mut self) {
        for pending in &mut self.pending {
            pending.added_after += 1;
        }
    }

    fn take_jobs(&mut self) -> Vec<FlushJob> {
        std::mem::take(&mut self.made_not_flushed)
    }

    /// Removes the oldest pending flush; `seq` must be it.
    fn complete(&mut self, seq: u64) -> io::Result<PendingFlush> {
        match self.pending.front() {
            Some(front) if front.seq == seq => Ok(self.pending.pop_front().expect("front checked")),
            _ => Err(io::Error::other(format!("segment write {seq} finished out of order"))),
        }
    }

    fn flushed_through(&self) -> u64 {
        self.next_seq - (self.pending.len() as u64)
    }

    fn writes_in_flight(&self) -> bool {
        self.pending.len() > self.made_not_flushed.len()
    }

    fn fail(&mut self, error: &io::Error) {
        self.failure.get_or_insert_with(|| error.to_string());
    }

    fn health(&self) -> io::Result<()> {
        match &self.failure {
            Some(message) =>
                Err(io::Error::other(format!("segment write failed earlier: {message}"))),
            None => Ok(()),
        }
    }
}

// #[timed(flushing)]
// fn unwrap_mem(generation: Arc<MemIndex>) -> MemIndex {
//     Arc::try_unwrap(generation).unwrap_or_else(|shared| (*shared).clone())
// }
pub trait FlushCheckpoint: Send + Sync {
    fn checkpoint(&self) -> io::Result<()>;
}
impl SearchIndex for LsmIndex {
    #[timed(search)]
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.snapshot().lookup(term, xpath)
    }

    fn terms(&self, xpath: XPathId) -> Vec<String> {
        self.snapshot().terms(xpath)
    }
    fn prefix_terms(&self, prefix: &str, xpath: XPathId) -> Vec<(String, u32)> {
        self.snapshot().prefix_terms(prefix, xpath)
    }

    #[timed(search)]
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.snapshot().lookup_prefix(prefix, xpath)
    }

    fn resolve_array_rows(&self, rows: &RoaringTreemap) -> RoaringTreemap {
        self.snapshot().resolve_array_rows(rows)
    }

    fn element_index_of_row(&self, row: ArrayRowId) -> Option<u32> {
        self.snapshot().element_index_of_row(row)
    }

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId> {
        self.snapshot().parent_of_row(row)
    }

    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId> {
        self.snapshot().doc_of_row(row)
    }

    fn bool_true_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.snapshot().bool_true_ids(xpath)
    }
    fn bool_false_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.snapshot().bool_false_ids(xpath)
    }
    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool> {
        self.snapshot().bool_value(xpath, doc_id)
    }

    #[timed(search)]
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        self.snapshot().lookup_wildcard(pattern, xpath)
    }

    #[timed(search)]
    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        self.snapshot().lookup_fuzzy(term, xpath, opts)
    }
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        self.snapshot().doc_freq(term, xpath)
    }

    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions
    ) -> Vec<FuzzyExpansion> {
        self.snapshot().fuzzy_expansions(term, xpath, opts)
    }
        #[timed(search)]
    fn term_cursor(&self, term: &str, xpath: XPathId) -> Option<TermCursorInfo<'_>> {
        let pending = self.flush_queue.pending.iter().flat_map(|flush| flush.generations.iter());

        // the same sources as `build_snapshot`, borrowed from `self` instead of cloned
        let memory = std::iter::once(&self.mem)
            .chain(self.generations.iter().map(|generation| &**generation))
            .chain(pending.map(|generation| &**generation));

        let mut children = Vec::new();
        let mut doc_freq = 0u32;
        let mut max_weight = 0u16;

        let infos = memory
            .filter_map(|mem| mem.term_cursor(term, xpath))
            .chain(self.query_segments.iter().filter_map(|segment| segment.term_cursor(term, xpath)));

        for info in infos {
            doc_freq = doc_freq.saturating_add(info.doc_freq);
            max_weight = max_weight.max(info.max_weight);
            children.push(info.cursor);
        }

        let cursor = if children.is_empty() {
            return None;
        } else if children.len() == 1 && self.deleted.is_empty() {
            children.pop()?
        } else {
            TermCursor::Union(Box::new(UnionCursor::new(children, &self.deleted)))
        };

        Some(TermCursorInfo { cursor, doc_freq, max_weight })
    }
}

impl SearchNumeric for LsmIndex {
    #[timed(search)]
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>
    ) -> RoaringTreemap {
        self.snapshot().numeric_range(xpath, lo, hi)
    }

    #[timed(search)]
    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.snapshot().numeric_value(xpath, doc_id)
    }

    #[timed(search)]
    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        self.snapshot().numeric_bounds(xpath)
    }
}

impl SearchStats for LsmIndex {
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.snapshot().doc_len(doc_id, xpath)
    }

    fn doc_count(&self, xpath: XPathId) -> u64 {
        self.snapshot().doc_count(xpath)
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        self.snapshot().total_doc_len(xpath)
    }
    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.snapshot().doc_range()
    }
}

impl LsmIndex {
    pub fn new(flush_threshold: usize) -> Self {
        Self {
            mem: MemIndex::new(),
            generations: Vec::new(),

            segment_handles: Vec::new(),
            query_segments: Arc::new(Vec::new()),
            flush_threshold,
            flush_queue: FlushQueue::default(),
            deleted: Arc::new(DeleteSet::new()),
            delete_generation: 0,
            root: None,
            next_doc_id: 0,
            next_segment_id: 0,
            next_compaction_job_id: 0,
            max_array_row: ArrayRowId::from_raw(0),
            last_ingest: None,
            vectors: Arc::new(Vec::new()),
            lost_segments: Vec::new(),
            removed_leftovers: Vec::new(),
        }
    }

    #[timed(database_lifecycle)]
    pub fn persistent(root: impl Into<PathBuf>, flush_threshold: usize) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;

        let segment_paths = manifest::read_manifest(&root)?;
        let removed_leftovers = manifest::remove_leftovers(&root, &segment_paths)?;
        let mut next_doc_id = 0;
        let mut segment_handles = Vec::new();
        let mut query_segments: Vec<Arc<dyn SearchReader + Send + Sync>> = Vec::new();
        let mut next_segment_id = 0;
        let mut max_array_row = ArrayRowId(0);
        let mut lost_segments = Vec::new();
        let deleted = crate::lsm::deletes::read_deletes(&root)?;
        // let mut deleted_generation=
        let mut vectors: Vec<SegmentVectors> = Vec::new();
        for path in segment_paths {
            let disk = match DiskSegment::open(&path) {
                Ok(disk) => disk,
                // derived data: start without it, a reindex rebuilds it from the store
                Err(e) if
                    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData)
                => {
                    // keep its number reserved so a new segment never reuses the name
                    if
                        let Some(id) = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .and_then(manifest::segment_id_of)
                    {
                        next_segment_id = next_segment_id.max(id + 1);
                    }
                    lost_segments.push(path);
                    continue;
                }
                Err(e) => {
                    return Err(e);
                }
            };
            if let Some((_, max)) = disk.doc_range() {
                next_doc_id = next_doc_id.max(max + 1);
            }

            max_array_row = max_array_row.max(
                disk.array_row_index().max_array_row().unwrap_or(ArrayRowId(0))
            );

            if
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) &&
                let Some(id) = stem
                    .strip_prefix("segment-")
                    .and_then(|value| value.parse::<u64>().ok())
            {
                next_segment_id = next_segment_id.max(id + 1);
            }
            for (xpath, vector_path) in find_vector_files(&path)? {
                match VectorFile::open(&vector_path) {
                    Ok(file) => {
                        let file = Arc::new(file);
                        let graph = open_graph(&path, xpath, &file);
                        vectors.push(SegmentVectors {
                            segment: path.clone(),
                            xpath,
                            file,
                            graph,
                        });
                    }
                    // Derived data: drop it and let the embedding worker rebuild it.
                    Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                        let _ = std::fs::remove_file(&vector_path);
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }

            segment_handles.push(SegmentHandle::Disk(path));
            // Non primitive cast alaallala
            let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
            query_segments.push(disk);
        }

        Ok(Self {
            mem: MemIndex::new(),
            generations: Vec::new(),
            segment_handles,
            query_segments: Arc::new(query_segments),
            flush_threshold,
            flush_queue: FlushQueue::default(),
            deleted: Arc::new(deleted),
            delete_generation: 0, //incorrect
            root: Some(root),
            next_segment_id,
            next_doc_id,
            next_compaction_job_id: 0,
            max_array_row,
            last_ingest: None,
            vectors: Arc::new(vectors),
            lost_segments,
            removed_leftovers,
        })
    }

    pub fn max_array_row(&self) -> ArrayRowId {
        self.max_array_row
    }

    /// Highest document id this index still holds anything for: in a segment, or marked
    /// deleted. A new document must get a higher id, or it would inherit those traces.
    pub fn highest_known_doc_id(&self) -> DocId {
        let in_segments = self.next_doc_id.saturating_sub(1);
        let deleted = self.deleted
            .iter()
            .filter(|&doc_id| (doc_id & ARRAY_ROW_FLAG) == 0)
            .max()
            .unwrap_or(0);
        in_segments.max(deleted)
    }

    #[timed(indexing_documents)]
    fn seal(&mut self) {
        if self.mem.term_count() == 0 && self.mem.doc_id_range().is_none() {
            return;
        }
        let mut sealed = std::mem::take(&mut self.mem);
        while let Some(newest) = self.generations.last() {
            if newest.estimated_size_bytes() > sealed.estimated_size_bytes() {
                break;
            }

            if
                Arc::strong_count(newest) > 1 &&
                newest.estimated_size_bytes() > SMALL_GENERATION_BYTES
            {
                break;
            }
            let Some(newest) = self.generations.pop() else {
                break;
            };
            let mut merged = Arc::try_unwrap(newest).unwrap_or_else(|shared| (*shared).clone());
            merged.merge_from(sealed);
            sealed = merged;
        }
        self.generations.push(Arc::new(sealed));
    }

    pub(crate) fn should_flush(&self) -> bool {
        self.memtable_size() >= self.flush_threshold
    }
    pub fn pending_flushes(&self) -> usize {
        self.flush_queue.pending.len()
    }

    pub fn publish_snapshot(&mut self) -> IndexSnapshot {
        self.seal();
        self.build_snapshot(Arc::new(MemIndex::new()))
    }

    /// Read-only callers only; copies `active`, keep off hot paths.
    pub fn snapshot(&self) -> IndexSnapshot {
        self.build_snapshot(Arc::new(self.mem.clone()))
    }

    fn build_snapshot(&self, mem: Arc<MemIndex>) -> IndexSnapshot {
        let pending: usize = self.flush_queue.pending
            .iter()
            .map(|pending| pending.generations.len())
            .sum();
        let mut segments: Vec<Arc<dyn SearchReader + Send + Sync>> = Vec::with_capacity(
            self.generations.len() + pending + self.query_segments.len()
        );
        segments.extend(
            self.generations
                .iter()
                .map(|generation| Arc::clone(generation) as Arc<dyn SearchReader + Send + Sync>)
        );
        for flush in &self.flush_queue.pending {
            segments.extend(
                flush.generations
                    .iter()
                    .map(|generation| {
                        Arc::clone(generation) as Arc<dyn SearchReader + Send + Sync>
                    })
            );
        }
        segments.extend(self.query_segments.iter().cloned());
        IndexSnapshot::new(mem, Arc::new(segments), Arc::clone(&self.deleted)).with_vectors(
            Arc::clone(&self.vectors)
        )
    }

    pub fn merge_into_memtable(&mut self, other: MemIndex) {
        self.last_ingest = Some(Instant::now());
        self.mem.merge_from(other);
    }

    pub fn memtable_size(&self) -> usize {
        self.mem.estimated_size_bytes() +
            self.generations
                .iter()
                .map(|generation| generation.estimated_size_bytes())
                .sum::<usize>()
    }
    /////////////////////////////////FLUSH/////////////////////
    pub fn flush_threshold(&self) -> usize {
        self.flush_threshold
    }
    /// Seals the memtable and queues its generations to be written to disk

    #[timed(flushing)]
    pub fn queue_flush(&mut self) -> io::Result<()> {
        self.flush_queue.health()?;
        self.seal();
        if let Some(root) = &self.root {
            crate::lsm::deletes::sync_deletes(root)?;
        }
        if self.generations.is_empty() {
            return Ok(());
        }

        // oldest first: index 0 holds the lowest doc ids
        let generations: Arc<[Arc<MemIndex>]> = std::mem::take(&mut self.generations).into();
        let max_doc_id = generations
            .iter()
            .filter_map(|generation| generation.doc_id_range())
            .map(|(_, max)| max)
            .max()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "flushed generations contain no document ids"
                )
            })?;
        self.next_doc_id = max_doc_id.saturating_add(1);

        if let Some(root) = &self.root {
            let path = root.join(manifest::segment_file_name(self.next_segment_id));
            self.next_segment_id += 1;
            self.flush_queue.push(path, generations);
        } else {
            let segment = Arc::new(MemIndex::freeze_shared(&generations));
            self.segment_handles.push(SegmentHandle::Memory(Arc::clone(&segment)));
            Arc::make_mut(&mut self.query_segments).push(
                segment as Arc<dyn SearchReader + Send + Sync>
            );
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.flush_queue.writes_in_flight() {
            return Err(io::Error::other("flush() called while segment writes are in flight"));
        }
        self.queue_flush()?;
        for job in self.flush_queue.take_jobs() {
            let seq = job.seq();
            let written = job.write();
            self.complete_flush(seq, written)?;
        }
        Ok(())
    }

    pub fn take_flush_jobs(&mut self) -> Vec<FlushJob> {
        self.flush_queue.take_jobs()
    }

    pub fn complete_flush(&mut self, seq: u64, written: io::Result<DiskSegment>) -> io::Result<()> {
        let pending = match self.flush_queue.complete(seq) {
            Ok(pending) => pending,
            Err(e) => {
                self.flush_queue.fail(&e);
                return Err(e);
            }
        };
        let outcome = written.and_then(|disk| self.install_flushed(&pending, disk));
        if let Err(e) = &outcome {
            self.keep_in_memory(&pending);
            self.flush_queue.fail(e);
        }
        drop_flushed_generations(pending.generations);
        outcome
    }

    /// Where a pending flush goes among the segments: before the ones added after it was
    /// queued, after everything older.
    fn flush_position(&self, pending: &PendingFlush) -> usize {
        self.segment_handles.len().saturating_sub(pending.added_after)
    }

    fn install_flushed(&mut self, pending: &PendingFlush, disk: DiskSegment) -> io::Result<()> {
        let root = self.root
            .clone()
            .ok_or_else(|| io::Error::other("flushed segment without a root"))?;
        manifest::append_segment(&root, &pending.path)?;
        let position = self.flush_position(pending);
        self.segment_handles.insert(position, SegmentHandle::Disk(pending.path.clone()));
        Arc::make_mut(&mut self.query_segments).insert(position, Arc::new(disk));
        Ok(())
    }

    /// A flush that could not be written stays searchable from memory, where it would have
    /// been installed.
    fn keep_in_memory(&mut self, pending: &PendingFlush) {
        let segment = Arc::new(MemIndex::freeze_shared(&pending.generations));
        let position = self.flush_position(pending);
        self.segment_handles.insert(position, SegmentHandle::Memory(Arc::clone(&segment)));
        Arc::make_mut(&mut self.query_segments).insert(
            position,
            segment as Arc<dyn SearchReader + Send + Sync>
        );
    }

    /// Every flush numbered below this has to finish before data queued so far is on disk.
    pub fn flush_target(&self) -> u64 {
        self.flush_queue.next_seq
    }

    pub fn flushed_through(&self) -> u64 {
        self.flush_queue.flushed_through()
    }

    pub fn has_pending_flushes(&self) -> bool {
        !self.flush_queue.pending.is_empty()
    }

    /// Err once any segment write has failed: from then on nothing is durably flushed.
    pub fn flush_health(&self) -> io::Result<()> {
        self.flush_queue.health()
    }

    #[timed(indexing_documents)]
    pub fn add_immutable_segment(&mut self, segments: Vec<ImmutableSegment>) -> io::Result<()> {
        self.last_ingest = Some(Instant::now());
        for segment in segments {
            let segment = Arc::new(segment);
            let reader: Arc<dyn SearchReader + Send + Sync> = if let Some(root) = &self.root {
                let path = root.join(manifest::segment_file_name(self.next_segment_id));
                self.next_segment_id += 1;

                write_segment(&path, &segment)?;
                let disk = DiskSegment::open(&path)?;
                manifest::append_segment(root, &path)?;
                self.segment_handles.push(SegmentHandle::Disk(path));
                Arc::new(disk)
            } else {
                self.segment_handles.push(SegmentHandle::Memory(segment.clone()));
                segment as Arc<dyn SearchReader + Send + Sync>
            };
            Arc::make_mut(&mut self.query_segments).push(reader);
            self.flush_queue.segment_added();
        }

        Ok(())
    }
    pub fn segment_count(&self) -> usize {
        self.segment_handles.len()
    }
    pub fn lost_segments(&self) -> &[PathBuf] {
        &self.lost_segments
    }

    /// Files removed at startup as crash leftovers.
    pub fn removed_leftovers(&self) -> &[PathBuf] {
        &self.removed_leftovers
    }
    #[timed(compaction)]
    fn segment_size_bytes(handle: &SegmentHandle) -> u64 {
        match handle {
            SegmentHandle::Disk(path) => std::fs::metadata(path).map_or(0, |m| m.len()),
            //migh need a smarter way but still this is ok for aproximating the segment size
            SegmentHandle::Memory(segment) => segment.terms().len() as u64,
        }
    }

    #[timed(compaction)]
    pub fn plan_compaction(
        &mut self,
        config: CompactionConfig
    ) -> io::Result<Option<CompactionJob>> {
        let Some(root) = &self.root else {
            return Ok(None);
        };

        let sizes = self.segment_sizes();
        let disk_segments = sizes
            .iter()
            .filter(|size| size.is_some())
            .count();
        if disk_segments < 2 {
            return Ok(None);
        }

        // Documents stopped arriving: the load the segments came from is over.
        let quiet = self.quiet_for(config.idle_merge_after);

        if !quiet && (self.has_pending_flushes() || sizes.len() <= config.max_segments) {
            return Ok(None);
        }

        let all_disk = disk_segments == sizes.len();
        let max_width = config.max_segments_per_compaction.max(config.min_width);

        let window = if quiet && all_disk {
            if Self::merge_all_due(&sizes, config.size_ratio) {
                // After a load: one segment per shard, before embedding reads it.
                Some((0, sizes.len()))
            } else {
                // A few small segments beside a large one: merge them among themselves and
                // leave the large one, which would otherwise be rewritten for little data.
                Self::tiered_window(&sizes, config.size_ratio, 2, max_width)
            }
        } else if self.segment_count() >= config.compact_when_segments_at_least {
            Self::tiered_window(&sizes, config.size_ratio, config.min_width, max_width).or_else(|| {
                (self.segment_count() > config.max_segments)
                    .then(|| Self::cheapest_window(&sizes, max_width))
                    .flatten()
            })
        } else {
            None
        };

        let Some((start, end)) = window else {
            return Ok(None);
        };

        let selected: Vec<SegmentHandle> = self.segment_handles[start..end].to_vec();
        let vectors: Vec<SegmentVectors> = self.vectors
            .iter()
            .filter(|entry| {
                selected
                    .iter()
                    .any(|handle| matches!(handle, SegmentHandle::Disk(p) if *p == entry.segment))
            })
            .cloned()
            .collect();
        let output_path = root.join(format!("segment-{}.idx", self.next_segment_id));
        self.next_segment_id += 1;
        let job_id = self.next_compaction_job_id;
        self.next_compaction_job_id += 1;

        Ok(
            Some(CompactionJob {
                job_id,
                selected,
                deleted: Arc::clone(&self.deleted),
                delete_generation: self.delete_generation,
                output_path,
                vectors,
            })
        )
    }

    fn merge_all_due(sizes: &[Option<u64>], ratio: u64) -> bool {
        let disk: Vec<u64> = sizes.iter().flatten().copied().collect();
        if disk.len() < 2 {
            return false;
        }
        let largest = disk.iter().copied().max().unwrap_or(0);
        let rest = disk.iter().sum::<u64>() - largest;
        rest.saturating_mul(ratio.max(1)) >= largest
    }

    pub fn settling(&self, config: &CompactionConfig) -> bool {
        let in_memory =
            self.root.is_some() &&
            (self.mem.doc_id_range().is_some() ||
                !self.generations.is_empty() ||
                self.segment_handles
                    .iter()
                    .any(|handle| matches!(handle, SegmentHandle::Memory(_))));
        !self.quiet_for(config.idle_merge_after) ||
            in_memory ||
            Self::merge_all_due(&self.segment_sizes(), config.size_ratio)
    }

    /// No document was added or deleted for `period` (or since the index opened).
    fn quiet_for(&self, period: Duration) -> bool {
        self.last_ingest.is_none_or(|at| at.elapsed() >= period)
    }

    /// Each segment's size in bytes, in segment order; None for one still in memory.
    fn segment_sizes(&self) -> Vec<Option<u64>> {
        self.segment_handles
            .iter()
            .map(|handle| {
                match handle {
                    SegmentHandle::Disk(_) => Some(Self::segment_size_bytes(handle).max(1)),
                    SegmentHandle::Memory(_) => None,
                }
            })
            .collect()
    }

    /// Cheapest contiguous run of disk segments whose sizes are within
    /// `COMPACTION_SIZE_RATIO` of each other, at least `min_width` long.
    fn tiered_window(
        sizes: &[Option<u64>],
        ratio: u64,
        min_width: usize,
        max_width: usize
    ) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, u64)> = None;

        for start in 0..sizes.len() {
            let Some(first) = sizes[start] else {
                continue;
            };
            let (mut smallest, mut largest, mut total) = (first, first, first);
            let mut end = start + 1;

            while end < sizes.len() && end - start < max_width {
                let Some(size) = sizes[end] else {
                    break;
                };
                let next_smallest = smallest.min(size);
                let next_largest = largest.max(size);
                if next_largest > next_smallest.saturating_mul(ratio) {
                    break;
                }
                smallest = next_smallest;
                largest = next_largest;
                total += size;
                end += 1;
            }

            if end - start >= min_width && best.is_none_or(|(_, _, best_total)| total < best_total) {
                best = Some((start, end, total));
            }
        }

        best.map(|(start, end, _)| (start, end))
    }

    /// Cheapest contiguous run of up to `width` disk segments, ignoring size ratios.
    /// Used to keep the segment count bounded when no tiered run qualifies.
    fn cheapest_window(sizes: &[Option<u64>], width: usize) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, u64)> = None;

        for start in 0..sizes.len() {
            let mut total = 0u64;
            let mut end = start;
            while end < sizes.len() && end - start < width {
                let Some(size) = sizes[end] else {
                    break;
                };
                total += size;
                end += 1;
            }
            if end - start >= 2 && best.is_none_or(|(_, _, best_total)| total < best_total) {
                best = Some((start, end, total));
            }
        }

        best.map(|(start, end, _)| (start, end))
    }
    #[timed(compaction)]
    pub fn install_compaction(&mut self, completed: CompletedCompaction) -> io::Result<bool> {
        let Some(root) = self.root.clone() else {
            return Ok(false);
        };

        // Locate the selected segments wherever they are in the live list.
        let mut positions: Vec<usize> = completed.selected
            .iter()
            .filter_map(|handle| self.segment_handles.iter().position(|live| live == handle))
            .collect();

        if positions.len() != completed.selected.len() {
            // Stale job — some input was already merged away. Drop the output.
            std::fs::remove_file(&completed.output_path).ok();
            for xpath in &completed.vector_xpaths {
                std::fs::remove_file(vector_file_path(&completed.output_path, *xpath)).ok();
            }
            return Ok(false);
        }

        // If every live segment was part of this merge, the new segment is
        // delete-free, so tombstones can be dropped (same as compact_all did).
        let merged_all = positions.len() == self.segment_handles.len();

        let disk = DiskSegment::open(&completed.output_path)?;
        let segs = Arc::make_mut(&mut self.query_segments);
        positions.sort_unstable();
        positions.dedup();
        let insert_pos = positions[0];
        for pos in positions.into_iter().rev() {
            self.segment_handles.remove(pos);
            segs.remove(pos);
        }

        self.segment_handles.insert(insert_pos, SegmentHandle::Disk(completed.output_path.clone()));
        let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
        segs.insert(insert_pos, disk);
        manifest::compact_manifest(&root, &self.disk_segment_paths())?;

        for handle in completed.selected {
            if let SegmentHandle::Disk(path) = handle {
                self.drop_vectors_for(&path);
                std::fs::remove_file(path).ok();
            }
        }

        for xpath in completed.vector_xpaths {
            if self.attach_vectors(&completed.output_path, xpath).is_err() {
                // Unreadable output only loses vectors; the embedding worker rebuilds them.
                std::fs::remove_file(vector_file_path(&completed.output_path, xpath)).ok();
            }
        }

        if merged_all && completed.delete_generation == self.delete_generation {
            self.deleted = Arc::new(DeleteSet::new());
            crate::lsm::deletes::clear_deletes(&root)?;
        }

        Ok(true)
    }

    #[timed(modifying_documents)]
    pub fn delete_documents(&mut self, doc_ids: &[DocId]) -> io::Result<()> {
        if doc_ids.is_empty() {
            return Ok(());
        }
        if let Some(root) = &self.root {
            crate::lsm::deletes::append_deletes(root, doc_ids)?;
        }
        self.last_ingest = Some(Instant::now());
        let deleted = Arc::make_mut(&mut self.deleted);
        for &doc_id in doc_ids {
            deleted.delete(doc_id);
        }
        self.delete_generation += 1;
        Ok(())
    }

    pub fn memtable_term_count(&self) -> usize {
        self.mem.term_count()
    }

    pub fn disk_segment_paths(&self) -> Vec<PathBuf> {
        self.segment_handles
            .iter()
            .filter_map(|handle| {
                match handle {
                    SegmentHandle::Disk(path) => Some(path.clone()),
                    SegmentHandle::Memory(_) => None,
                }
            })
            .collect()
    }
    /// Attaches `<segment>.<xpath>.vec`, replacing any earlier version.
    /// Returns false (and removes the file) when the segment is no longer live.
    /// Attaches `<segment>.<xpath>.vec`, replacing any earlier version.
    /// Returns false (and removes the file) when the segment is no longer live.
    pub fn install_vectors(&mut self, segment: &Path, xpath: XPathId) -> io::Result<bool> {
        let live = self.segment_handles
            .iter()
            .any(|handle| matches!(handle, SegmentHandle::Disk(p) if p == segment));
        if !live {
            let _ = std::fs::remove_file(vector_file_path(segment, xpath));
            return Ok(false);
        }
        self.attach_vectors(segment, xpath)?;
        Ok(true)
    }

    /// Attaches `<segment>.<xpath>.hnsw` to that segment's current vector file.
    /// Returns false, removing the graph file, when the segment is no longer live or the
    /// graph was built from an older version of the vector file.
    pub fn install_graph(&mut self, segment: &Path, xpath: XPathId) -> io::Result<bool> {
        let path = hnsw_file_path(segment, xpath);
        let vectors = Arc::make_mut(&mut self.vectors);
        let Some(entry) = vectors
            .iter_mut()
            .find(|entry| entry.segment == segment && entry.xpath == xpath) else {
            let _ = std::fs::remove_file(&path);
            return Ok(false);
        };
        match HnswGraph::open(&path, Arc::clone(&entry.file)) {
            Ok(graph) => {
                entry.graph = Some(Arc::new(graph));
                Ok(true)
            }
            // derived data: a stale or corrupt graph is dropped and built again
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = std::fs::remove_file(&path);
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn attach_vectors(&mut self, segment: &Path, xpath: XPathId) -> io::Result<()> {
        let file = Arc::new(VectorFile::open(vector_file_path(segment, xpath))?);
        let vectors = Arc::make_mut(&mut self.vectors);
        match vectors.iter_mut().find(|entry| entry.segment == segment && entry.xpath == xpath) {
            Some(existing) => {
                existing.file = file;
                // the graph indexes the old file: drop it, the background worker builds a new
                // one. Removing can fail while a reader still maps it (Windows); the stale file
                // is then rejected by its file id at the next open or install.
                if existing.graph.take().is_some() {
                    let _ = std::fs::remove_file(hnsw_file_path(segment, xpath));
                }
            }
            None =>
                vectors.push(SegmentVectors {
                    segment: segment.to_path_buf(),
                    xpath,
                    file,
                    graph: None,
                }),
        }
        Ok(())
    }

    fn drop_vectors_for(&mut self, segment: &Path) {
        let vectors = Arc::make_mut(&mut self.vectors);
        vectors.retain(|entry| {
            if entry.segment == segment {
                let _ = std::fs::remove_file(entry.file.path());
                let _ = std::fs::remove_file(hnsw_file_path(segment, entry.xpath));
                false
            } else {
                true
            }
        });
    }
}
/// Opens the graph next to a vector file at load. A missing graph is normal (small segment,
/// or not built yet); a stale or corrupt one is deleted so it gets rebuilt.
fn open_graph(segment: &Path, xpath: XPathId, file: &Arc<VectorFile>) -> Option<Arc<HnswGraph>> {
    let path = hnsw_file_path(segment, xpath);
    match HnswGraph::open(&path, Arc::clone(file)) {
        Ok(graph) => Some(Arc::new(graph)),
        Err(e) => {
            if e.kind() == io::ErrorKind::InvalidData {
                let _ = std::fs::remove_file(&path);
            }
            None
        }
    }
}
#[timed(flushing)]
fn drop_flushed_generations(generations: Arc<[Arc<MemIndex>]>) {
    drop(generations);
}
