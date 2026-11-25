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

    pub fn merge(&mut self, other: Self) {
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



#[derive(Debug, Clone, Copy)]
pub enum NumericValue {
    Int(i64),
    Float(f64),
}

impl NumericValue {
        #[allow(clippy::cast_precision_loss, reason = "lossy i64 to f64 is the documented behaviour of as_f64")]
    pub const fn as_f64(self) -> f64 {
        match self {
            Self::Int(v) => v as f64,
            Self::Float(v) => v,
        }
    }

    pub const fn kind(self) -> NumericKind {
        match self {
            Self::Int(_) => NumericKind::Int,
            Self::Float(_) => NumericKind::Float,
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
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Int => 0,
            Self::Float => 1,
        }
    }

    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Int),
            1 => Some(Self::Float),
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
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Float(a), Self::Float(b)) => a.total_cmp(b),
            (Self::Int(i), Self::Float(f)) => cmp_int_float(*i, *f),
            (Self::Float(f), Self::Int(i)) => cmp_int_float(*i, *f).reverse(),
        }
    }
}

//smart funciton to get the exact total order between an integer and a float.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "casts are guarded: fractional floats are below 2^52, whole ones are range-checked first"
)]
fn cmp_int_float(i: i64, f: f64) -> Ordering {
    const TWO_63: f64 = 9_223_372_036_854_776_000.0;
    // NaN sorts last (matches f64::total_cmp's convention)
    if f.is_nan() {
        return Ordering::Less;
    }
    if f.is_infinite() {
        // i < +inf, i > -inf
        return if f.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // A fractional f64 only exists below 2^52, so f64 comparison is exact there.
    if f.fract() != 0.0 {
        return (i as f64).total_cmp(&f);
    }
    // f is a whole number: compare exactly as integers.
    if f >= TWO_63 {
        return Ordering::Less;
    }
    if f < -TWO_63 {
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
pub const fn pack_int(value: i64) -> u64 {
    value.cast_unsigned() ^ (1u64 << 63)
}

pub const fn unpack_int(packed: u64) -> i64 {
    (packed ^ (1u64 << 63)).cast_signed()
}

//negative<positive
pub const fn pack_float(value: f64) -> u64 {
    let bits = value.to_bits();

    if bits & (1u64 << 63) != 0 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    }
}

pub const fn unpack_float(packed: u64) -> f64 {
    let bits = if packed & (1u64 << 63) != 0 {
        packed ^ (1u64 << 63)
    } else {
        !packed
    };

    f64::from_bits(bits)
}

pub const fn pack(value: NumericValue) -> u64 {
    match value {
        NumericValue::Int(v) => pack_int(v),
        NumericValue::Float(v) => pack_float(v),
    }
}

pub const fn unpack(packed: u64, kind: NumericKind) -> NumericValue {
    match kind {
        NumericKind::Int => NumericValue::Int(unpack_int(packed)),
        NumericKind::Float => NumericValue::Float(unpack_float(packed)),
    }
}

//jobani ar +- 0.00
pub fn parse_float(raw: &str) -> Result<NumericValue, &'static str> {
    let value: f64 = raw.trim().parse().map_err(|_err| "not a number")?;
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
