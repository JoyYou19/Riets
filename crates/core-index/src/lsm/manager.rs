use std::{io, path::PathBuf, sync::Arc};

use core_timing::timed;

use crate::{
    analyzer::analyzer::Analyzer,
    disk::{reader::DiskSegment, writer::write_segment},
    lsm::{
        IndexSnapshot,
        compaction::{CompactionConfig, CompactionJob, CompletedCompaction},
        manifest,
    },
    mem::MemIndex,
    posting::{DeleteSet, PostingList},
    search::{SearchIndex, SearchReader, SearchStats},
    segment::{ImmutableSegment, SegmentHandle},
    types::{DocId, XPathId},
    wildcard::WildcardPattern,
};

// Live index of data, this will be flushed in other words put into a persistent
// memtable, snapshoting, deleting
pub struct LsmIndex {
    mem: MemIndex,
    segment_handles: Vec<SegmentHandle>,
    query_segments: Vec<Arc<dyn SearchReader + Send + Sync>>,
    flush_threshold: usize,
    deleted: DeleteSet,

    root: Option<PathBuf>,
    next_segment_id: u64,
    next_compaction_job_id: u64,
}

impl SearchIndex for LsmIndex {
    #[timed(search)]
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.snapshot().lookup(term, xpath)
    }

    #[timed(search)]
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.snapshot().lookup_prefix(prefix, xpath)
    }

    #[timed(search)]
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        self.snapshot().lookup_wildcard(pattern, xpath)
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
            segment_handles: Vec::new(),
            query_segments: Vec::new(),
            flush_threshold,
            deleted: DeleteSet::new(),
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

        for path in segment_paths {
            let disk = DiskSegment::open(&path)?;

            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && let Some(id) = stem
                    .strip_prefix("segment-")
                    .and_then(|value| value.parse::<u64>().ok())
            {
                next_segment_id = next_segment_id.max(id + 1);
            }

            segment_handles.push(SegmentHandle::Disk(path));
            // Non primitive cast alaallala
            let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
            query_segments.push(disk);
        }

        let deleted = crate::lsm::deletes::read_deletes(&root)?;

        Ok(Self {
            mem: MemIndex::new(),
            segment_handles,
            query_segments,
            flush_threshold,
            deleted,
            root: Some(root),
            next_segment_id,
            next_compaction_job_id: 0,
        })
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

        if self.mem.term_count() >= self.flush_threshold {
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

        if self.mem.term_count() >= self.flush_threshold {
            self.flush()?;
        }

        Ok(())
    }

    #[timed(indexing_documents)]
    pub fn add_immutable_segment(&mut self, segment: ImmutableSegment) -> io::Result<()> {
        let segment = Arc::new(segment);

        match &self.root {
            Some(root) => {
                let path = root.join(format!("segment-{}.idx", self.next_segment_id));
                self.next_segment_id += 1;

                write_segment(&path, &segment)?;

                let disk = DiskSegment::open(&path)?;

                self.segment_handles.push(SegmentHandle::Disk(path));
                self.query_segments
                    .push(Arc::new(disk) as Arc<dyn SearchReader + Send + Sync>);

                let disk_paths: Vec<PathBuf> = self
                    .segment_handles
                    .iter()
                    .filter_map(|handle| match handle {
                        SegmentHandle::Disk(path) => Some(path.clone()),
                        SegmentHandle::Memory(_) => None,
                    })
                    .collect();

                manifest::write_manifest(root, &disk_paths)?;
            }

            None => {
                self.segment_handles
                    .push(SegmentHandle::Memory(segment.clone()));
                self.query_segments
                    .push(segment as Arc<dyn SearchReader + Send + Sync>);
            }
        }

        Ok(())
    }

    // Converts a mutable indexing state into a readonly segment
    // so we can query, share, serialize, compact the data
    #[timed(flushing)]
    pub fn flush(&mut self) -> io::Result<()> {
        let old_mem = std::mem::take(&mut self.mem);

        if old_mem.term_count() == 0 {
            return Ok(());
        }

        let segment = Arc::new(old_mem.freeze());

        match &self.root {
            Some(root) => {
                let path = root.join(format!("segment-{}.idx", self.next_segment_id));
                self.next_segment_id += 1;

                write_segment(&path, &segment)?;

                let disk = DiskSegment::open(&path)?;

                self.segment_handles.push(SegmentHandle::Disk(path));
                let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
                self.query_segments.push(disk);

                let disk_paths: Vec<PathBuf> = self
                    .segment_handles
                    .iter()
                    .filter_map(|handle| match handle {
                        SegmentHandle::Disk(path) => Some(path.clone()),
                        SegmentHandle::Memory(_) => None,
                    })
                    .collect();

                manifest::write_manifest(root, &disk_paths)?;
            }

            None => {
                self.segment_handles
                    .push(SegmentHandle::Memory(segment.clone()));

                let reader: Arc<dyn SearchReader + Send + Sync> = segment;
                self.query_segments.push(reader);
            }
        }

        Ok(())
    }

    pub fn snapshot(&self) -> IndexSnapshot {
        IndexSnapshot::new(
            self.mem.clone(),
            self.query_segments.clone(),
            self.deleted.clone(),
        )
    }

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
        if self.segment_count() < config.compact_when_segments_at_least {
            return Ok(None);
        }

        if config.max_segments_per_compaction < 2 {
            return Ok(None);
        }

        let Some(root) = &self.root else {
            return Ok(None);
        };

        let mut by_size: Vec<(u64, usize, SegmentHandle)> = self
            .segment_handles
            .iter()
            .enumerate()
            .map(|(index, handle)| (Self::segment_size_bytes(handle), index, handle.clone()))
            .collect();

        by_size.sort_by_key(|(size, index, _)| (*size, *index));

        let selected: Vec<SegmentHandle> = by_size
            .iter()
            .take(config.max_segments_per_compaction)
            .map(|(_, _, handle)| handle.clone())
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

        positions.sort_unstable();
        positions.dedup();
        for pos in positions.into_iter().rev() {
            self.segment_handles.remove(pos);
            self.query_segments.remove(pos);
        }

        self.segment_handles
            .insert(0, SegmentHandle::Disk(completed.output_path.clone()));
        let disk: Arc<dyn SearchReader + Send + Sync> = Arc::new(disk);
        self.query_segments.insert(0, disk);

        let disk_paths: Vec<PathBuf> = self
            .segment_handles
            .iter()
            .filter_map(|handle| match handle {
                SegmentHandle::Disk(path) => Some(path.clone()),
                SegmentHandle::Memory(_) => None,
            })
            .collect();

        manifest::write_manifest(root, &disk_paths)?;

        for handle in completed.selected {
            if let SegmentHandle::Disk(path) = handle {
                std::fs::remove_file(path).ok();
            }
        }

        if merged_all {
            self.deleted = DeleteSet::new();
            crate::lsm::deletes::clear_deletes(root)?;
        }

        Ok(true)
    }

    #[timed(modifying_documents)]
    pub fn delete_document(&mut self, doc_id: DocId) -> io::Result<()> {
        self.deleted.delete(doc_id);

        if let Some(root) = &self.root {
            crate::lsm::deletes::append_delete(root, doc_id)?;
        }

        Ok(())
    }
    pub fn memtable_term_count(&self) -> usize {
        self.mem.term_count()
    }
}
