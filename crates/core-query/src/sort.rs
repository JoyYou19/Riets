use std::cmp::Ordering;
use std::collections::HashMap;

use core_index::types::DocId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Default)]
pub struct DocValues {
    values: HashMap<DocId, String>,
}

impl DocValues {
    pub fn from_pairs(pairs: Vec<(DocId, String)>) -> Self {
        let mut values = HashMap::with_capacity(pairs.len());
        for (doc, value) in pairs {
            values.insert(doc, value);
        }
        Self { values }
    }

    pub fn value_of(&self, doc: DocId) -> Option<&str> {
        self.values.get(&doc).map(|value| value.as_str())
    }
}

pub struct SortableDoc<'a> {
    pub doc_id: DocId,
    pub relevance: f32,
    pub keys: Vec<Option<&'a str>>,
}

impl<'a> SortableDoc<'a> {
    pub fn from_columns(doc_id: DocId, relevance: f32, columns: &[&'a DocValues]) -> Self {
        let keys = columns
            .iter()
            .map(|column| column.value_of(doc_id))
            .collect();
        Self {
            doc_id,
            relevance,
            keys,
        }
    }
}

pub fn compare(a: &SortableDoc, b: &SortableDoc, orders: &[SortOrder]) -> Ordering {
    for (index, order) in orders.iter().enumerate() {
        let outcome = match (&a.keys[index], &b.keys[index]) {
            (Some(x), Some(y)) => match order {
                SortOrder::Asc => x.cmp(y),
                SortOrder::Desc => y.cmp(x),
            },

            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };

        if outcome != Ordering::Equal {
            return outcome;
        }
    }

    //all equal ----> big relevance first then smaller doc id.
    b.relevance
        .partial_cmp(&a.relevance)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.doc_id.cmp(&b.doc_id))
}
