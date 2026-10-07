use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::types::XPathId;
use core_protocol::command_reponse_definitions::{SearchCommand, SortMode, SortOrderRequest};
use core_protocol::errors::CorelamoError;
use core_query::SearchHit;
use core_query::resolver::{FieldQuery, compile_filters as resolve_filters};
use rayon::iter::IntoParallelRefIterator;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::shard_worker::ShardHandle;
use rayon::prelude::*;

pub fn compile_filters(
    analyzer: &Analyzer,
    command: &SearchCommand,
    policy: &IndexPolicy,
) -> Result<Option<Arc<HashMap<String, FieldQuery>>>, CorelamoError> {
    let Some(filters) = command.filters.as_ref() else {
        return Ok(None);
    };
    Ok(Some(Arc::new(resolve_filters(filters, analyzer, policy)?)))
}

pub struct SortField {
    pub xpath: XPathId,
    pub order: SortOrderRequest,
    pub ratio: u8,
    pub is_bool: bool,
}

pub struct ResolvedSort {
    pub mode: SortMode,
    pub fields: Arc<Vec<SortField>>,
}

pub fn resolve_sorts(
    command: &SearchCommand,
    policy: &IndexPolicy,
) -> Result<Option<ResolvedSort>, CorelamoError> {
    let Some(sort_cmd) = command.sort.as_ref() else {
        return Ok(None);
    };
    let fields_map = sort_cmd.sort_fields();
    if fields_map.is_empty() {
        return Ok(None);
    }

    let field_count = fields_map.len();
    let mut fields = Vec::with_capacity(field_count);
    let mut total_ratio: u32 = 0;

    for (name, spec) in fields_map {
        let field_pol = policy
            .field_by_path(name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        if !field_pol.kind.is_numeric() && !field_pol.kind.is_bool() {
            return Err(CorelamoError::InvalidData(format!(
                "sorting by '{name}' is not supported yet (only numeric or bool fields)"
            )));
        }

        // ratio is only meaningful in blend mode; strict ignores it
        let ratio = match sort_cmd.mode {
            SortMode::Strict => 0,
            SortMode::Blend => {
                let r = if field_count == 1 && spec.ratio.is_none() {
                    100
                } else {
                    spec.ratio.ok_or_else(|| CorelamoError::InvalidData(format!(
                        "'ratio' is required for sort field '{name}' when sorting by more than one field"
                    )))?
                };
                if r > 100 {
                    return Err(CorelamoError::InvalidData(format!(
                        "sort ratio for '{name}' must be between 0 and 100"
                    )));
                }
                total_ratio += r as u32;
                if total_ratio > 100 {
                    return Err(CorelamoError::InvalidData(
                        "sort ratios must add up to at most 100 (the rest is relevance)"
                            .to_string(),
                    ));
                }
                r
            }
        };

        fields.push(SortField {
            xpath: field_pol.xpath(&policy),
            order: spec.order,
            ratio,
            is_bool: field_pol.kind.is_bool(),
        });
    }
    Ok(Some(ResolvedSort {
        mode: sort_cmd.mode,
        fields: Arc::new(fields),
    }))
}

/// strict: apply fields in declaration order, relevance is the final tiebreak.
pub fn order_strict(items: &mut Vec<(SearchHit, Vec<Option<f64>>)>, specs: &[SortField]) {
    items.sort_unstable_by(|(a, a_keys), (b, b_keys)| {
        for (i, spec) in specs.iter().enumerate() {
            let av = a_keys.get(i).copied().flatten();
            let bv = b_keys.get(i).copied().flatten();
            let cmp = match spec.order {
                SortOrderRequest::Desc => cmp_desc(av, bv),
                SortOrderRequest::Asc => cmp_asc(av, bv),
            };
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        }
        // relevance tiebreak
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
}

fn cmp_desc(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_asc(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// relevance_norm = score / rel_best                    → 0..1 (or 0)
/// field_norm     = value/overall_max so its 0..1
/// direction      = desc ? field_norm : 1 - field_norm  → "how good is this doc's value"
/// blend          = (rel_weight * relevance_norm + Σ ratio_i * direction_i) / denom
//                                  hit          sort fields like year, imdb...
pub fn order_blended(
    items: &mut Vec<(SearchHit, Vec<Option<f64>>)>,
    specs: &[SortField],
    bounds: &[(Option<f64>, Option<f64>)], //min max per field
) {
    if items.is_empty() {
        return;
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
                let component = match keys[index] {
                    None => 0.0,
                    Some(v) => match bounds[index] {
                        (Some(min), Some(max)) if max > min => {
                            let norm = ((v - min) / (max - min)) as f32;
                            match spec.order {
                                SortOrderRequest::Asc => 1.0 - norm,
                                SortOrderRequest::Desc => norm,
                            }
                        }
                        _ => 1.0,
                    },
                };
                field_sum += spec.ratio as f32 * component;
            }
            (rel_weight * relevance + field_sum) / denom
        })
        .collect();

    //sorting by computed vals
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        blends[b]
            .partial_cmp(&blends[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| items[a].0.doc_id.cmp(&items[b].0.doc_id))
    });

    //send back to shard_manager
    let reordered: Vec<(SearchHit, Vec<Option<f64>>)> = order
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

fn fold_min(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        _ => a.or(b),
    }
}

fn fold_max(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        _ => a.or(b),
    }
}

pub fn fold_sort_bounds(
    fields: &[SortField],
    per_shard: impl IntoIterator<Item = Vec<(Option<f64>, Option<f64>)>>,
) -> Vec<(Option<f64>, Option<f64>)> {
    let mut bounds: Vec<(Option<f64>, Option<f64>)> = fields
        .iter()
        .map(|f| {
            if f.is_bool {
                (Some(0.0), Some(1.0))
            } else {
                (None, None)
            }
        })
        .collect();

    for shard_bounds in per_shard {
        for (i, (min, max)) in shard_bounds.into_iter().enumerate() {
            if fields[i].is_bool {
                continue;
            }
            bounds[i].0 = fold_min(bounds[i].0, min);
            bounds[i].1 = fold_max(bounds[i].1, max);
        }
    }

    bounds
}
