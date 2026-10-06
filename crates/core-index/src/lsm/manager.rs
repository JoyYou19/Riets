use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::HashSet;
use core_timing::timed;
use roaring::RoaringTreemap;

use crate::{
    analyzer::analyzer::Analyzer,
    disk::{reader::DiskSegment, writer::write_segment},
    fuzzy::{FuzzyExpansion, FuzzyOptions},
    lsm::{
        IndexSnapshot,
        compaction::{CompactionConfig, CompactionJob, CompletedCompaction},
        manifest,
    },
    mem::MemIndex,
    numeric_values::{NumericBound, NumericValue},
    posting::{DeleteSet, PostingList},
    search::{SearchIndex, SearchNumeric, SearchReader, SearchStats},
    segment::{ImmutableSegment, SegmentHandle},
    types::{ArrayRowId, DocId, XPathId},
    wildcard::WildcardPattern,
};

// Live index of data, this will be flushed in other words put into a persistent
// memtable, snapshoting, deleting
pub struct LsmIndex {
    mem: MemIndex,

    generations: Vec<Arc<MemIndex>>,
    segment_handles: Vec<SegmentHandle>,

    query_segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
    flush_threshold: usize,
    deleted: DeleteSet,
    delete_generation: u64,
    root: Option<PathBuf>,
    next_segment_id: u64,
    next_doc_id: DocId,
    next_compaction_job_id: u64,
    max_array_row: ArrayRowId,
    last_ingest: Instant,
}

//INFO: WATAFAK - normunds

//THIS DOESNT NEED TO BE CONFIGURABlE I THINK?
/// Hard cap on in-memory generations so snapshot fan-out stays bounded.
const MAX_GENERATIONS: usize = 16;
const STAGING_LIMIT_BYTES: usize = 16 * 1024 * 1024;
/// A compaction run only contains segments whose sizes are within this factor of each other.
const COMPACTION_SIZE_RATIO: u64 = 4;
/// Fewest similar-sized adjacent segments worth merging during ingest.
const MIN_COMPACTION_WIDTH: usize = 4;
/// Above this many segments, merge the cheapest adjacent run even if sizes differ.
const MAX_SEGMENTS_TARGET: usize = 10;
/// After this long without inserts or deletes, merge everything into one segment.
const IDLE_FULL_MERGE_AFTER: Duration = Duration::from_secs(30);
/// Fewest segments worth merging in one job.

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
        opts: FuzzyOptions,
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
        hi: Option<NumericBound>,
    ) -> PostingList {
        self.snapshot().numeric_range(xpath, lo, hi)
    }

    #[timed(search)]
    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.snapshot().numeric_value(xpath, doc_id)
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
            deleted: DeleteSet::new(),
            delete_generation: 0,
            root: None,
            next_doc_id: 0,
            next_segment_id: 0,
            next_compaction_job_id: 0,
            max_array_row: 0,
            last_ingest: Instant::now(),
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
        for path in segment_paths {
            let disk = DiskSegment::open(&path)?;
            if let Some((_, max)) = disk.doc_range() {
                next_doc_id = next_doc_id.max(max + 1);
            }

            max_array_row = max_array_row.max(disk.array_row_index().max_array_row().unwrap_or(0));

            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && let Some(id) = stem
                    .strip_prefix("segment-")
                    .and_then(|value| value.parse::<u64>().ok())
            {
                next_segment_id = next_segment_id.max(id + 1);
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
            deleted,
            delete_generation: 0, //incorrect
            root: Some(root),
            next_segment_id,
            next_doc_id,
            next_compaction_job_id: 0,
            max_array_row,
            last_ingest: Instant::now(),
        })
    }

    pub fn max_array_row(&self) -> ArrayRowId {
        self.max_array_row
    }

    //TEST
    #[timed(indexing_documents)]
    fn seal(&mut self) {
        if self.mem.term_count() == 0 && self.mem.doc_id_range().is_none() {
            return;
        }
        let sealed = std::mem::take(&mut self.mem);
        self.generations.push(Arc::new(sealed));
    }

    fn should_flush(&self) -> bool {
        self.memtable_size() >= self.flush_threshold || self.generations.len() >= MAX_GENERATIONS
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
        let mut segments: Vec<Arc<dyn SearchReader + Send + Sync>> =
            Vec::with_capacity(self.generations.len() + self.query_segments.len());
        segments.extend(
            self.generations
                .iter()
                .map(|generation| Arc::clone(generation) as Arc<dyn SearchReader + Send + Sync>),
        );
        segments.extend(self.query_segments.iter().cloned());
        IndexSnapshot::new(mem, Arc::new(segments), self.deleted.clone())
    }

    #[timed(indexing_documents)]
    pub fn add_document(
        &mut self,
        analyzer: &Analyzer,
        doc_id: DocId,
        xpath: XPathId,
        text: &str,
    ) -> io::Result<()> {
        self.mem.add_document(analyzer, doc_id, xpath, text);

        if self.should_flush() {
            self.flush()?;
        }

        Ok(())
    }

    #[timed(indexing_documents)]
    pub fn add_indexed_document(
        &mut self,
        analyzer: &Analyzer,
        document: &crate::document::IndexedDocument,
    ) -> io::Result<()> {
        self.mem.add_indexed_document(analyzer, document);

        if self.memtable_size() >= self.flush_threshold {
            self.flush()?;
        }

        Ok(())
    }
    pub fn add_indexed_documents(
        &mut self,
        analyzer: &Analyzer,
        documents: &[crate::document::IndexedDocument],
    ) -> (u64, io::Result<()>) {
        let mut staging = MemIndex::default();
        let mut added = 0u64;
        let mut pending = 0u64;

        for document in documents {
            staging.add_indexed_document(analyzer, document);
            pending += 1;

            let staged = staging.estimated_size_bytes();
            if staged >= STAGING_LIMIT_BYTES
                || self.memtable_size() + staged >= self.flush_threshold
            {
                self.mem.merge_from(std::mem::take(&mut staging));
                added += pending;
                pending = 0;
                if self.should_flush() {
                    if let Err(error) = self.flush() {
                        return (added, Err(error));
                    }
                }
            }
        }

        if pending > 0 {
            self.mem.merge_from(staging);
            added += pending;
            if self.should_flush() {
                if let Err(error) = self.flush() {
                    return (added, Err(error));
                }
            }
        }

        (added, Ok(()))
    }
    pub fn merge_into_memtable(&mut self, other: MemIndex) {
        self.mem.merge_from(other);
    }

    pub fn memtable_size(&self) -> usize {
        self.mem.estimated_size_bytes()
            + self
                .generations
                .iter()
                .map(|generation| generation.estimated_size_bytes())
                .sum::<usize>()
    }

    pub fn flush_threshold(&self) -> usize {
        self.flush_threshold
    }
    #[timed(indexing_documents)]
    pub fn add_immutable_segment(&mut self, segments: Vec<ImmutableSegment>) -> io::Result<()> {
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
                    self.segment_handles
                        .push(SegmentHandle::Memory(segment.clone()));
                    segment as Arc<dyn SearchReader + Send + Sync>
                }
            };
            Arc::make_mut(&mut self.query_segments).push(reader);
        }

        Ok(())
    }

    // Converts a mutable indexing state into a readonly segment
    // so we can query, share, serialize, compact the data
    #[timed(flushing)]

    pub fn flush(&mut self) -> io::Result<()> {
        self.seal();
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "flushed generations contain no document ids",
            ));
        };
        self.next_doc_id = max_doc_id.saturating_add(1);

        let segment = Arc::new(merged_mem.freeze());

        let reader: Arc<dyn SearchReader + Send + Sync> = match &self.root {
            Some(root) => {
                let path = root.join(format!("segment-{}.idx", self.next_segment_id));
                self.next_segment_id += 1;
                write_segment(&path, &segment)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
                let disk = DiskSegment::open(&path)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
                manifest::append_segment(root, &path)?;
                self.segment_handles.push(SegmentHandle::Disk(path));
                drop_flushed_segment(segment);
                Arc::new(disk)
            }
            None => {
                self.segment_handles
                    .push(SegmentHandle::Memory(segment.clone()));
                segment as Arc<dyn SearchReader + Send + Sync>
            }
        };

        Arc::make_mut(&mut self.query_segments).push(reader);
        Ok(())
    }

    // pub fn snapshot(&self) -> IndexSnapshot {
    //     IndexSnapshot::new(
    //         self.mem.clone(),
    //         self.query_segments.clone(),
    //         self.deleted.clone(),
    //     )
    // }

    pub fn segment_count(&self) -> usize {
        self.segment_handles.len()
    }

    // Compacts all segments, probably not what we want
    // #[timed(compaction)]
    // pub fn compact_all(&mut self) -> io::Result<()> {
    //     if self.segment_handles.len() <= 1 {
    //         return Ok(());
    //     }
    //
    //     let Some(root) = &self.root else {
    //         return Ok(());
    //     };
    //
    //     let old_paths: Vec<PathBuf> = self
    //         .segment_handles
    //         .iter()
    //         .filter_map(|handle| match handle {
    //             SegmentHandle::Disk(path) => Some(path.clone()),
    //             SegmentHandle::Memory(_) => None,
    //         })
    //         .collect();
    //
    //     let compacted_path = root.join(format!("segment-{}.idx", self.next_segment_id));
    //     self.next_segment_id += 1;
    //
    //     compact_segments_streaming(&self.segment_handles, &self.deleted, &compacted_path)?;
    //
    //     let disk = DiskSegment::open(&compacted_path)?;
    //
    //     self.segment_handles.clear();
    //     self.query_segments.clear();
    //
    //     self.segment_handles
    //         .push(SegmentHandle::Disk(compacted_path.clone()));
    //     let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
    //     self.query_segments.push(disk);
    //
    //     manifest::write_manifest(root, &[compacted_path])?;
    //
    //     for path in old_paths {
    //         std::fs::remove_file(path).ok();
    //     }
    //
    //     self.deleted = DeleteSet::new();
    //     crate::lsm::deletes::clear_deletes(root)?;
    //
    //     Ok(())
    // }

    #[timed(compaction)]
    fn segment_size_bytes(handle: &SegmentHandle) -> u64 {
        match handle {
            SegmentHandle::Disk(path) => std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            //migh need a smarter way but still this is ok for aproximating the segment size
            SegmentHandle::Memory(segment) => segment.terms().len() as u64,
        }
    }

    #[timed(compaction)]

    pub fn plan_compaction(
        &mut self,
        config: CompactionConfig,
    ) -> io::Result<Option<CompactionJob>> {
        let Some(root) = &self.root else {
            return Ok(None);
        };

        let sizes: Vec<Option<u64>> = self
            .segment_handles
            .iter()
            .map(|handle| match handle {
                SegmentHandle::Disk(_) => Some(Self::segment_size_bytes(handle).max(1)),
                SegmentHandle::Memory(_) => None,
            })
            .collect();

        let disk_segments = sizes.iter().filter(|size| size.is_some()).count();
        if disk_segments < 2 {
            return Ok(None);
        }

        let idle = self.last_ingest.elapsed() >= IDLE_FULL_MERGE_AFTER;
        let all_disk = disk_segments == sizes.len();
        let max_width = config.max_segments_per_compaction.max(MIN_COMPACTION_WIDTH);

        let window = if idle && all_disk {
            Some((0, sizes.len()))
        } else if self.segment_count() >= config.compact_when_segments_at_least {
            Self::tiered_window(&sizes, MIN_COMPACTION_WIDTH, max_width).or_else(|| {
                (self.segment_count() > MAX_SEGMENTS_TARGET)
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
        let output_path = root.join(format!("segment-{}.idx", self.next_segment_id));
        self.next_segment_id += 1;
        let job_id = self.next_compaction_job_id;
        self.next_compaction_job_id += 1;

        Ok(Some(CompactionJob {
            job_id,
            selected,
            deleted: self.deleted.clone(),
            delete_generation: self.delete_generation,
            output_path,
        }))
    }

    /// Cheapest contiguous run of disk segments whose sizes are within
    /// COMPACTION_SIZE_RATIO of each other, at least `min_width` long.
    fn tiered_window(
        sizes: &[Option<u64>],
        min_width: usize,
        max_width: usize,
    ) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, u64)> = None;

        for start in 0..sizes.len() {
            let Some(first) = sizes[start] else { continue };
            let (mut smallest, mut largest, mut total) = (first, first, first);
            let mut end = start + 1;

            while end < sizes.len() && end - start < max_width {
                let Some(size) = sizes[end] else { break };
                let next_smallest = smallest.min(size);
                let next_largest = largest.max(size);
                if next_largest > next_smallest.saturating_mul(COMPACTION_SIZE_RATIO) {
                    break;
                }
                smallest = next_smallest;
                largest = next_largest;
                total += size;
                end += 1;
            }

            if end - start >= min_width
                && best.map_or(true, |(_, _, best_total)| total < best_total)
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
                let Some(size) = sizes[end] else { break };
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
        let Some(root) = &self.root else {
            return Ok(false);
        };

        // Locate the selected segments wherever they are in the live list.
        let mut positions: Vec<usize> = completed
            .selected
            .iter()
            .filter_map(|handle| self.segment_handles.iter().position(|live| live == handle))
            .collect();

        if positions.len() != completed.selected.len() {
            // Stale job — some input was already merged away. Drop the output.
            std::fs::remove_file(&completed.output_path).ok();
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

        self.segment_handles.insert(
            insert_pos,
            SegmentHandle::Disk(completed.output_path.clone()),
        );
        let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
        segs.insert(insert_pos, disk);

        let disk_paths: Vec<PathBuf> = self
            .segment_handles
            .iter()
            .filter_map(|handle| match handle {
                SegmentHandle::Disk(path) => Some(path.clone()),
                SegmentHandle::Memory(_) => None,
            })
            .collect();

        manifest::compact_manifest(root, &disk_paths)?;

        for handle in completed.selected {
            if let SegmentHandle::Disk(path) = handle {
                std::fs::remove_file(path).ok();
            }
        }

        if merged_all && completed.delete_generation == self.delete_generation {
            self.deleted = DeleteSet::new();
            crate::lsm::deletes::clear_deletes(root)?;
        }

        Ok(true)
    }

    #[timed(modifying_documents)]
    pub fn delete_document(&mut self, doc_id: DocId) -> io::Result<()> {
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
}

#[timed(flushing)]
fn drop_flushed_segment(segment: Arc<crate::segment::ImmutableSegment>) {
    drop(segment);
}
