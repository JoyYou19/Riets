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

                idf * ((tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm))
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

        let Some((left_pos, right_pos, distance)) = closest_window(&l.positions, &r.positions)
        else {
            continue;
        };

        let proximity = 1.0 + (1.0 / (1.0 + distance as f32));

        result.push(ScoredPosting {
            doc_id: l.doc_id,
            positions: Arc::from([left_pos, right_pos]),
            score: l.score + ((r.weight as u64) * 1000),
            density: l.density * proximity,
            matched_terms: l.matched_terms + 1,
        });
    }

    result
}

// WARN: Need to add a maximum window later down the road for large documents, might want to
// specify in query the amount of positions to search for
#[timed(search)]
fn closest_window(left: &[Position], right: &[Position]) -> Option<(Position, Position, u32)> {
    let mut best: Option<(Position, Position, u32)> = None;

    let mut i = 0;
    let mut j = 0;

    while i < left.len() && j < right.len() {
        let a = left[i];
        let b = right[j];
        let distance = a.abs_diff(b);

        if best.map_or(true, |(_, _, best_distance)| distance < best_distance) {
            best = Some((a, b, distance));
        }

        if a < b {
            i += 1;
        } else {
            j += 1;
        }
    }

    best
}
