//numbers helpers - defines what types would be stored for what types + helpers
//TODO: coordinates, points.... would need K>1 BKD logic but for now i think this will do
//
//INFO: used by bkd/document_values
use std::cmp::Ordering;

use std::collections::BTreeMap;

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

    pub fn field(&self, xpath: XPathId) -> Option<&NumericField> {
        self.fields.get(&xpath)
    }

    //range for our NumericFields
    pub fn range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> Vec<DocId> {
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

    //all values for this xpath in this segment
    pub fn values(&self, xpath: XPathId) -> Vec<(DocId, NumericValue)> {
        self.field(xpath)
            .map(|field| {
                field
                    .doc_values
                    .entries()
                    .iter()
                    .map(|&(doc_id, packed)| (doc_id, unpack(packed, field.doc_values.kind())))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn iter(&self) -> impl Iterator<Item = (XPathId, &NumericField)> + '_ {
        self.fields.iter().map(|(&xpath, field)| (xpath, field))
    }

    pub fn len(&self) -> usize {
        self.fields.values().map(|field| field.bkd.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.values().all(|field| field.bkd.is_empty())
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
        self.points.entry(xpath).or_default().push((doc_id, value));
    }

    pub fn build(self) -> NumericFields {
        NumericFields::from_points(self.points)
    }

    pub fn get(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.points
            .get(&xpath)?
            .iter()
            .find(|(d, _)| *d == doc_id)
            .map(|(_, value)| *value)
    }

    pub fn range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> Vec<DocId> {
        let Some(points) = self.points.get(&xpath) else {
            return Vec::new();
        };

        let mut docs: Vec<DocId> = points
            .iter()
            .filter(|(_, value)| in_bounds(*value, lo, hi))
            .map(|(doc_id, _)| *doc_id)
            .collect();

        docs.sort_unstable();
        docs.dedup();
        docs
    }

    pub fn values(&self, xpath: XPathId) -> Vec<(DocId, NumericValue)> {
        self.points.get(&xpath).cloned().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.points.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.points.values().all(Vec::is_empty)
    }
}

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
            //this is kind of wrong but a field would never hold both int and float
            (NumericValue::Int(_), NumericValue::Float(_)) => Ordering::Less,
            (NumericValue::Float(_), NumericValue::Int(_)) => Ordering::Greater,
        }
    }
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

pub fn parse_integer(raw: &str) -> Option<NumericValue> {
    raw.trim().parse::<i64>().ok().map(NumericValue::Int)
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
pub fn parse_float(raw: &str) -> Option<NumericValue> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    //lai vienadi
    let value = if value == 0.0 { 0.0 } else { value };
    Some(NumericValue::Float(value))
}

pub fn in_bounds(value: NumericValue, lo: Option<NumericBound>, hi: Option<NumericBound>) -> bool {
    lo.is_none_or(|lo| !lo.below_lo(value)) && hi.is_none_or(|hi| !hi.past_hi(value))
}
