use std::{
    collections::BTreeMap,
    io,
    iter::Peekable,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{
    disk::{reader::DiskSegment, writer::write_merged_segment},
    posting::{DeleteSet, PostingList, ops::union},
    segment::{ImmutableSegment, SegmentHandle},
    types::{DocId, FieldStats, TermKey, XPathId},
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
    sources: Vec<Peekable<TermIter<'a>>>,
    deleted: &'a DeleteSet,
}

impl<'a> Iterator for MergedTerms<'a> {
    type Item = (TermKey, PostingList);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let min_key = self
                .sources
                .iter_mut()
                .filter_map(|s| s.peek().map(|(k, _)| k.clone()))
                .min()?;

            let mut items = Vec::new();
            for source in self.sources.iter_mut() {
                if let Some((k, _)) = source.peek() {
                    if *k == min_key {
                        let (_, postings) = source.next().unwrap();
                        items.extend(postings.items().iter().cloned());
                    }
                }
            }

            let merged = self.deleted.filter(&PostingList::from_items(items));
            if !merged.is_empty() {
                return Some((min_key, merged));
            }
            // every posting for this term was tombstoned — keep scanning
        }
    }
}

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

    let sources: Vec<Peekable<TermIter<'_>>> =
        opened.iter().map(|s| s.iter_terms().peekable()).collect();

    let merged_terms = MergedTerms { sources, deleted };

    write_merged_segment(output_path, merged_terms, &merged_doc_lengths)
}

// WARN: Compacts and merges segments together, this desperately needs to be udpated to a smarter system - Valtero Meero
pub fn compact_segments(segments: &[ImmutableSegment], deleted: &DeleteSet) -> ImmutableSegment {
    let mut merged: BTreeMap<TermKey, PostingList> = BTreeMap::new();
    let mut merged_doc_lengths: BTreeMap<(DocId, XPathId), u32> = BTreeMap::new();

    for segment in segments {
        for (&(doc_id, xpath), &len) in segment.doc_lengths() {
            if deleted.contains(doc_id) {
                continue;
            }

            merged_doc_lengths.insert((doc_id, xpath), len);
        }
        for (key, postings) in segment.terms() {
            // First we check if the posting is not already deleted
            let postings = deleted.filter(postings);

            if postings.is_empty() {
                continue;
            }

            // TODO: Once again, this might not be optimal
            merged
                .entry(key.clone())
                .and_modify(|existing| {
                    *existing = deleted.filter(&union(existing, &postings));
                })
                .or_insert_with(|| postings.clone());
        }
    }

    let merged_field_stats = build_field_stats(&merged_doc_lengths);

    ImmutableSegment::new(merged, merged_doc_lengths, merged_field_stats)
}

fn build_field_stats(
    doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
) -> BTreeMap<XPathId, FieldStats> {
    let mut stats = BTreeMap::<XPathId, FieldStats>::new();

    for ((_, xpath), len) in doc_lengths {
        let entry = stats.entry(*xpath).or_default();
        entry.doc_count += 1;
        entry.total_doc_len += *len as u64;
    }

    stats
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub max_segments_per_compaction: usize,
    pub compact_when_segments_at_least: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_segments_per_compaction: 8,
            compact_when_segments_at_least: 16,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionJob {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub deleted: DeleteSet,
    pub output_path: PathBuf,
}

#[derive(Debug)]
pub struct CompletedCompaction {
    pub job_id: u64,
    pub selected: Vec<SegmentHandle>,
    pub output_path: PathBuf,
}
