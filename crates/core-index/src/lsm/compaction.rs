use std::{collections::BTreeMap, path::PathBuf};

use crate::{
    posting::{ops::union, DeleteSet, PostingList},
    segment::{ImmutableSegment, SegmentHandle},
    types::TermKey,
};

// WARN: Compacts and merges segments together, this desperately needs to be udpated to a smarter system
pub fn compact_segments(segments: &[ImmutableSegment], deleted: &DeleteSet) -> ImmutableSegment {
    let mut merged: BTreeMap<TermKey, PostingList> = BTreeMap::new();

    for segment in segments {
        for (key, postings) in segment.terms() {
            let postings = deleted.filter(&postings);

            if postings.is_empty() {
                continue;
            }

            merged
                .entry(key.clone())
                .and_modify(|existing| {
                    *existing = deleted.filter(&union(existing, &postings));
                })
                .or_insert_with(|| postings.clone());
        }
    }

    ImmutableSegment::new(merged)
}

#[derive(Debug, Clone, Copy)]
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
