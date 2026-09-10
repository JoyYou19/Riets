use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::document::policy::FieldKind;
use core_index::numbers::{decode_f64, decode_i64, float_term, integer_term, parse_filter};
use core_index::types::XPathId;
use core_protocol::command_reponse_definitions::{SearchCommand, SortOrderRequest};
use core_protocol::errors::CorelamoError;
use core_query::SearchHit;
use core_query::executor::{FieldFilter, FieldFilterKind};
use core_query::query_string_parser::parse_and_analyze;
use std::collections::HashMap;
use std::sync::Arc;

pub struct SortField {
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

                let kind = match field_pol.kind {
                    //old behavior with text
                    FieldKind::Text => FieldFilterKind::Text(parse_and_analyze(term, analyzer)?),
                    //numeric >40  >=40  <50  <=50  =20  30..40
                    FieldKind::Integer => {
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
                    FieldKind::Float => {
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
) -> Result<Option<Arc<Vec<SortField>>>, CorelamoError> {
    let Some(requests) = command.sort.as_ref() else {
        return Ok(None);
    };
    if requests.is_empty() {
        return Ok(None);
    }

    let field_count = requests.len();

    let mut sorts = Vec::with_capacity(field_count);
    let mut total_ratio: u32 = 0;

    for (field, spec) in requests {
        let field_pol = policy
            .fields
            .iter()
            .find(|f| &f.name == field)
            .ok_or_else(|| CorelamoError::PathNotIndexed(field.clone()))?;

        let is_float = match field_pol.kind {
            FieldKind::Integer => false,
            FieldKind::Float => true,
            _ => {
                return Err(CorelamoError::InvalidData(format!(
                    "sorting by '{field}' is not supported yet (only numeric fields)"
                )));
            }
        };

        let ratio = if field_count == 1 && spec.ratio.is_none() {
            100
        } else {
            match spec.ratio {
                Some(ratio) => ratio,
                None => {
                    return Err(CorelamoError::InvalidData(format!(
                        "'ratio' is required for sort field '{field}' when sorting by more than one field"
                    )));
                }
            }
        };

        if ratio > 100 {
            return Err(CorelamoError::InvalidData(format!(
                "sort ratio for '{field}' must be between 0 and 100"
            )));
        }

        total_ratio += ratio as u32;
        if total_ratio > 100 {
            return Err(CorelamoError::InvalidData(
                "sort ratios must add up to at most 100 (the rest is relevance)".to_string(),
            ));
        }

        sorts.push(SortField {
            xpath: field_pol.xpath(&policy),
            order: spec.order,
            ratio,
            is_float,
        });
    }

    Ok(Some(Arc::new(sorts)))
}

/// relevance_norm = score / rel_best                    → 0..1 (or 0)
/// field_norm     = (value - min) / (max - min)         → 0..1 position in the window
/// direction      = desc ? field_norm : 1 - field_norm  → "how good is this doc's value"
/// blend          = (rel_weight * relevance_norm + Σ ratio_i * direction_i) / denom
//                                  hit          sort fields like year, imdb...
pub fn order_blended(items: &mut Vec<(SearchHit, Vec<Option<String>>)>, specs: &[SortField]) {
    if items.is_empty() {
        return;
    }

    //find the min/max values for normalization across all fields
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

    //same for relevance just some dark magic to normalize everything 0-100
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
                // score relative to the best
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
                                //for ascending its the same only negative basically (smaller better)
                                SortOrderRequest::Asc => 1.0 - norm,
                                SortOrderRequest::Desc => norm,
                            }
                        }
                    }
                };
                //weight math
                field_sum += spec.ratio as f32 * component;
            }

            (rel_weight * relevance + field_sum) / denom
        })
        .collect();

    //sorting
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        blends[b]
            .partial_cmp(&blends[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| items[a].0.doc_id.cmp(&items[b].0.doc_id))
    });

    //send back to shard_manager
    let reordered: Vec<(SearchHit, Vec<Option<String>>)> = order
        .into_iter()
        .map(|index| {
            let (mut hit, keys) = items[index].clone();
            hit.score = blends[index]; //switching the relevance score with our calculated one for
            //better output
            (hit, keys)
        })
        .collect();
    *items = reordered;
}
