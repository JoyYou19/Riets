use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::types::{DocId, XPathId};

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
    // true when `value` falls below this (lower) bound and must be skipped
    pub fn below_lo(self, value: NumericValue) -> bool {
        match value.cmp(&self.value) {
            Ordering::Less => true,
            Ordering::Equal => !self.inclusive,
            Ordering::Greater => false,
        }
    }

    // true when `value` falls past this (upper) bound and the scan can stop
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

//jobani ar +- 0.00
pub fn parse_float(raw: &str) -> Option<NumericValue> {
    let value: f64 = raw.trim().parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    // normalize -0.0 == 0.0 so Eq/Ord stay consistent
    let value = if value == 0.0 { 0.0 } else { value };
    Some(NumericValue::Float(value))
}

fn has_op_prefix(t: &str) -> bool {
    t.starts_with('=') || t.starts_with('>') || t.starts_with('<')
}

//vibemaxxing funciton: parsing the query for the foken numbres
pub fn parse_numeric_range(
    raw: &str,
    parse: fn(&str) -> Option<NumericValue>,
) -> Result<NumericRange, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty numeric filter".to_string());
    }

    if let Some(idx) = s.find("..") {
        let left = s[..idx].trim();
        let right = s[idx + 2..].trim();
        if has_op_prefix(left) || has_op_prefix(right) {
            return Err(format!(
                "comparison operators can't be combined with '..' (use '30..40', or '>40'): '{s}'"
            ));
        }
        if left.is_empty() || right.is_empty() {
            return Err(format!(
                "'..' requires both bounds, e.g. '30..40' (use '>=30' or '<=40' for one-sided ranges): '{s}'"
            ));
        }
        let lo = NumericBound {
            value: parse(left).ok_or_else(|| format!("invalid lower bound '{left}'"))?,
            inclusive: true,
        };
        let hi = NumericBound {
            value: parse(right).ok_or_else(|| format!("invalid upper bound '{right}'"))?,
            inclusive: true,
        };
        if lo.value > hi.value {
            return Err(format!(
                "empty range: lower '{left}' is greater than upper '{right}'"
            ));
        }
        return Ok(NumericRange {
            lo: Some(lo),
            hi: Some(hi),
        });
    }

    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = s.strip_prefix('=') {
        ("=", r)
    } else {
        ("", s)
    };

    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("missing number after '{op}'"));
    }
    let value = parse(rest).ok_or_else(|| format!("invalid number '{rest}'"))?;

    let range = match op {
        "=" | "==" | "" => {
            let bound = NumericBound {
                value,
                inclusive: true,
            };
            NumericRange {
                lo: Some(bound),
                hi: Some(bound),
            }
        }
        ">=" => NumericRange {
            lo: Some(NumericBound {
                value,
                inclusive: true,
            }),
            hi: None,
        },
        ">" => NumericRange {
            lo: Some(NumericBound {
                value,
                inclusive: false,
            }),
            hi: None,
        },
        "<=" => NumericRange {
            lo: None,
            hi: Some(NumericBound {
                value,
                inclusive: true,
            }),
        },
        "<" => NumericRange {
            lo: None,
            hi: Some(NumericBound {
                value,
                inclusive: false,
            }),
        },
        _ => return Err(format!("unknown operator in '{s}'")),
    };
    Ok(range)
}

//all values of a single xpath in a column, keyed by value (ascending) so
//range filters are O(log n + k) and sorting walks values in order
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumericColumn {
    by_value: BTreeMap<NumericValue, Vec<DocId>>,
}

impl NumericColumn {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, value: NumericValue, doc_id: DocId) {
        let docs = self.by_value.entry(value).or_default();
        match docs.binary_search(&doc_id) {
            Ok(_) => {}
            Err(index) => docs.insert(index, doc_id),
        }
    }

    //doc ids whose value sit inside the [lo, hi] asc and deduped.
    pub fn range(&self, lo: Option<NumericBound>, hi: Option<NumericBound>) -> Vec<DocId> {
        let mut out = Vec::new();
        for (&value, docs) in &self.by_value {
            if let Some(lo) = lo {
                if lo.below_lo(value) {
                    continue;
                }
            }
            if let Some(hi) = hi {
                if hi.past_hi(value) {
                    break;
                }
            }
            out.extend_from_slice(docs);
        }
        out
    }

    pub fn entries(&self) -> impl Iterator<Item = (NumericValue, DocId)> + '_ {
        self.by_value
            .iter()
            .flat_map(|(&value, docs)| docs.iter().map(move |&doc_id| (value, doc_id)))
    }

    pub fn len(&self) -> usize {
        self.by_value.values().map(|docs| docs.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_value.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumericColumns {
    columns: BTreeMap<XPathId, NumericColumn>,
}

impl NumericColumns {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, xpath: XPathId, value: NumericValue, doc_id: DocId) {
        self.columns.entry(xpath).or_default().insert(value, doc_id);
    }

    pub fn column(&self, xpath: XPathId) -> Option<&NumericColumn> {
        self.columns.get(&xpath)
    }

    pub fn range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> Vec<DocId> {
        self.column(xpath)
            .map(|column| column.range(lo, hi))
            .unwrap_or_default()
    }

    pub fn iter(&self) -> impl Iterator<Item = (XPathId, &NumericColumn)> + '_ {
        self.columns.iter().map(|(&xpath, column)| (xpath, column))
    }

    pub fn xpaths(&self) -> impl Iterator<Item = XPathId> + '_ {
        self.columns.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.columns.values().map(|column| column.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}
