use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::document::policy::IndexKind;
use core_index::numbers::{decode_f64, decode_i64, float_term, integer_term, parse_filter};
use core_index::types::XPathId;
use core_protocol::command_reponse_definitions::{SearchCommand, SortOrderRequest};
use core_protocol::errors::CorelamoError;
use core_query::SearchHit;
use core_query::executor::{FieldFilter, FieldFilterKind};
use core_query::query_string_parser::parse_and_analyze;
use std::collections::HashMap;
use std::sync::Arc;

pub struct SortFieldSpec {
    pub xpath: XPathId,
    pub order: SortOrderRequest,
    pub ratio: u8,
    pub is_float: bool,
}

pub fn resolve_filters(
    analyzer: &Analyzer,
    command: &SearchCommand,
    policy: &IndexPolicy,
) -> Result<Option<Arc<HashMap<String, FieldFilter>>>, CorelamoError> {
    match command.filters.as_ref() {
        Some(fs) => {
            let mut resolved = HashMap::with_capacity(fs.len());
            for (field, term) in fs {
                if term.trim().is_empty() {
                    continue;
                }

                let field_pol = policy
                    .fields
                    .iter()
                    .find(|f| &f.name == field)
                    .ok_or_else(|| CorelamoError::PathNotIndexed(field.clone()))?;

                let kind = match field_pol.index {
                    //old behavior with text
                    IndexKind::Text => FieldFilterKind::Text(parse_and_analyze(term, analyzer)?),
                    //numeric >40  >=40  <50  <=50  =20  30..40
                    IndexKind::Integer => {
                        let range = parse_filter(term, integer_term).map_err(|e| {
                            CorelamoError::InvalidData(format!(
                                "invalid filter '{term}' on numeric field '{field}': {e}"
                            ))
                        })?;
                        FieldFilterKind::Range {
                            lo: range.lo,
                            hi: range.hi,
                        }
                    }
                    IndexKind::Float => {
                        let range = parse_filter(term, float_term).map_err(|e| {
                            CorelamoError::InvalidData(format!(
                                "invalid filter '{term}' on numeric field '{field}': {e}"
                            ))
                        })?;
                        FieldFilterKind::Range {
                            lo: range.lo,
                            hi: range.hi,
                        }
                    }
                    _ => return Err(CorelamoError::PathNotIndexed(field.clone())),
                };

                resolved.insert(
                    field.clone(),
                    FieldFilter {
                        xpath: field_pol.xpath(&policy),
                        kind,
                    },
                );
            }
            Ok(Some(Arc::new(resolved)))
        }
        None => Ok(None),
    }
}

pub fn resolve_sorts(
    command: &SearchCommand,
    policy: &IndexPolicy,
) -> Result<Option<Arc<Vec<SortFieldSpec>>>, CorelamoError> {
    let Some(requests) = command.sort.as_ref() else {
        return Ok(None);
    };
    if requests.is_empty() {
        return Ok(None);
    }

    let mut sorts = Vec::with_capacity(requests.len());
    for (field, spec) in requests {
        let field_pol = policy
            .fields
            .iter()
            .find(|f| &f.name == field)
            .ok_or_else(|| CorelamoError::PathNotIndexed(field.clone()))?;

        let is_float = match field_pol.index {
            IndexKind::Integer => false,
            IndexKind::Float => true,
            _ => {
                return Err(CorelamoError::InvalidData(format!(
                    "sorting by '{field}' is not supported yet (only numeric fields)"
                )));
            }
        };

        if spec.ratio > 100 {
            return Err(CorelamoError::InvalidData(format!(
                "sort ratio for '{field}' must be between 0 and 100"
            )));
        }

        sorts.push(SortFieldSpec {
            xpath: field_pol.xpath(&policy),
            order: spec.order,
            ratio: spec.ratio,
            is_float,
        });
    }

    Ok(Some(Arc::new(sorts)))
}

pub fn order_blended(items: &mut Vec<(SearchHit, Vec<Option<String>>)>, specs: &[SortFieldSpec]) {
    if items.is_empty() {
        return;
    }

    let mut mins = vec![f64::INFINITY; specs.len()];
    let mut maxs = vec![f64::NEG_INFINITY; specs.len()];
    for (_, keys) in items.iter() {
        for (index, spec) in specs.iter().enumerate() {
            let value = keys[index].as_deref().and_then(|t| {
                if spec.is_float {
                    decode_f64(t)
                } else {
                    decode_i64(t).map(|v| v as f64)
                }
            });
            if let Some(v) = value {
                mins[index] = mins[index].min(v);
                maxs[index] = maxs[index].max(v);
            }
        }
    }

    let rel_best = items
        .iter()
        .fold(0.0f32, |best, (hit, _)| best.max(hit.score));
    let total_ratio: u32 = specs.iter().map(|s| s.ratio as u32).sum();
    let rel_weight = 100u32.saturating_sub(total_ratio) as f32;
    let denom = total_ratio.max(100) as f32;

    let blends: Vec<f32> = items
        .iter()
        .map(|(hit, keys)| {
            let relevance = if rel_best > 0.0 {
                (hit.score / rel_best).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let mut field_sum = 0.0f32;
            for (index, spec) in specs.iter().enumerate() {
                let value = keys[index].as_deref().and_then(|t| {
                    if spec.is_float {
                        decode_f64(t)
                    } else {
                        decode_i64(t).map(|v| v as f64)
                    }
                });
                let component = match value {
                    None => 0.0, // missing contributes nothing
                    Some(v) => {
                        if !mins[index].is_finite() || mins[index] == maxs[index] {
                            1.0 // no spread -> all equal
                        } else {
                            let norm = ((v - mins[index]) / (maxs[index] - mins[index])) as f32;
                            match spec.order {
                                SortOrderRequest::Asc => 1.0 - norm,
                                SortOrderRequest::Desc => norm,
                            }
                        }
                    }
                };
                field_sum += spec.ratio as f32 * component;
            }

            (rel_weight * relevance + field_sum) / denom
        })
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        blends[b]
            .partial_cmp(&blends[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| items[a].0.doc_id.cmp(&items[b].0.doc_id))
    });

    let reordered: Vec<(SearchHit, Vec<Option<String>>)> = order
        .into_iter()
        .map(|index| items[index].clone())
        .collect();
    *items = reordered;
}
