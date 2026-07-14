use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    posting::{DeleteSet, PostingList, ops::union},
    segment::{ImmutableSegment, SegmentHandle},
    types::{DocId, FieldStats, TermKey, XPathId},
};

// WARN: Compacts and merges segments together, this desperately needs to be udpated to a smarter system
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
            let postings = deleted.filter(&postings);

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
