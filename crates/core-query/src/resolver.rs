//place where MatchSpec -> Query for the query and filters in one place

use std::sync::Arc;

use core_index::{
    analyzer::Analyzer,
    document::{
        IndexPolicy,
        policy::{FieldKind, FieldPolicy},
    },
    fuzzy::{DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec},
    numeric_columns::{
        NumericBound, NumericValue, parse_float, parse_integer, parse_numeric_range,
    },
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

pub fn compile_query(
    spec: &MatchSpec,
    analyzer: &Analyzer,
    policy: &IndexPolicy,
) -> Result<(Option<Query>, Arc<Vec<XPathId>>), CorelamoError> {
    let query = text_query(spec, analyzer)?;
    let xpaths: Vec<XPathId> = match spec {
        MatchSpec::Exact(_) => policy.exact_xpaths().collect(),
        _ => policy.searchable_xpaths().collect(),
    };
    Ok((query, Arc::new(xpaths)))
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
