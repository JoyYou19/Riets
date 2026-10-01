 pub mod handle;
 mod immutable;

 pub use handle::SegmentHandle;
 pub use immutable::ImmutableSegment;

 use std::collections::BTreeMap;

 use crate::{
     numeric_values::NumericFields,
     types::{DocId, FieldStats, XPathId},
 };

 /// Compute the min/max DocId present in a segment from both text
 /// (doc_lengths) and numeric-only (BKD points) sources.
 pub fn compute_doc_id_range(
     doc_lengths: &BTreeMap<(DocId, XPathId), u32>,
     numeric_fields: &NumericFields,
 ) -> Option<(DocId, DocId)> {
     let mut range: Option<(DocId, DocId)> = None;

     for &(doc_id, _) in doc_lengths.keys() {
         range = Some(match range {
             None => (doc_id, doc_id),
             Some((min, max)) => (min.min(doc_id), max.max(doc_id)),
         });
     }

     // numeric-only docs don't appear in doc_lengths
     for (_, field) in numeric_fields.iter() {
         for &(_, doc_id) in field.bkd.points() {
             range = Some(match range {
                 None => (doc_id, doc_id),
                 Some((min, max)) => (min.min(doc_id), max.max(doc_id)),
             });
         }
     }

     range
 }

 /// Derive per-field stats from doc_lengths. This is the single source of
 /// truth so that in-memory segments and reopened disk segments agree.
 pub fn build_field_stats(doc_lengths: &BTreeMap<(DocId, XPathId), u32>) ->
 BTreeMap<XPathId, FieldStats> {
     let mut stats: BTreeMap<XPathId, FieldStats> = BTreeMap::new();

     for ((_, xpath), len) in doc_lengths {
         let entry = stats.entry(*xpath).or_default();
         entry.doc_count += 1;
         entry.total_doc_len += *len as u64;
     }

     stats
 }