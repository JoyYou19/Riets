use std::{io, path::PathBuf, sync::Arc};

use core_timing::timed;

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
    types::{DocId, XPathId},
    wildcard::WildcardPattern,
};

// Live index of data, this will be flushed in other words put into a persistent
// memtable, snapshoting, deleting
pub struct LsmIndex {
    mem: MemIndex,
    //TEST
    generations:Vec<Arc<MemIndex>>,
    segment_handles: Vec<SegmentHandle>,
    generation_bytes: usize,
    query_segments: Arc<Vec<Arc<dyn SearchReader + Send + Sync>>>,
    flush_threshold: usize,
    deleted: DeleteSet,
    delete_generation:u64,
    root: Option<PathBuf>,
    next_segment_id: u64,
    next_compaction_job_id: u64,
    
}
const GENERATION_MERGE_RATIO:usize=2;
fn unwrap_mem(generation: Arc<MemIndex>) -> MemIndex {
    Arc::try_unwrap(generation).unwrap_or_else(|shared| (*shared).clone())
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
}

impl LsmIndex {
    pub fn new(flush_threshold: usize) -> Self {
        Self {
            mem: MemIndex::new(),
            generations:Vec::new(),
            generation_bytes:0,
            segment_handles: Vec::new(),
            query_segments: Arc::new(Vec::new()),
            flush_threshold,
            deleted: DeleteSet::new(),
            delete_generation:0,
            root: None,
            next_segment_id: 0,
            next_compaction_job_id: 0,
        }
    }

    #[timed(database_lifecycle)]
    pub fn persistent(root: impl Into<PathBuf>, flush_threshold: usize) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;

        let segment_paths = manifest::read_manifest(&root)?;

        let mut segment_handles = Vec::new();
        let mut query_segments: Vec<Arc<dyn SearchReader + Send + Sync>> = Vec::new();
        let mut next_segment_id = 0;
        let deleted = crate::lsm::deletes::read_deletes(&root)?;
        // let mut deleted_generation= 
        for path in segment_paths {
            let disk = DiskSegment::open(&path)?;

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
            generations:Vec::new(),
            generation_bytes:0,
            segment_handles,
            query_segments: Arc::new(query_segments),
            flush_threshold,
            deleted,
            delete_generation:0, //incorrect
            root: Some(root),
            next_segment_id,
            next_compaction_job_id: 0,
        })
    }
    //TEST
        #[timed(indexing_documents)]
        fn seal(&mut self) {
        if self.mem.term_count() == 0 {
            return;
        }
        let sealed = std::mem::take(&mut self.mem);
        self.generation_bytes += sealed.estimated_size_bytes();
        self.generations.push(Arc::new(sealed));

        // Merge the two newest while the older isn't at least RATIO× bigger.
        while self.generations.len() >= 2 {
            let n = self.generations.len();
            if self.generations[n - 2].estimated_size_bytes()
                > GENERATION_MERGE_RATIO * self.generations[n - 1].estimated_size_bytes()
            {
                break;
            }
            let newer = unwrap_mem(self.generations.pop().unwrap());
            let mut older = unwrap_mem(self.generations.pop().unwrap());
            older.merge_from(newer);
            self.generations.push(Arc::new(older));
        }
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
        segments.extend(self.generations.iter().map(|g| g.clone() as Arc<dyn SearchReader + Send + Sync>));
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

        if self.mem.estimated_size_bytes() >= self.flush_threshold {
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

        if self.mem.estimated_size_bytes() >= self.flush_threshold {
            self.flush()?;
        }

        Ok(())
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
        let mut gens = std::mem::take(&mut self.generations).into_iter().map(unwrap_mem);
        let mut merged = gens.next().unwrap();
        for newer in gens {
            merged.merge_from(newer);
        }
        self.generation_bytes = 0;

        let segment = Arc::new(merged.freeze());

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
        #[timed(compaction)]
    pub fn plan_compaction(
        &mut self,
        config: CompactionConfig,
    ) -> io::Result<Option<CompactionJob>> {
        if self.segment_count() < config.compact_when_segments_at_least {
            return Ok(None);
        }
        let Some(root) = &self.root else {
            return Ok(None);
        };

        // Merge every disk segment into one, in list order (roughly doc_id order).
        let selected: Vec<SegmentHandle> = self
            .segment_handles
            .iter()
            .filter(|handle| matches!(handle, SegmentHandle::Disk(_)))
            .take(config.max_segments_per_compaction)
            .cloned()
            .collect();

        if selected.len() < 2 {
            return Ok(None);
        }

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
        let insert_pos=positions[0];
        for pos in positions.into_iter().rev() {
            self.segment_handles.remove(pos);
            segs.remove(pos);
        }

        self.segment_handles.insert(insert_pos, SegmentHandle::Disk(completed.output_path.clone()));
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

        if merged_all && completed.delete_generation == self.delete_generation{
            self.deleted = DeleteSet::new();
            crate::lsm::deletes::clear_deletes(root)?;
        }

        Ok(true)
    }

    #[timed(modifying_documents)]
    pub fn delete_document(&mut self, doc_id: DocId) -> io::Result<()> {
        self.deleted.delete(doc_id);
        self.delete_generation+=1;

        if let Some(root) = &self.root {
            crate::lsm::deletes::append_delete(root, doc_id)?;
        }

        Ok(())
    }
    pub fn memtable_term_count(&self) -> usize {
        self.mem.term_count()
    }
}
