//K = 1 BKD tree for now, later need something smarter for multi-number values like (lat long)
use crate::{
    numeric_values::{NumericBound, NumericKind, NumericValue, pack},
    types::DocId,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bkd {
    kind: NumericKind,
    points: Vec<(u64, DocId)>,
}

impl Bkd {
    pub fn build(
        kind: NumericKind,
        points: impl IntoIterator<Item = (NumericValue, DocId)>,
    ) -> Self {
        let mut points: Vec<(u64, DocId)> = points
            .into_iter()
            .map(|(value, doc_id)| (pack(value), doc_id))
            .collect();

        points.sort_unstable();

        Self { kind, points }
    }

    pub fn from_packed(kind: NumericKind, points: Vec<(u64, DocId)>) -> Result<Self, &'static str> {
        //safety
        if !points.is_sorted_by_key(|point| point.0) {
            return Err("bkd points must be sorted by packed value");
        }
        Ok(Self { kind, points })
    }

    pub fn from_points(points: Vec<(DocId, NumericValue)>) -> Self {
        let kind = points
            .first()
            .map(|(_, value)| value.kind())
            .unwrap_or_default();

        Self::build(
            kind,
            points.into_iter().map(|(doc_id, value)| (value, doc_id)),
        )
    }

    pub fn kind(&self) -> NumericKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn min_value(&self) -> Option<u64> {
        self.points.first().map(|point| point.0)
    }

    pub fn max_value(&self) -> Option<u64> {
        self.points.last().map(|point| point.0)
    }

    //array of ascending values
    pub fn points(&self) -> &[(u64, DocId)] {
        &self.points
    }

    //Doc ids whose value lies inside the bounds, ascending and deduped.
    pub fn range(&self, lo: Option<NumericBound>, hi: Option<NumericBound>) -> Vec<DocId> {
        let start = match lo {
            Some(lo) => {
                let bound = pack(lo.value);
                if lo.inclusive {
                    self.points.partition_point(|point| point.0 < bound)
                } else {
                    self.points.partition_point(|point| point.0 <= bound)
                }
            }
            None => 0,
        };

        let end = match hi {
            Some(hi) => {
                let bound = pack(hi.value);
                if hi.inclusive {
                    self.points.partition_point(|point| point.0 <= bound)
                } else {
                    self.points.partition_point(|point| point.0 < bound)
                }
            }
            None => self.points.len(),
        };

        if start >= end {
            return Vec::new();
        }

        let mut docs: Vec<DocId> = self.points[start..end]
            .iter()
            .map(|point| point.1)
            .collect();

        // docs are sorted here cuz the original array is sorted by value
        docs.sort_unstable();
        docs.dedup();
        docs
    }
}
