//doc-id > numeric_value FAST
use crate::{
    numeric_values::{NumericKind, NumericValue, pack, unpack},
    types::DocId,
};

const BLOCK_SIZE: usize = 256;
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocValues {
    kind: NumericKind,
    //docid + parsed_value
    entries: Vec<(DocId, u64)>,
    //this is split into blocks so that we can already binary search a 256 item list not a million
    block_first_doc: Vec<DocId>,
}

impl DocValues {
    pub fn from_points(points: Vec<(DocId, NumericValue)>) -> Self {
        let kind = points
            .first()
            .map(|(_, value)| value.kind())
            .unwrap_or_default();

        Self::build(kind, points)
    }

    pub fn build(
        kind: NumericKind,
        entries: impl IntoIterator<Item = (DocId, NumericValue)>,
    ) -> Self {
        let mut entries: Vec<(DocId, u64)> = entries
            .into_iter()
            .map(|(doc_id, value)| (doc_id, pack(value)))
            .collect();

        entries.sort_unstable_by_key(|entry| entry.0);

        let block_first_doc = entries.chunks(BLOCK_SIZE).map(|chunk| chunk[0].0).collect();

        Self {
            kind,
            entries,
            block_first_doc,
        }
    }

    pub fn kind(&self) -> NumericKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn first_doc(&self) -> Option<DocId> {
        self.entries.first().map(|entry| entry.0)
    }

    pub fn last_doc(&self) -> Option<DocId> {
        self.entries.last().map(|entry| entry.0)
    }

    pub fn entries(&self) -> &[(DocId, u64)] {
        &self.entries
    }

    pub fn from_packed(
        kind: NumericKind,
        entries: Vec<(DocId, u64)>,
    ) -> Result<Self, &'static str> {
        //safety check
        if !entries.is_sorted_by_key(|entry| entry.0) {
            return Err("doc values must be sorted by doc id");
        }

        let block_first_doc = entries.chunks(BLOCK_SIZE).map(|chunk| chunk[0].0).collect();
        Ok(Self {
            kind,
            entries,
            block_first_doc,
        })
    }

    pub fn get(&self, doc_id: DocId) -> Option<NumericValue> {
        //fast exit
        if doc_id < self.first_doc()? || doc_id > self.last_doc()? {
            return None;
        }

        //first block whose first doc is <= doc_id only possible block
        let block = self
            .block_first_doc
            .partition_point(|&first| first <= doc_id)
            .saturating_sub(1);

        let start = block * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(self.entries.len());

        self.entries[start..end]
            .binary_search_by_key(&doc_id, |entry| entry.0)
            .ok()
            .map(|offset| unpack(self.entries[start + offset].1, self.kind))
    }

    pub fn values_for(&self, docs: impl IntoIterator<Item = DocId>) -> Vec<(DocId, NumericValue)> {
        docs.into_iter()
            .filter_map(|doc_id| self.get(doc_id).map(|value| (doc_id, value)))
            .collect()
    }
}
