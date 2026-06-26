use std::sync::Arc;

use core_index::{posting::PostingList, types::Position};

use crate::ScoredPosting;

/*
* Turns postings into scored postings based on whatever criteria
*/

pub fn score_term(postings: &PostingList) -> Vec<ScoredPosting> {
    postings
        .items()
        .iter()
        .filter(|p| !p.positions.is_empty())
        .map(|p| ScoredPosting {
            doc_id: p.doc_id,
            positions: Arc::from(p.positions.as_slice()),
            weight_sum: p.weight as u32,
            matched_terms: 1,
            density: 1.0,
        })
        .collect()
}

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

        let proximity = 1.0 / (1.0 + distance as f32);

        result.push(ScoredPosting {
            doc_id: l.doc_id,
            positions: Arc::from([left_pos, right_pos]),
            weight_sum: l.weight_sum + r.weight as u32,
            density: l.density + proximity,
            matched_terms: l.matched_terms + 1,
        });
    }

    result
}

// WARN: Need to add a maximum window later down the road for large documents, might want to
// specify in query the amount of positions to search for
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
