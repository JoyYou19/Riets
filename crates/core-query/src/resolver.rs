//place where MatchSpec -> Query for the query and filters in one place

use std::sync::Arc;

use core_index::{
    analyzer::Analyzer,
    document::{
        IndexPolicy,
        policy::{FieldKind, FieldPolicy},
    },
    fuzzy::{DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec},
    numeric_values::{NumericBound, NumericRange, NumericValue, parse_float, parse_integer},
    types::XPathId,
};
use core_protocol::{
    command_reponse_definitions::{Fuzziness, MatchSpec},
    errors::CorelamoError,
};

use crate::{Query, executor::FieldFilter, query_string_parser::parse_and_analyze};

#[derive(Debug, Clone)]
pub enum MatchOp {
    Query(Option<Query>),
    Range {
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    },
}

pub fn parse_fuzziness(raw: Option<&str>) -> Result<Fuzziness, CorelamoError> {
    let Some(raw) = raw else {
        return Ok(Fuzziness::Auto);
    };

    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(Fuzziness::Auto),
        "0" => Ok(Fuzziness::Zero),
        "1" => Ok(Fuzziness::One),
        "2" => Ok(Fuzziness::Two),
        other => Err(CorelamoError::InvalidData(format!(
            "fuzziness must be \"auto\", 0, 1 or 2, got '{other}'"
        ))),
    }
}

pub fn compile_query(
    spec: &MatchSpec,
    search_fields: Option<&[String]>,
    analyzer: &Analyzer,
    policy: &IndexPolicy,
) -> Result<(Option<Query>, Arc<Vec<XPathId>>), CorelamoError> {
    let query = text_query(spec, analyzer)?;
    let xpaths: Vec<XPathId> = match search_fields {
        Some(names) => resolve_search_xpaths(names, spec, policy)?,
        None => match spec {
            MatchSpec::Exact(_) => policy.exact_xpaths().collect(),
            _ => policy.searchable_xpaths().collect(),
        },
    };
    Ok((query, Arc::new(xpaths)))
}

fn resolve_search_xpaths(
    names: &[String],
    spec: &MatchSpec,
    policy: &IndexPolicy,
) -> Result<Vec<XPathId>, CorelamoError> {
    let mut out: Vec<XPathId> = Vec::with_capacity(names.len());

    for name in names {
        let field = policy
            .fields
            .iter()
            .find(|f| &f.name == name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        if !field.searchable() && !matches!(spec, MatchSpec::Exact(_)) {
            return Err(CorelamoError::InvalidData(format!(
                "field '{name}' is not searchable (add 'searchable = true' to its policy and reindex the database)"
            )));
        }

        let xpath = text_xpath(spec, policy, field)?;

        if !out.contains(&xpath) {
            out.push(xpath);
        }
    }

    Ok(out)
}

//basically the same as the resolve_search_xpaths but without exact matching cuz its fuzzy
pub fn resolve_suggest_xpaths(
    names: &[String],
    policy: &IndexPolicy,
) -> Result<Vec<XPathId>, CorelamoError> {
    let mut out: Vec<XPathId> = Vec::with_capacity(names.len());

    for name in names {
        let field = policy
            .fields
            .iter()
            .find(|f| &f.name == name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        if !field.searchable() {
            return Err(CorelamoError::InvalidData(format!(
                "field '{name}' is not searchable (add 'searchable = true' to its policy)"
            )));
        }

        let xpath = field.xpath(policy);

        if !out.contains(&xpath) {
            out.push(xpath);
        }
    }

    Ok(out)
}

pub fn compile_field_filter(
    field_name: &str,
    spec: &MatchSpec,
    analyzer: &Analyzer,
    policy: &IndexPolicy,
) -> Result<Option<FieldFilter>, CorelamoError> {
    let field = policy
        .fields
        .iter()
        .find(|f| f.name == field_name)
        .ok_or_else(|| CorelamoError::PathNotIndexed(field_name.to_string()))?;

    if spec.is_blank() {
        return Ok(None);
    }

    match field.kind {
        FieldKind::Integer | FieldKind::Float => {
            let MatchSpec::Plain(term) = spec else {
                return Err(CorelamoError::InvalidData(format!(
                    "field '{field_name}' is numeric: use a range like '30..40', '>2010' or '<=5', \
                     not exact/fuzzy matching"
                )));
            };

            let parse: fn(&str) -> Option<NumericValue> = match field.kind {
                FieldKind::Integer => parse_integer,
                _ => parse_float,
            };
            let range = parse_numeric_range(term, parse).map_err(|e| {
                CorelamoError::InvalidData(format!(
                    "invalid filter '{term}' on numeric field '{field_name}': {e}"
                ))
            })?;

            Ok(Some(FieldFilter {
                xpath: field.xpath(policy),
                kind: MatchOp::Range {
                    lo: range.lo,
                    hi: range.hi,
                },
            }))
        }

        // text fields: same semantics as the global query, on one xpath
        FieldKind::Text => Ok(Some(FieldFilter {
            xpath: text_xpath(spec, policy, field)?,
            kind: MatchOp::Query(text_query(spec, analyzer)?),
        })),

        // unchanged: None / Date / Id / IdAuto are not filterable
        _ => Err(CorelamoError::PathNotIndexed(field_name.to_string())),
    }
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

fn has_op_prefix(t: &str) -> bool {
    t.starts_with('=') || t.starts_with('>') || t.starts_with('<')
}

fn text_xpath(
    spec: &MatchSpec,
    policy: &IndexPolicy,
    field: &FieldPolicy,
) -> Result<XPathId, CorelamoError> {
    match spec {
        MatchSpec::Exact(_) => field.exact_xpath(policy).ok_or_else(|| {
            CorelamoError::InvalidData(format!(
                "field '{}' has no exact index (add 'exact = true' to its policy)",
                field.name
            ))
        }),
        _ => Ok(field.xpath(policy)),
    }
}

fn text_query(spec: &MatchSpec, analyzer: &Analyzer) -> Result<Option<Query>, CorelamoError> {
    Ok(match spec {
        MatchSpec::Plain(raw) => parse_and_analyze(raw, analyzer)?,
        MatchSpec::Exact(value) => Some(Query::Exact(value.clone())),
        MatchSpec::Fuzzy {
            value,
            fuzziness,
            prefix_length,
            max_expansions,
        } => Some(Query::Fuzzy(
            value.clone(),
            fuzziness.unwrap_or(Fuzziness::Auto),
            FuzzySpec {
                prefix_length: prefix_length.unwrap_or(DEFAULT_PREFIX_LENGTH),
                max_expansions: max_expansions.unwrap_or(DEFAULT_MAX_EXPANSIONS),
            },
        )),
    })
}
