use std::{
    cmp::Reverse, collections::{BTreeMap, BinaryHeap}, io, iter::Peekable, path::{Path, PathBuf}, sync::Arc,
};

use serde::{Deserialize, Serialize};

use core_timing::timed;

use crate::{
    disk::{reader::DiskSegment, writer::write_merged_segment},
    numeric_values::{NumericFields, NumericPoints, unpack},
    posting::{DeleteSet, PostingList},
    segment::{ImmutableSegment, SegmentHandle},
    types::{DocId, TermKey, XPathId},
};

type TermIter<'a> = Box<dyn Iterator<Item = (TermKey, PostingList)> + 'a>;

enum OpenSegment {
    Disk(DiskSegment),
    Memory(Arc<ImmutableSegment>),
}

impl OpenSegment {
    fn doc_lengths(&self) -> &BTreeMap<(DocId, XPathId), u32> {
        match self {
            OpenSegment::Disk(d) => d.doc_lengths(),
            OpenSegment::Memory(m) => m.doc_lengths(),
        }
    }

    fn numeric_fields(&self) -> &NumericFields {
        match self {
            OpenSegment::Disk(d) => d.numeric_fields(),
            OpenSegment::Memory(m) => m.numeric_fields(),
        }
    }

    fn iter_terms(&self) -> TermIter<'_> {
        match self {
            OpenSegment::Disk(d) => Box::new(d.iter_terms()),
            OpenSegment::Memory(m) => {
                Box::new(m.terms().iter().map(|(k, v)| (k.clone(), v.clone())))
            }
        }
    }
}

struct MergedTerms<'a> {
    sources: Vec<TermIter<'a>>,
    heads: Vec<Option<PostingList>>,
    heap: BinaryHeap<Reverse<(TermKey, usize)>>,
    deleted: &'a DeleteSet,
}
impl<'a> MergedTerms<'a> {
    fn new(mut sources: Vec<TermIter<'a>>, deleted: &'a DeleteSet) -> Self {
        let mut heads = Vec::with_capacity(sources.len());
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (index, source) in sources.iter_mut().enumerate() {
            match source.next() {
                Some((key, list)) => {
                    heap.push(Reverse((key, index)));
                    heads.push(Some(list));
                }
                None => heads.push(None),
            }
        }
        Self { sources, heads, heap, deleted }
    }

    /// Takes the current posting list of `index` and loads its next term.
    fn advance(&mut self, index: usize) -> PostingList {
        let list = self.heads[index].take().unwrap_or_default();
        if let Some((key, next)) = self.sources[index].next() {
            self.heap.push(Reverse((key, index)));
            self.heads[index] = Some(next);
        }
        list
    }
}
impl<'a> Iterator for MergedTerms<'a> {
    type Item = (TermKey, PostingList);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let Reverse((key, index)) = self.heap.pop()?;
            let mut items = self.advance(index).into_items();

            // Same term in other sources: pop them too. Ties pop in source
            // index order, which is doc_id order (sources are sorted below).
            while matches!(self.heap.peek(), Some(Reverse((next_key, _))) if *next_key == key) {
                let Reverse((_, other)) = self.heap.pop().unwrap();
                items.extend(self.advance(other).into_items());
            }

            // Sorts only if the input ranges overlapped; merges duplicate doc_ids.
            let mut merged = PostingList::from_items(items);
            self.deleted.filter_in_place(&mut merged);
            if !merged.is_empty() {
                return Some((key, merged));
            }
        }
    }
}

#[timed(compaction)]
pub fn compact_segments_streaming(
    handles: &[SegmentHandle],
    deleted: &DeleteSet,
    output_path: &Path,
) -> io::Result<()> {
    let mut opened = Vec::with_capacity(handles.len());
    for handle in handles {
        opened.push(match handle {
            SegmentHandle::Disk(path) => OpenSegment::Disk(DiskSegment::open(path)?),
            SegmentHandle::Memory(segment) => OpenSegment::Memory(segment.clone()),
        });
    }

    // doc_lengths has no position lists, so it's far smaller than postings —
    // merging it eagerly here is a deliberate simplification, not an oversight.
    let mut merged_doc_lengths: BTreeMap<(DocId, XPathId), u32> = BTreeMap::new();
    for segment in &opened {
        for (&(doc_id, xpath), &len) in segment.doc_lengths() {
            if deleted.contains(doc_id) {
                continue;
            }
            merged_doc_lengths.insert((doc_id, xpath), len);
        }
    }

    let mut merged_points = NumericPoints::default();
    for segment in &opened {
        for (xpath, field) in segment.numeric_fields().iter() {
            let kind = field.doc_values.kind();
            for &(doc_id, packed) in field.doc_values.entries() {
                if deleted.contains(doc_id) {
                    continue;
                }
                merged_points.insert(xpath, unpack(packed, kind), doc_id);
            }
        }
    }

    let sources: Vec<TermIter<'_>> = opened.iter().map(|s| s.iter_terms()).collect();
    let merged_terms = MergedTerms::new(sources, deleted);

    write_merged_segment(
        output_path,
        merged_terms,
        &merged_doc_lengths,
        &merged_points.build(),
    )
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub max_segments_per_compaction: usize,
    pub compact_when_segments_at_least: usize,
   
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_segments_per_compaction: 16,
            compact_when_segments_at_least: 4,
            

        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionJob {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub deleted: DeleteSet,
    pub delete_generation:u64,
    pub output_path: PathBuf,
}

#[derive(Debug)]
pub struct CompletedCompaction {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub output_path: PathBuf,
    pub delete_generation:u64
}
