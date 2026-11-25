use roaring::RoaringTreemap;

//K = 1 BKD tree for now, later need something smarter for multi-number values like (lat long)
use crate::{
    numeric_values::{NumericBound, NumericKind, NumericValue, pack, unpack},
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

    //top-k doc ids + values, in the requested order (lowest-first when ascending)
        pub fn top(&self, k: usize, descending: bool) -> Vec<(DocId, NumericValue)> {
        let unpacked = |&(packed, doc_id): &(u64, DocId)| (doc_id, unpack(packed, self.kind));
        if descending {
            self.points.iter().rev().take(k).map(unpacked).collect()
        } else {
            self.points.iter().take(k).map(unpacked).collect()
        }
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

    pub const fn kind(&self) -> NumericKind {
        self.kind
    }

    pub const fn len(&self) -> usize {
        self.points.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    //array of ascending values
    pub fn points(&self) -> &[(u64, DocId)] {
        &self.points
    }

    //for bounds lol
    pub fn min(&self) -> Option<NumericValue> {
        self.points
            .first()
            .map(|(packed, _)| unpack(*packed, self.kind))
    }
    pub fn max(&self) -> Option<NumericValue> {
        self.points
            .last()
            .map(|(packed, _)| unpack(*packed, self.kind))
    }

    //Doc ids whose value lies inside the bounds, ascending and deduped.
        pub fn range(&self, lo: Option<NumericBound>, hi: Option<NumericBound>) -> RoaringTreemap {
        let start = lo.map_or(0, |lo| {
            let bound = pack(lo.value);
            if lo.inclusive {
                self.points.partition_point(|point| point.0 < bound)
            } else {
                self.points.partition_point(|point| point.0 <= bound)
            }
        });

        let end = hi.map_or(self.points.len(), |hi| {
            let bound = pack(hi.value);
            if hi.inclusive {
                self.points.partition_point(|point| point.0 <= bound)
            } else {
                self.points.partition_point(|point| point.0 < bound)
            }
        });

        if start >= end {
            return RoaringTreemap::new();
        }

        // docs are sorted here cuz the original array is sorted by value
        self.points
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|point| point.1)
            .collect()
    }
}
