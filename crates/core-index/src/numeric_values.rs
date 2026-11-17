//numbers helpers - defines what types would be stored for what types + helpers
//TODO: coordinates, points.... would need K>1 BKD logic but for now i think this will do
//
//INFO: used by bkd/document_values
use std::{cmp::Ordering, num::IntErrorKind};

use std::collections::BTreeMap;

use roaring::RoaringTreemap;

use crate::{
    bkd::Bkd,
    document_values::DocValues,
    types::{DocId, XPathId},
};

//bkd+values for a single xpath
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumericField {
    pub bkd: Bkd,
    pub doc_values: DocValues,
}

impl NumericField {
    pub fn bounds(&self) -> (Option<NumericValue>, Option<NumericValue>) {
        (self.bkd.min(), self.bkd.max())
    }
}

//every numeic value for a segment per xpath
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumericFields {
    fields: BTreeMap<XPathId, NumericField>,
}

impl NumericFields {
    pub fn from_points(points: BTreeMap<XPathId, Vec<(DocId, NumericValue)>>) -> Self {
        Self {
            fields: points
                .into_iter()
                .map(|(xpath, points)| {
                    (
                        xpath,
                        NumericField {
                            bkd: Bkd::from_points(points.clone()),
                            doc_values: DocValues::from_points(points),
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn top(&self, xpath: XPathId, k: usize, descending: bool) -> Vec<(DocId, NumericValue)> {
        self.field(xpath)
            .map(|field| field.bkd.top(k, descending))
            .unwrap_or_default()
    }

    pub fn bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        self.field(xpath).map_or((None, None), NumericField::bounds)
    }

    pub fn field(&self, xpath: XPathId) -> Option<&NumericField> {
        self.fields.get(&xpath)
    }

    //range for our NumericFields
    pub fn range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> RoaringTreemap {
        self.field(xpath)
            .map(|field| field.bkd.range(lo, hi))
            .unwrap_or_default()
    }

    pub fn insert_field(&mut self, xpath: XPathId, field: NumericField) {
        self.fields.insert(xpath, field);
    }

    //docid->value for the NumericFields
    pub fn get(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.field(xpath)?.doc_values.get(doc_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (XPathId, &NumericField)> + '_ {
        self.fields.iter().map(|(&xpath, field)| (xpath, field))
    }
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

//sits only in ram, cause documents come in docid order so btree map is faster in ram for ranges n
//shit, at flush time we sort this and convert to BKD + DocValues
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumericPoints {
    points: BTreeMap<XPathId, Vec<(DocId, NumericValue)>>,
}

impl NumericPoints {
    pub fn insert(&mut self, xpath: XPathId, value: NumericValue, doc_id: DocId) {
        let points = self.points.entry(xpath).or_default();
        // kept in document order (documents arrive in it, so this is a push) so `get` can
        // binary search
        match points.last() {
            Some(&(last, _)) if last > doc_id => {
                let at = points.partition_point(|&(doc, _)| doc <= doc_id);
                points.insert(at, (doc_id, value));
            }
            _ => points.push((doc_id, value)),
        }
    }

    pub fn build(self) -> NumericFields {
        NumericFields::from_points(self.points)
    }

    pub fn top(&self, xpath: XPathId, k: usize, descending: bool) -> Vec<(DocId, NumericValue)> {
        let Some(points) = self.points.get(&xpath) else {
            return Vec::new();
        };
        let mut sorted: Vec<(DocId, NumericValue)> = points.clone();
        sorted.sort_by_key(|(_, v)| *v);
        if descending {
            sorted.reverse();
        }
        sorted.truncate(k);
        sorted
    }

    pub fn get(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        let points = self.points.get(&xpath)?;
        let at = points.partition_point(|&(doc, _)| doc < doc_id);
        points
            .get(at)
            .filter(|(doc, _)| *doc == doc_id)
            .map(|(_, value)| *value)
    }

    pub fn range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> RoaringTreemap {
        let Some(points) = self.points.get(&xpath) else {
            return RoaringTreemap::new();
        };

        let mut out = RoaringTreemap::new();
        for &(doc_id, value) in points {
            if in_bounds(value, lo, hi) {
                out.insert(doc_id);
            }
        }
        out
    }

    pub fn merge(&mut self, other: NumericPoints) {
        for (xpath, mut points) in other.points {
            let mine = self.points.entry(xpath).or_default();
            mine.append(&mut points);
            // still in document order for `get`
            if !mine.is_sorted_by_key(|&(doc, _)| doc) {
                mine.sort_by_key(|&(doc, _)| doc);
            }
        }
    }

    pub fn bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        let Some(points) = self.points.get(&xpath) else {
            return (None, None);
        };
        let vals = points.iter().map(|(_, v)| *v);
        (vals.clone().min(), vals.max())
    }
}

impl NumericPoints {}

#[derive(Debug, Clone, Copy)]
pub enum NumericValue {
    Int(i64),
    Float(f64),
}

impl NumericValue {
    pub fn as_f64(self) -> f64 {
        match self {
            NumericValue::Int(v) => v as f64,
            NumericValue::Float(v) => v,
        }
    }

    pub fn kind(self) -> NumericKind {
        match self {
            NumericValue::Int(_) => NumericKind::Int,
            NumericValue::Float(_) => NumericKind::Float,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumericKind {
    #[default]
    Int,
    Float,
}

//helper for writing reading to disk
impl NumericKind {
    pub fn to_byte(self) -> u8 {
        match self {
            NumericKind::Int => 0,
            NumericKind::Float => 1,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(NumericKind::Int),
            1 => Some(NumericKind::Float),
            _ => None,
        }
    }
}

impl PartialEq for NumericValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for NumericValue {}

impl PartialOrd for NumericValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NumericValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (NumericValue::Int(a), NumericValue::Int(b)) => a.cmp(b),
            (NumericValue::Float(a), NumericValue::Float(b)) => a.total_cmp(b),
            (NumericValue::Int(i), NumericValue::Float(f)) => cmp_int_float(*i, *f),
            (NumericValue::Float(f), NumericValue::Int(i)) => cmp_int_float(*i, *f).reverse(),
        }
    }
}

//smart funciton to get the exact total order between an integer and a float.
fn cmp_int_float(i: i64, f: f64) -> Ordering {
    // NaN sorts last (matches f64::total_cmp's convention)
    if f.is_nan() {
        return Ordering::Less;
    }
    if f == f64::INFINITY {
        return Ordering::Less; // i < +inf
    }
    if f == f64::NEG_INFINITY {
        return Ordering::Greater; // i > -inf
    }

    // f is finite. A *fractional* f64 can only exist below 2^52, so an integer
    // is either exactly representable there or far away — f64 comparison is exact.
    if f.fract() != 0.0 {
        return (i as f64).total_cmp(&f);
    }

    // f is a whole number: compare exactly as integers.
    if f > i64::MAX as f64 {
        return Ordering::Less;
    }
    if f < i64::MIN as f64 {
        return Ordering::Greater;
    }

    i.cmp(&(f as i64))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumericBound {
    pub value: NumericValue,
    pub inclusive: bool,
}

impl NumericBound {
    pub fn below_lo(self, value: NumericValue) -> bool {
        match value.cmp(&self.value) {
            Ordering::Less => true,
            Ordering::Equal => !self.inclusive,
            Ordering::Greater => false,
        }
    }
    pub fn past_hi(self, value: NumericValue) -> bool {
        match value.cmp(&self.value) {
            Ordering::Greater => true,
            Ordering::Equal => !self.inclusive,
            Ordering::Less => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NumericRange {
    pub lo: Option<NumericBound>,
    pub hi: Option<NumericBound>,
}

pub fn parse_integer(raw: &str) -> Result<NumericValue, &'static str> {
    match raw.trim().parse::<i64>() {
        Ok(v) => Ok(NumericValue::Int(v)),
        Err(e) => match e.kind() {
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => Err("out of i64 range"),
            _ => Err("not an integer"),
        },
    }
}

// so that i64 sorts the same way that u64 does
pub fn pack_int(value: i64) -> u64 {
    (value as u64) ^ (1u64 << 63)
}

pub fn unpack_int(packed: u64) -> i64 {
    (packed ^ (1u64 << 63)) as i64
}

//negative<positive
pub fn pack_float(value: f64) -> u64 {
    let bits = value.to_bits();

    if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    }
}

pub fn unpack_float(packed: u64) -> f64 {
    let bits = if packed & (1u64 << 63) != 0 {
        packed ^ (1u64 << 63)
    } else {
        !packed
    };

    f64::from_bits(bits)
}

pub fn pack(value: NumericValue) -> u64 {
    match value {
        NumericValue::Int(v) => pack_int(v),
        NumericValue::Float(v) => pack_float(v),
    }
}

pub fn unpack(packed: u64, kind: NumericKind) -> NumericValue {
    match kind {
        NumericKind::Int => NumericValue::Int(unpack_int(packed)),
        NumericKind::Float => NumericValue::Float(unpack_float(packed)),
    }
}

//jobani ar +- 0.00
pub fn parse_float(raw: &str) -> Result<NumericValue, &'static str> {
    let value: f64 = raw.trim().parse().map_err(|_| "not a number")?;
    if !value.is_finite() {
        return Err("out of f64 range");
    }
    let value = if value == 0.0 { 0.0 } else { value };
    Ok(NumericValue::Float(value))
}

pub fn in_bounds(value: NumericValue, lo: Option<NumericBound>, hi: Option<NumericBound>) -> bool {
    lo.is_none_or(|lo| !lo.below_lo(value)) && hi.is_none_or(|hi| !hi.past_hi(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_53: i64 = 1i64 << 53; // 9007199254740992

    #[test]
    fn int_cmp_is_exact_above_2_53() {
        let a = NumericValue::Int(TWO_53);
        let b = NumericValue::Int(TWO_53 + 1);
        assert_eq!(a.cmp(&b), Ordering::Less);
        assert_eq!(b.cmp(&a), Ordering::Greater);
        assert_ne!(a, b);
    }

    #[test]
    fn mixed_int_float_still_compares_as_float() {
        assert_eq!(
            NumericValue::Int(2).cmp(&NumericValue::Float(1.5)),
            Ordering::Greater
        );
    }

    #[test]
    fn range_matches_bkd_around_2_53() {
        let xpath: XPathId = 0;
        let points: Vec<(DocId, NumericValue)> = vec![
            (1, NumericValue::Int(TWO_53)),
            (2, NumericValue::Int(TWO_53 + 1)),
            (3, NumericValue::Int(TWO_53 + 2)),
        ];

        // before flush: in-memory points (compare via cmp / in_bounds)
        let mut mem = NumericPoints::default();
        for &(doc_id, value) in &points {
            mem.insert(xpath, value, doc_id);
        }

        // after flush: BKD (compares exact packed i64)
        let fields = NumericFields::from_points(std::iter::once((xpath, points)).collect());

        // exact filter: value == 2^53 + 1
        let bound = NumericBound {
            value: NumericValue::Int(TWO_53 + 1),
            inclusive: true,
        };
        let mem_hits = mem.range(xpath, Some(bound), Some(bound));
        let bkd_hits = fields.range(xpath, Some(bound), Some(bound));

        assert_eq!(mem_hits, bkd_hits, "mem and BKD disagree around 2^53");
        assert!(mem_hits.contains(2));
        assert!(!mem_hits.contains(1));
        assert!(!mem_hits.contains(3));
    }
}
