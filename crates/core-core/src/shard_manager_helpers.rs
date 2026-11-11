use core_index::analyzer::analyzer::AnalyzerSet;
use core_index::document::IndexPolicy;
use core_index::types::XPathId;
use core_protocol::command_reponse_definitions::{
    SearchCommand, SortMode, SortModifier, SortOrderRequest,
};
use core_protocol::errors::{CorelamoError, DocFailure, FailReason};
use core_query::SearchHit;
use core_query::resolver::{FieldQuery, compile_filters as resolve_filters};
use core_storage::search_database::{DeleteReport, InsertReport, ReplaceReport};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub struct SortField {
    pub xpath: XPathId,
    pub order: SortOrderRequest,
    pub ratio: u8,
    pub is_bool: bool,
    pub modifier: SortModifier,
}

pub struct ResolvedSort {
    pub mode: SortMode,
    pub fields: Arc<Vec<SortField>>,
}

pub(crate) trait WriteReport:Send + 'static{
    fn empty() -> Self;
    fn merge(&mut self, other: Self);
    fn failures_mut(&mut self) -> &mut Vec<DocFailure>;

}
impl WriteReport for InsertReport{
    fn empty() -> Self {
        Self { inserted: 0, failures: Vec::new()}
    }
    fn merge(&mut self, other: Self) {
        self.inserted += other.inserted;
        self.failures.extend(other.failures);
    }
    fn failures_mut(&mut self) -> &mut Vec<DocFailure> {
        &mut self.failures
    }
}
impl WriteReport for ReplaceReport{
    fn empty() -> Self {
        Self { replaced: 0, failures: Vec::new()}
    }
    fn merge(&mut self, other: Self) {
        self.replaced += other.replaced;
        self.failures.extend(other.failures);
    }
    fn failures_mut(&mut self) -> &mut Vec<DocFailure> {
        &mut self.failures
    }
}
impl WriteReport for DeleteReport{
    fn empty() -> Self {
        Self { deleted: 0, failures: Vec::new()}
    }
    fn merge(&mut self, other: Self) {
        self.deleted += other.deleted;
        self.failures.extend(other.failures);
    }
    fn failures_mut(&mut self) -> &mut Vec<DocFailure> {
        &mut self.failures
    }
}

pub(crate) fn set_request_positions(failures: &mut [DocFailure], ids: &[(usize, String)]) {
    if failures.is_empty() {
        return;
    }
    let mut first_position: HashMap<&str, usize> = HashMap::new();
    for (position, id) in ids.iter().rev() {
        first_position.insert(id.as_str(), *position);
    }
    for failure in failures.iter_mut() {
        // the shard's index is the item's place in its own batch, which is ids[i]
        let by_index = failure.index.and_then(|i| ids.get(i)).map(|(position, _)| *position);
        let by_id = failure.id.as_deref().and_then(|id| first_position.get(id).copied());
        failure.index = by_index.or(by_id);
    }
}
pub(crate)fn mark_all_failed(failures: &mut Vec<DocFailure>, ids:Vec<(usize,String)>, error:&CorelamoError){
     let reason = error.to_string();
    failures.extend(ids.into_iter().map(|(position, id)| {
        DocFailure::new(Some(position), Some(id), FailReason::Internal(reason.clone()))
    }));
}
fn cmp_desc(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_asc(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn apply_modifier(v: f64, m: SortModifier) -> f64 {
    match m {
        SortModifier::None => v,
        SortModifier::Log1p => (v + 1.0).ln(),
        SortModifier::Sqrt => v.sqrt(),
    }
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

pub fn order_blended(
    items: &mut Vec<(SearchHit, Vec<Option<f64>>)>,
    specs: &[SortField],
    bounds: &[(Option<f64>, Option<f64>)],
) {
    if items.is_empty() {
        return;
    }

    let total_ratio: f32 = specs
        .iter()
        .map(|s| f32::from(s.ratio))
        .sum::<f32>()
        .min(100.0);
    let relevance_weight = 100.0 - total_ratio;

    // tiers are ranked by their place among the (tier, level) pairs present, not by value.
    // Scores are compared within one pair only: named hits keep their BM25 while fused hits
    // carry RRF scores of at most 1
    let mut ranks: Vec<(u8, u8)> = items.iter().map(|(h, _)| (h.tier, h.level)).collect();
    ranks.sort_unstable();
    ranks.dedup();
    let last_rank = ranks.len().saturating_sub(1).max(1) as f32;
    let mut best_in_rank: HashMap<(u8, u8), f32> = HashMap::with_capacity(ranks.len());
    for (hit, _) in items.iter() {
        let best = best_in_rank.entry((hit.tier, hit.level)).or_insert(0.0);
        *best = best.max(hit.score);
    }

    let blends: Vec<f32> = items
        .iter()
        .map(|(hit, keys)| {
            //relevance is rank (tier) + score, both on 0..1
            let key = (hit.tier, hit.level);
            let position = ranks.binary_search(&key).unwrap_or(0) as f32;
            let rank_norm = 1.0 - position / last_rank; // 1.0 best tier, 0.0 worst
            let best = best_in_rank.get(&key).copied().unwrap_or(0.0);
            let score_norm = if best > 0.0 {
                (hit.score / best).clamp(0.0, 1.0)
            } else {
                0.0
            };

            //we do / 2 so that final relevance is [0...1] not [0....2]
            let relevance = f32::midpoint(rank_norm, score_norm) * relevance_weight;

            let mut field = 0.0f32;
            for (index, spec) in specs.iter().enumerate() {
                let component = match keys.get(index).copied().flatten() {
                    None => 0.0,
                    Some(v) => match bounds[index] {
                        (Some(min), Some(max)) => {
                            let min = apply_modifier(min, spec.modifier);
                            let max = apply_modifier(max, spec.modifier);
                            let v = apply_modifier(v, spec.modifier);
                            if max > min {
                                let norm = ((v - min) / (max - min)) as f32;
                                match spec.order {
                                    SortOrderRequest::Asc => 1.0 - norm,
                                    SortOrderRequest::Desc => norm,
                                }
                            } else {
                                1.0
                            }
                        }
                        _ => 1.0,
                    },
                };
                field += f32::from(spec.ratio) * component;
            }

            relevance + field
        })
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        blends[b]
            .total_cmp(&blends[a])
            .then_with(|| items[a].0.doc_id.cmp(&items[b].0.doc_id))
    });

    let reordered: Vec<(SearchHit, Vec<Option<f64>>)> = order
        .into_iter()
        .map(|index| {
            let (mut hit, keys) = items[index].clone();
            hit.score = blends[index];
            (hit, keys)
        })
        .collect();
    *items = reordered;
}

pub fn compile_filters(
    analyzers: &AnalyzerSet,
    command: &SearchCommand,
    policy: &IndexPolicy,
) -> Result<Option<Arc<HashMap<String, FieldQuery>>>, CorelamoError> {
    let Some(filters) = command.filters.as_ref() else {
        return Ok(None);
    };
    Ok(Some(Arc::new(resolve_filters(filters, analyzers, policy)?)))
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

        let ratio = match sort_cmd.mode {
            SortMode::Strict => 0,
            SortMode::Blend => {
                //sort optional
                let r = match spec.ratio {
                    Some(r) => r,
                    None if field_count == 1 => 40,
                    None => (100 / (field_count + 1)) as u8,
                };
                if r > 100 {
                    return Err(CorelamoError::InvalidData(format!(
                        "sort ratio for '{name}' must be between 0 and 100"
                    )));
                }
                total_ratio += u32::from(r);
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
            xpath: field_pol.xpath(policy),
            order: spec.order,
            ratio,
            is_bool: field_pol.kind.is_bool(),
            modifier: spec.modifier,
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
        // the ranking cascade comes first: a better tier always ranks above a worse one
        if a.tier != b.tier {
            return a.tier.cmp(&b.tier);
        }
        for (i, spec) in specs.iter().enumerate() {
            let av = a_keys.get(i).copied().flatten();
            let bv = b_keys.get(i).copied().flatten();
            let cmp = match spec.order {
                SortOrderRequest::Desc => cmp_desc(av, bv),
                SortOrderRequest::Asc => cmp_asc(av, bv),
            };
            if cmp != Ordering::Equal {
                return cmp;
            }
        }
        // relevance tiebreak
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
}
