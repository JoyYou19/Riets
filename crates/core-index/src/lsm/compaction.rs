use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    fs
};
use serde::{Deserialize, Serialize};
use core_timing::timed;
use crate::{
    array_rows::ArrayRowIndex,
    disk::{reader::DiskSegment, writer::write_merged_segment},
    document::document::BoolField,
    numeric_values::{NumericFields, NumericPoints, unpack},
    posting::{DeleteSet, PostingList},
    segment::{ImmutableSegment, SegmentHandle},
    types::{DocId, TermKey, XPathId},
    vector::{SegmentVectors, VectorFile, merge_vector_files, vector_file_path}
};

type TermIter<'a> = Box<dyn Iterator<Item = (TermKey, PostingList)> + 'a>;

enum OpenSegment {
    Disk(DiskSegment),
    Memory(Arc<ImmutableSegment>),
}

impl OpenSegment {
    fn doc_lengths(&self) -> Box<dyn Iterator<Item = ((DocId, XPathId), u32)> + '_> {
        match self {
            OpenSegment::Disk(d) => Box::new(d.doc_lengths().iter()),
            OpenSegment::Memory(m) => Box::new(m.doc_lengths().iter().map(|(&key, &len)| (key, len))),
        }
    }

    fn numeric_fields(&self) -> &NumericFields {
        match self {
            OpenSegment::Disk(d) => d.numeric_fields(),
            OpenSegment::Memory(m) => m.numeric_fields(),
        }
    }

    fn bool_fields(&self) -> &BTreeMap<XPathId, BoolField> {
        match self {
            OpenSegment::Disk(d) => d.bool_fields(),
            OpenSegment::Memory(m) => m.bool_fields(),
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

    fn array_row_index(&self) -> &ArrayRowIndex {
        match self {
            OpenSegment::Disk(d) => d.array_row_index(),
            OpenSegment::Memory(m) => m.array_row_index(),
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
        Self {
            sources,
            heads,
            heap,
            deleted,
        }
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub max_segments_per_compaction: usize,
    pub compact_when_segments_at_least: usize,
}
impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_segments_per_compaction: 16,
            compact_when_segments_at_least: 2,
        }
    }
}
#[derive(Debug, Clone)]
pub struct CompactionJob {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub deleted: DeleteSet,
    pub delete_generation: u64,
    pub output_path: PathBuf,
    pub vectors: Vec<SegmentVectors>
}
#[derive(Debug)]
pub struct CompletedCompaction {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub output_path: PathBuf,
    pub delete_generation: u64,
    pub vector_xpaths:Vec<XPathId>
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
        for ((doc_id, xpath), len) in segment.doc_lengths() {
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

    let mut merged_bool_fields: BTreeMap<XPathId, BoolField> = BTreeMap::new();
    for segment in &opened {
        for (xpath, field) in segment.bool_fields() {
            let slot = merged_bool_fields
                .entry(*xpath)
                .or_insert_with(BoolField::default);
            for id in field.true_ids.iter() {
                if !deleted.contains(id) {
                    slot.true_ids.insert(id);
                }
            }
            for id in field.false_ids.iter() {
                if !deleted.contains(id) {
                    slot.false_ids.insert(id);
                }
            }
        }
    }

    let mut merged_array_rows = ArrayRowIndex::default();
    for segment in &opened {
        merged_array_rows.merge_from(segment.array_row_index());
    }

    let sources: Vec<TermIter<'_>> = opened.iter().map(|s| s.iter_terms()).collect();
    let merged_terms = MergedTerms::new(sources, deleted);

    write_merged_segment(
        output_path,
        merged_terms,
        &merged_doc_lengths,
        &merged_points.build(),
        &merged_array_rows,
        &merged_bool_fields,
    )
}
/// Merges the selected segments' vectors into one file per field next to `output_path`,
/// dropping deleted documents. Returns the fields that got a file. A failure only loses
/// that field's vectors (the embedding worker rebuilds them); it never fails the compaction.
#[timed(compaction)]
pub fn compact_vectors(
    vectors: &[SegmentVectors],
    deleted: &DeleteSet,
    output_path: &Path,
) -> Vec<XPathId> {
    let mut xpaths: Vec<XPathId> = vectors.iter().map(|entry| entry.xpath).collect();
    xpaths.sort_unstable();
    xpaths.dedup();

    let mut produced = Vec::with_capacity(xpaths.len());
    for xpath in xpaths {
        let inputs: Vec<&VectorFile> = vectors
            .iter()
            .filter(|entry| entry.xpath == xpath)
            .map(|entry| entry.file.as_ref())
            .collect();
        let target = vector_file_path(output_path, xpath);
        match merge_vector_files(&inputs, deleted, &target) {
            Ok(written) if written > 0 => produced.push(xpath),
            Ok(_) | Err(_) => {
                let _ = fs::remove_file(&target);
            }
        }
    }
    produced
}

#[cfg(test)]
mod vector_compaction_tests {
    use super::*;
    use crate::vector::{VectorFile, VectorFileWriter};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("corelamo-vcompact-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(dir: &Path, segment: &str, xpath: XPathId, model: &str, docs: &[DocId]) -> SegmentVectors {
        let segment = dir.join(segment);
        let path = vector_file_path(&segment, xpath);
        let mut w = VectorFileWriter::create(&path, model, 2, 127.0).unwrap();
        for doc in docs {
            w.push(*doc, &[1, 0]).unwrap();
        }
        w.finish().unwrap();
        SegmentVectors { segment, xpath, file: Arc::new(VectorFile::open(&path).unwrap()) }
    }

    #[test]
    fn merges_per_field_and_drops_deleted() {
        let dir = temp_dir("merge");
        let vectors = vec![
            entry(&dir, "segment-1.idx", 3, "m", &[1, 2]),
            entry(&dir, "segment-2.idx", 3, "m", &[5]),
            entry(&dir, "segment-1.idx", 4, "m", &[1]),
        ];
        let mut deleted = DeleteSet::new();
        deleted.delete(2);
        deleted.delete(1);
        let output = dir.join("segment-9.idx");

        let produced = compact_vectors(&vectors, &deleted, &output);
        assert_eq!(produced, vec![3], "field 4 had only deleted docs, so no file");
        let merged = VectorFile::open(vector_file_path(&output, 3)).unwrap();
        assert_eq!((0..merged.len()).map(|i| merged.doc_id(i)).collect::<Vec<_>>(), vec![5]);
        assert!(!vector_file_path(&output, 4).exists());
    }

    #[test]
    fn mixed_models_drop_vectors_instead_of_failing() {
        let dir = temp_dir("mixed");
        let vectors = vec![
            entry(&dir, "segment-1.idx", 3, "old-model", &[1]),
            entry(&dir, "segment-2.idx", 3, "new-model", &[2]),
        ];
        let output = dir.join("segment-9.idx");

        assert!(compact_vectors(&vectors, &DeleteSet::new(), &output).is_empty());
        assert!(!vector_file_path(&output, 3).exists());
        assert!(!dir.join("segment-9.3.vec.tmp").exists());
    }
}


