use std::{
    collections::VecDeque,
    io,
    path::{ Path, PathBuf },
    sync::Arc,
    time::{ Duration, Instant },
};

use ahash::HashSet;
use core_timing::timed;
use roaring::RoaringTreemap;

use crate::{
    disk::{ reader::DiskSegment, writer::write_segment }, fuzzy::{ FuzzyExpansion, FuzzyOptions }, lsm::{
        IndexSnapshot,
        compaction::{ CompactionConfig, CompactionJob, CompletedCompaction },
        manifest,
    }, mem::MemIndex, numeric_values::{ NumericBound, NumericValue }, posting::{ DeleteSet, PostingList }, search::{ SearchIndex, SearchNumeric, SearchReader, SearchStats }, segment::{ ImmutableSegment, SegmentHandle }, types::{ ArrayRowId, DocId, XPathId }, vector::{ HnswGraph, SegmentVectors, VectorFile, find_vector_files, hnsw_file_path, vector_file_path }, wildcard::WildcardPattern,
};

// Live index of data, this will be flushed in other words put into a persistent
// memtable, snapshoting, deleting
pub struct LsmIndex {
    mem: MemIndex,
    generations: Vec<Arc<MemIndex>>,
    segment_handles: Vec<SegmentHandle>,
    query_segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
    flush_threshold: usize,
    //flush queue
    flush_queue: FlushQueue,
    deleted: DeleteSet,
    delete_generation: u64,
    root: Option<PathBuf>,
    next_segment_id: u64,
    next_doc_id: DocId,
    next_compaction_job_id: u64,
    max_array_row: ArrayRowId,
    /// When documents were last added or deleted; None since the index was opened.
    last_ingest: Option<Instant>,
    vectors: Arc<Vec<SegmentVectors>>,
}

pub struct FlushJob {
    seq: u64,
    path: PathBuf,
    segment: Arc<ImmutableSegment>,
}

impl FlushJob {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Writes and fsyncs the segment file, then opens it for reading. Touches no index state,
    /// so it can run on any thread.
    #[timed(writing_files)]
    pub fn write(self) -> io::Result<DiskSegment> {
        let with_path = |e: io::Error|
            io::Error::new(e.kind(), format!("{}: {e}", self.path.display()));
        write_segment(&self.path, &self.segment).map_err(with_path)?;
        DiskSegment::open(&self.path).map_err(with_path)
    }
}

/// A queued segment that is not yet installed from disk.
struct PendingFlush {
    seq: u64,
    path: PathBuf,
    segment: Arc<ImmutableSegment>,
}
#[derive(Default)]
struct FlushQueue {
    made_not_flushed: Vec<FlushJob>,
    pending: VecDeque<PendingFlush>,
    next_seq: u64,
    failure: Option<String>,
}
impl FlushQueue {
    fn push(&mut self, path: PathBuf, segment: Arc<ImmutableSegment>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push_back(PendingFlush {
            seq,
            path: path.clone(),
            segment: Arc::clone(&segment),
        });
        self.made_not_flushed.push(FlushJob { seq, path, segment });
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

    /// Every flush numbered below this has finished, successfully or not.
    fn flushed_through(&self) -> u64 {
        self.next_seq - (self.pending.len() as u64)
    }

    /// Whether some queued jobs were already handed to a writer and are still running.
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

#[timed(flushing)]
fn unwrap_mem(generation: Arc<MemIndex>) -> MemIndex {
    Arc::try_unwrap(generation).unwrap_or_else(|shared| (*shared).clone())
}
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

    #[timed(search)]
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.snapshot().lookup_prefix(prefix, xpath)
    }

    fn resolve_array_rows(&self, rows: &HashSet<ArrayRowId>) -> HashSet<DocId> {
        self.snapshot().resolve_array_rows(rows)
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
}

impl SearchNumeric for LsmIndex {
    #[timed(search)]
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>
    ) -> PostingList {
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
            deleted: DeleteSet::new(),
            delete_generation: 0,
            root: None,
            next_doc_id: 0,
            next_segment_id: 0,
            next_compaction_job_id: 0,
            max_array_row: 0,
            last_ingest: None,
            vectors: Arc::new(Vec::new()),
        }
    }

    #[timed(database_lifecycle)]
    pub fn persistent(root: impl Into<PathBuf>, flush_threshold: usize) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;

        let segment_paths = manifest::read_manifest(&root)?;
        let mut next_doc_id = 0;
        let mut segment_handles = Vec::new();
        let mut query_segments: Vec<Arc<dyn SearchReader + Send + Sync>> = Vec::new();
        let mut next_segment_id = 0;
        let mut max_array_row = 0;
        let deleted = crate::lsm::deletes::read_deletes(&root)?;
        // let mut deleted_generation=
        let mut vectors: Vec<SegmentVectors> = Vec::new();
        for path in segment_paths {
            let disk = DiskSegment::open(&path)?;
            if let Some((_, max)) = disk.doc_range() {
                next_doc_id = next_doc_id.max(max + 1);
            }

            max_array_row = max_array_row.max(disk.array_row_index().max_array_row().unwrap_or(0));

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

            segment_handles.push(SegmentHandle::Disk(path.into()));
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
            deleted,
            delete_generation: 0, //incorrect
            root: Some(root),
            next_segment_id,
            next_doc_id,
            next_compaction_job_id: 0,
            max_array_row,
            last_ingest: None,
            vectors: Arc::new(vectors),
        })
    }

    pub fn max_array_row(&self) -> ArrayRowId {
        self.max_array_row
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
            // Older first, so doc ids stay ascending within the merged generation.
            let mut merged = unwrap_mem(self.generations.pop().expect("checked non-empty"));
            merged.merge_from(sealed);
            sealed = merged;
        }
        self.generations.push(Arc::new(sealed));
    }

    pub(crate) fn should_flush(&self) -> bool {
        self.memtable_size() >= self.flush_threshold
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
        let mut segments: Vec<Arc<dyn SearchReader + Send + Sync>> = Vec::with_capacity(
            self.generations.len() + self.query_segments.len()
        );
        segments.extend(
            self.generations
                .iter()
                .map(|generation| Arc::clone(generation) as Arc<dyn SearchReader + Send + Sync>)
        );
        segments.extend(self.query_segments.iter().cloned());
        IndexSnapshot::new(mem, Arc::new(segments), self.deleted.clone()).with_vectors(
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
    /// Freezes the memtable into a segment that is searchable right away from memory, and
    /// queues it to be written to disk (`take_flush_jobs`). Indexes without a root keep the
    /// segment in memory.
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

        // Oldest first: index 0 holds the lowest doc ids.
        let mut generations = std::mem::take(&mut self.generations).into_iter();
        let mut merged_mem = unwrap_mem(generations.next().expect("checked non-empty"));
        for generation in generations {
            merged_mem.merge_from(unwrap_mem(generation));
        }
        let Some((_, max_doc_id)) = merged_mem.doc_id_range() else {
            return Err(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "flushed generations contain no document ids"
                )
            );
        };
        self.next_doc_id = max_doc_id.saturating_add(1);

        let segment = Arc::new(merged_mem.freeze());
        self.segment_handles.push(SegmentHandle::Memory(Arc::clone(&segment)));
        Arc::make_mut(&mut self.query_segments).push(
            Arc::clone(&segment) as Arc<dyn SearchReader + Send + Sync>
        );

        if let Some(root) = &self.root {
            let path = root.join(format!("segment-{}.idx", self.next_segment_id));
            self.next_segment_id += 1;
            self.flush_queue.push(path, segment);
        }
        Ok(())
    }

    /// Flushes and writes the segment on the calling thread. For indexes without a background
    /// writer (reindex staging, tools); the index worker uses `queue_flush` instead.
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

    /// The queued segment writes, oldest first, to hand to a writer thread.
    pub fn take_flush_jobs(&mut self) -> Vec<FlushJob> {
        self.flush_queue.take_jobs()
    }

    /// Installs a written segment in place of its in-memory copy and records it in the
    /// manifest. A failure is kept and reported by every later flush.
    pub fn complete_flush(&mut self, seq: u64, written: io::Result<DiskSegment>) -> io::Result<()> {
        let outcome = self.flush_queue.complete(seq).and_then(|pending| {
            let installed = written.and_then(|disk| self.install_flushed(&pending, disk));
            drop_flushed_segment(pending.segment);
            installed
        });
        if let Err(e) = &outcome {
            self.flush_queue.fail(e);
        }
        outcome
    }

    fn install_flushed(&mut self, pending: &PendingFlush, disk: DiskSegment) -> io::Result<()> {
        let root = self.root
            .clone()
            .ok_or_else(|| io::Error::other("flushed segment without a root"))?;
        let position = self.segment_handles
            .iter()
            .position(
                |handle|
                    matches!(handle, SegmentHandle::Memory(s) if Arc::ptr_eq(s, &pending.segment))
            )
            .ok_or_else(|| io::Error::other("flushed segment is no longer live"))?;
        manifest::append_segment(&root, &pending.path)?;
        self.segment_handles[position] = SegmentHandle::Disk(pending.path.clone());
        Arc::make_mut(&mut self.query_segments)[position] = Arc::new(disk);
        Ok(())
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
    //////////////////////////////////////////////////////////////////////////////////////////

    #[timed(indexing_documents)]
    pub fn add_immutable_segment(&mut self, segments: Vec<ImmutableSegment>) -> io::Result<()> {
        self.last_ingest = Some(Instant::now());
        for segment in segments {
            let segment = Arc::new(segment);
            let reader: Arc<dyn SearchReader + Send + Sync> = match &self.root {
                Some(root) => {
                    let path = root.join(format!("segment-{}.idx", self.next_segment_id));
                    self.next_segment_id += 1;

                    write_segment(&path, &segment)?;
                    let disk = DiskSegment::open(&path)?;
                    manifest::append_segment(root, &path)?;
                    self.segment_handles.push(SegmentHandle::Disk(path));
                    Arc::new(disk)
                }
                None => {
                    self.segment_handles.push(SegmentHandle::Memory(segment.clone()));
                    segment as Arc<dyn SearchReader + Send + Sync>
                }
            };
            Arc::make_mut(&mut self.query_segments).push(reader);
        }

        Ok(())
    }
    pub fn segment_count(&self) -> usize {
        self.segment_handles.len()
    }

    #[timed(compaction)]
    fn segment_size_bytes(handle: &SegmentHandle) -> u64 {
        match handle {
            SegmentHandle::Disk(path) =>
                std::fs
                    ::metadata(path)
                    .map(|m| m.len())
                    .unwrap_or(0),
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
                deleted: self.deleted.clone(),
                delete_generation: self.delete_generation,
                output_path,
                vectors,
            })
        )
    }

    /// Whether merging every segment into one is worth it: the segments besides the largest
    /// hold at least 1/`ratio` of what it holds. After a load (segments of similar size) it is;
    /// for a few small segments beside a large one it is not, until enough of them pile up.
    /// Each byte is then rewritten at most about `ratio + 1` times by these merges.
    fn merge_all_due(sizes: &[Option<u64>], ratio: u64) -> bool {
        let disk: Vec<u64> = sizes.iter().flatten().copied().collect();
        if disk.len() < 2 {
            return false;
        }
        let largest = disk.iter().copied().max().unwrap_or(0);
        let rest = disk.iter().sum::<u64>() - largest;
        rest.saturating_mul(ratio.max(1)) >= largest
    }

    /// Whether a load is still settling into its final segment: documents arrived within
    /// `idle_merge_after`, some are still only in memory, or the segments are due to be merged
    /// into one (`plan_compaction` plans that merge next). Embedding waits for this to end, so
    /// it reads the merged segment rather than the ones the load leaves behind.
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
    /// COMPACTION_SIZE_RATIO of each other, at least `min_width` long.
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

            if
                end - start >= min_width &&
                best.map_or(true, |(_, _, best_total)| total < best_total)
            {
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
            if end - start >= 2 && best.map_or(true, |(_, _, best_total)| total < best_total) {
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
            self.deleted = DeleteSet::new();
            crate::lsm::deletes::clear_deletes(&root)?;
        }

        Ok(true)
    }

    #[timed(modifying_documents)]
    pub fn delete_document(&mut self, doc_id: DocId) -> io::Result<()> {
        self.last_ingest = Some(Instant::now());
        self.deleted.delete(doc_id);
        self.delete_generation += 1;

        if let Some(root) = &self.root {
            crate::lsm::deletes::append_delete(root, doc_id)?;
        }

        Ok(())
    }
    pub fn memtable_term_count(&self) -> usize {
        self.mem.term_count()
    }

    //VECTORS
    /// Paths of the live on-disk segments, oldest first. Empty for in-memory indexes.
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

    fn attach_vectors(&mut self, segment: &Path, xpath: XPathId) -> io::Result<()> {
        let file = Arc::new(VectorFile::open(vector_file_path(segment, xpath))?);
        let vectors = Arc::make_mut(&mut self.vectors);
        match vectors.iter_mut().find(|entry| entry.segment == segment && entry.xpath == xpath) {
            Some(existing) => {
                existing.file = file;
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

    /// Forgets and deletes the vector files of a segment that is being removed.
    fn drop_vectors_for(&mut self, segment: &Path) {
        let vectors = Arc::make_mut(&mut self.vectors);
        vectors.retain(|entry| {
            if entry.segment == segment {
                let _ = std::fs::remove_file(entry.file.path());
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
fn drop_flushed_segment(segment: Arc<ImmutableSegment>) {
    drop(segment);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod compaction_rules {
    use std::time::Duration;

    use super::LsmIndex;
    use crate::{
        analyzer::Analyzer,
        document::{ DocumentPart, IndexedDocument, WeightInterval },
        lsm::compaction::CompactionConfig,
        mem::MemIndex,
    };

    fn config(idle_merge_after: Duration) -> CompactionConfig {
        CompactionConfig {
            max_segments_per_compaction: 16,
            compact_when_segments_at_least: 2,
            size_ratio: 4,
            min_width: 4,
            max_segments: 10,
            idle_merge_after,
        }
    }

    /// Adds `count` documents starting at `first` and flushes them into one segment.
    fn add_segment(index: &mut LsmIndex, analyzer: &Analyzer, first: u64, count: u64) {
        let mut mem = MemIndex::default();
        for doc_id in first..first + count {
            let mut doc = IndexedDocument::new(doc_id);
            doc.parts.push(DocumentPart {
                xpath: 0,
                text: format!("document {doc_id} about rivers and cities and the sea"),
                weight: WeightInterval::TEXT,
                exact: false,
                names: false,
                gap: 0,
                segments: Vec::new(),
            });
            mem.add_analyzed_document(doc.analyze(analyzer));
        }
        index.merge_into_memtable(mem);
        index.flush().unwrap();
    }

    #[test]
    fn a_finished_load_is_merged_into_one_segment() {
        assert!(LsmIndex::merge_all_due(&[Some(225), Some(478)], 4));
        assert!(LsmIndex::merge_all_due(&[Some(33), Some(187), Some(452)], 4));
    }

    #[test]
    fn small_segments_beside_a_large_one_leave_it_alone() {
        assert!(!LsmIndex::merge_all_due(&[Some(4_000), Some(8), Some(9)], 4));
        // until together they hold a quarter of it
        assert!(LsmIndex::merge_all_due(&[Some(4_000), Some(600), Some(400)], 4));
        assert!(!LsmIndex::merge_all_due(&[Some(100)], 4));
        assert!(!LsmIndex::merge_all_due(&[Some(100), None], 4));
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn segments_merge_into_one_once_documents_stop_arriving() {
        let root = temp_root("compaction-rules");
        let analyzer = Analyzer::new();
        let mut index = LsmIndex::persistent(&root, usize::MAX).unwrap();
        for part in 0..3 {
            add_segment(&mut index, &analyzer, part * 50, 50);
        }
        assert_eq!(index.segment_count(), 3);
        let loading = config(Duration::from_secs(3600));
        let stopped = config(Duration::ZERO);

        // still loading: three similar segments are fewer than a tiered merge needs
        assert!(index.plan_compaction(loading).unwrap().is_none());
        assert!(index.settling(&loading));
        // loading stopped: all three into one, and embedding waits for that merge
        assert!(index.settling(&stopped));
        let job = index.plan_compaction(stopped).unwrap().unwrap();
        assert_eq!(job.selected.len(), 3);

        // reopened: nothing arrived since, so the load is over
        drop(index);
        let mut reopened = LsmIndex::persistent(&root, usize::MAX).unwrap();
        let job = reopened.plan_compaction(loading).unwrap().unwrap();
        assert_eq!(job.selected.len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn documents_still_in_memory_keep_a_load_settling() {
        let root = temp_root("compaction-settling");
        let analyzer = Analyzer::new();
        let mut index = LsmIndex::persistent(&root, usize::MAX).unwrap();
        add_segment(&mut index, &analyzer, 0, 2_000);
        let stopped = config(Duration::ZERO);
        // one segment, nothing arriving: settled
        assert!(!index.settling(&stopped));

        // a few new documents not written yet
        let mut mem = MemIndex::default();
        let mut doc = IndexedDocument::new(5_000);
        doc.parts.push(DocumentPart {
            xpath: 0,
            text: "a late document".to_owned(),
            weight: WeightInterval::TEXT,
            exact: false,
            names: false,
            gap: 0,
            segments: Vec::new(),
        });
        mem.add_analyzed_document(doc.analyze(&analyzer));
        index.merge_into_memtable(mem);
        assert!(index.settling(&stopped));

        // written: a small segment beside a large one is no reason to wait
        index.flush().unwrap();
        assert_eq!(index.segment_count(), 2);
        assert!(!index.settling(&stopped));
        let _ = std::fs::remove_dir_all(&root);
    }
}
