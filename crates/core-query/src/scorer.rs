use std::sync::Arc;

use core_index::{
    posting::PostingList,
    search::SearchStats,
    types::{Position, XPathId},
};
use core_timing::timed;

use crate::ScoredPosting;

/*
* Turns postings into scored postings based on whatever criteria
*/

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;
const SCORE_SCALE: f32 = 1000.0;

// fn trace_bm25() -> bool {
//     std::env::var_os("CORELAMO_TRACE_BM25").is_some()
// }

#[timed(search)]
pub fn score_term_hybrid<S: SearchStats>(
    stats: &S,
    postings: &PostingList,
    xpath: XPathId,
) -> Vec<ScoredPosting> {
    // let trace = trace_bm25();
    //let total_started = std::time::Instant::now();

    //let started = std::time::Instant::now();

    let n = stats.doc_count(xpath) as f32;
    let df = postings.len() as f32;
    let avgdl = stats.avg_doc_len(xpath);

    /*  if trace {
      tracing::trace!(
            xpath=%xpath,
            docs=n,
            doc_freq=%df,
            avg_doc_len=%avgdl,
            time=?started.elapsed(),
            "bm25 stats",
        );
    }
    */
    //  let started = std::time::Instant::now();

    let scored: Vec<ScoredPosting> = postings
        .items()
        .iter()
        .filter(|p| !p.positions.is_empty())
        .map(|p| {
            let policy_weight = p.weight as f32;

            let bm25 = if n > 0.0 && df > 0.0 && avgdl > 0.0 {
                let tf = p.positions.len() as f32;
                let dl = stats.doc_len(p.doc_id, xpath).unwrap_or(avgdl as u32) as f32;

                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let norm = 1.0 - BM25_B + BM25_B * (dl / avgdl);

                idf * ((tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm) + 1 as f32)
            } else {
                1.0
            };

            let hybrid = policy_weight * bm25.max(0.001);

            ScoredPosting {
                doc_id: p.doc_id,
                positions: Arc::from(p.positions.as_slice()),
                score: (hybrid * SCORE_SCALE) as u64,
                matched_terms: 1,
                density: 1.0,
            }
        })
        .collect();

    /*   if trace {
           tracing::trace!(
                xpath=%xpath,
                postings=%postings.len(),
                scored=%scored.len(),
                scoring_took=?started.elapsed(),
                total_took=?total_started.elapsed(),

                "bm25 score",
            );
        }
    */
    scored
}

// pub fn score_term(postings: &PostingList) -> Vec<ScoredPosting> {
//     postings
//         .items()
//         .iter()
//         .filter(|p| !p.positions.is_empty())
//         .map(|p| ScoredPosting {
//             doc_id: p.doc_id,
//             positions: Arc::from(p.positions.as_slice()),
//             score: p.weight as u64 * 1000,
//             matched_terms: 1,
//             density: 1.0,
//         })
//         .collect()
// }

#[timed(search)]
pub fn scored_and(left: &[ScoredPosting], right: &PostingList) -> Vec<ScoredPosting> {
    let mut result = Vec::new();

    for l in left {
        let Ok(i) = right.items().binary_search_by_key(&l.doc_id, |p| p.doc_id) else {
            continue;
        };

        let r = &right.items()[i];

        let mut positions = l.positions.clone();
        let mut density = l.density;

        if let Some(pair) = closest_window(&l.positions, &r.positions) {
            positions = Arc::from([pair.left_pos, pair.right_pos]);
            density *= 1.0 + (1.0 / (1.0 + pair.distance as f32));
        }

        result.push(ScoredPosting {
            doc_id: l.doc_id,
            positions,
            score: l.score + ((r.weight as u64) * 1000),
            density,
            matched_terms: l.matched_terms + 1,
        });
    }

    result
}

//helper to make code prettier
#[derive(Debug, Clone, Copy)]
struct ClosestPair {
    left_pos: Position,
    right_pos: Position,
    distance: u32,
}

fn make_pair(few_is_left: bool, few_pos: Position, other_pos: Position) -> ClosestPair {
    let distance = few_pos.abs_diff(other_pos);
    if few_is_left {
        ClosestPair {
            left_pos: few_pos,
            right_pos: other_pos,
            distance,
        }
    } else {
        ClosestPair {
            left_pos: other_pos,
            right_pos: few_pos,
            distance,
        }
    }
}

//WARN: es esmu valters es uzrakstiju kko kas nav optimals plz help me:
//INFO: es esmu normunds, es centos uzlbot sito pec valtera warninga
const PROXIMITY_WINDOW: Position = 8;
#[timed(search)]
fn closest_window(left: &[Position], right: &[Position]) -> Option<ClosestPair> {
    if left.is_empty() || right.is_empty() {
        return None;
    }

    let probe_cost = left
        .len()
        .min(right.len())
        .saturating_mul(right.len().max(left.len()).ilog2() as usize + 1);
    let merge_cost = left.len() + right.len();

    if probe_cost < merge_cost {
        closest_window_probe(left, right) //one term is much rarer in this doc
    } else {
        closest_window_merge(left, right) //both terms occur a similar number of times
    }
}

fn closest_window_merge(left: &[Position], right: &[Position]) -> Option<ClosestPair> {
    let mut best: Option<ClosestPair> = None;

    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        let left_pos = left[left_index];
        let right_pos = right[right_index];
        let distance = left_pos.abs_diff(right_pos);

        if distance <= PROXIMITY_WINDOW {
            if best.is_none_or(|b| distance < b.distance) {
                best = Some(ClosestPair {
                    left_pos,
                    right_pos,
                    distance,
                });

                if distance <= 1 {
                    break;
                }
            }
        }

        if left_pos < right_pos {
            left_index += 1;
        } else {
            right_index += 1;
        }
    }

    best
}

///the fancy binary search for occasions when one array has like 2 and the other has like 500
//positions
fn closest_window_probe(left: &[Position], right: &[Position]) -> Option<ClosestPair> {
    let (few, many, few_is_left) = if left.len() <= right.len() {
        (left, right, true)
    } else {
        (right, left, false)
    };

    let mut best: Option<ClosestPair> = None;

    for &position in few {
        let split = many.partition_point(|&other| other < position);

        if split > 0 {
            let neighbour = many[split - 1];
            let distance = position - neighbour;
            if distance <= PROXIMITY_WINDOW && best.is_none_or(|b| distance < b.distance) {
                best = Some(make_pair(few_is_left, position, neighbour));
            }
        }

        // Neighbour after us.
        if let Some(&neighbour) = many.get(split) {
            let distance = neighbour - position;
            if distance <= PROXIMITY_WINDOW && best.is_none_or(|b| distance < b.distance) {
                best = Some(make_pair(few_is_left, position, neighbour));
            }
        }
    }

    best
}
