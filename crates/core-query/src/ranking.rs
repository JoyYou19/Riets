//! Turns the evidence the cascade collects into the final order. Every hit, whichever tier or
//! candidate source found it and whichever analyzer group scored it, gets its score here, once:
//!
//! ```text
//! score = base × coverage^COVERAGE_POWER × (1 + DENSITY_BOOST × density) × (1 + NAME_BONUS × name_share)
//! ```
//!
//! Coverage is skipped for documents a name field ties to the query: their name is the evidence.

use std::collections::{HashMap, hash_map::Entry};

use core_index::types::DocId;
use core_timing::timed;

use crate::{
    planner::{DENSITY_BOOST, NAME_BONUS},
    search_hit::{SearchHit, rank_order},
};

/// Coverage is raised to this power: 0 ignores it; 4 made partial matches nearly worthless.
pub const COVERAGE_POWER: f32 = 2.0;
/// Share of the query's weight a document needs to enter the ranked pool, when the plan allows
/// partial matches.
pub const MIN_COVERAGE: f32 = 0.5;
/// Candidates the ranked pool keeps before the final scoring.
pub const POOL_SIZE: usize = 200;
/// Candidates the "any word" source keeps, so pages holding more words survive to be scored.
pub const ANY_POOL_SIZE: usize = 300;
/// Clauses tracked in `SearchHit::mask`, one bit each; longer queries are scored without
/// coverage.
pub const MAX_CLAUSES: usize = 64;

/// The hits one analyzer group's cascade found, with the weight (idf) of each query clause their
/// masks refer to. Empty weights: the group measured no coverage.
#[derive(Debug, Clone, Default)]
pub struct RankedGroup {
    pub hits: Vec<SearchHit>,
    pub clause_weights: Vec<f32>,
}

/// Share of `weights` held by the clauses set in `mask`, 0..=1; 1 when nothing has weight.
pub fn coverage(mask: u64, weights: &[f32]) -> f32 {
    let total: f32 = weights.iter().sum();
    if total.is_nan() || total <= 0.0 {
        return 1.0;
    }
    let held: f32 = weights
        .iter()
        .enumerate()
        .take(MAX_CLAUSES)
        .filter(|(index, _)| (mask & (1u64 << index)) != 0)
        .map(|(_, weight)| weight)
        .sum();
    (held / total).clamp(0.0, 1.0)
}

/// The final score of a hit with the given coverage.
pub fn final_score(hit: &SearchHit, coverage: f32) -> f32 {
    let score = hit.base
        * coverage.powf(COVERAGE_POWER)
        * (1.0 + DENSITY_BOOST * hit.distance_factor)
        * (1.0 + NAME_BONUS * hit.name_share);
    if score.is_finite() {
        score.max(0.0)
    } else {
        0.0
    }
}

struct Merged {
    hit: SearchHit,
    // best coverage any single group measured for the document
    best_coverage: f32,
    // a group with clause weights saw the document, so its mask means something
    measured: bool,
}

/// Merges the groups' hits per document, scores every hit with `final_score` and returns the
/// best `k` in `rank_order`.
///
/// Across groups a document takes its best tier, the sum of its BM25 and the union of its
/// matched clauses when every group split the query into the same clauses (each clause weighed
/// by its lowest weight in any group); otherwise its best coverage in any one group.
#[timed(search)]
pub fn finalize(groups: Vec<RankedGroup>, k: usize) -> Vec<SearchHit> {
    if k == 0 {
        return Vec::new();
    }
    let weighted: Vec<&[f32]> = groups
        .iter()
        .map(|group| group.clause_weights.as_slice())
        .filter(|weights| !weights.is_empty())
        .collect();
    let combined: Option<Vec<f32>> = match weighted.first() {
        Some(first) if weighted.iter().all(|weights| weights.len() == first.len()) => Some(
            (0..first.len())
                .map(|index| {
                    weighted
                        .iter()
                        .map(|weights| weights[index])
                        .fold(f32::INFINITY, f32::min)
                })
                .collect(),
        ),
        _ => None,
    };

    let capacity = groups.iter().map(|group| group.hits.len()).sum();
    let mut merged: HashMap<DocId, Merged> = HashMap::with_capacity(capacity);
    for group in groups {
        let measured = !group.clause_weights.is_empty();
        for hit in group.hits {
            let group_coverage = coverage(hit.mask, &group.clause_weights);
            match merged.entry(hit.doc_id) {
                Entry::Occupied(mut slot) => {
                    let known = slot.get_mut();
                    if (hit.tier, hit.level) < (known.hit.tier, known.hit.level) {
                        known.hit.tier = hit.tier;
                        known.hit.level = hit.level;
                    }
                    known.hit.base += hit.base;
                    known.hit.mask |= hit.mask;
                    known.hit.distance_factor = known.hit.distance_factor.max(hit.distance_factor);
                    known.hit.name_share = known.hit.name_share.max(hit.name_share);
                    known.hit.matched_terms = known.hit.matched_terms.max(hit.matched_terms);
                    known.hit.weight_sum = known.hit.weight_sum.saturating_add(hit.weight_sum);
                    known.best_coverage = known.best_coverage.max(group_coverage);
                    known.measured |= measured;
                }
                Entry::Vacant(slot) => {
                    slot.insert(Merged {
                        hit,
                        best_coverage: group_coverage,
                        measured,
                    });
                }
            }
        }
    }

    let mut hits: Vec<SearchHit> = merged
        .into_values()
        .map(|entry| {
            let mut hit = entry.hit;
            let doc_coverage = match &combined {
                Some(weights) if entry.measured => coverage(hit.mask, weights),
                _ => entry.best_coverage,
            };
            hit.score = final_score(&hit, doc_coverage);
            hit
        })
        .collect();
    hits.sort_unstable_by(rank_order);
    hits.truncate(k);
    hits
}
