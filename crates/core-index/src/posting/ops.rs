use core_timing::timed;
use std::{cmp::Reverse, collections::BinaryHeap};
use crate::{posting::{Posting, PostingList}, types::DocId};


#[timed(search)]
pub fn union(left: &PostingList, right: &PostingList) -> PostingList {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;

    let a = left.items();
    let b = right.items();

    while i < a.len() && j < b.len() {
        match a[i].doc_id.cmp(&b[j].doc_id) {
            std::cmp::Ordering::Less => {
                result.push(a[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j].clone());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let mut positions = Vec::with_capacity(a[i].positions.len() + b[j].positions.len());
                let (pa, pb) = (&a[i].positions, &b[j].positions);
                let (mut x, mut y) = (0, 0);
                while x < pa.len() && y < pb.len() {
                    match pa[x].cmp(&pb[y]) {
                        std::cmp::Ordering::Less => { positions.push(pa[x]); x += 1; }
                        std::cmp::Ordering::Greater => { positions.push(pb[y]); y += 1; }
                        std::cmp::Ordering::Equal => { positions.push(pa[x]); x += 1; y += 1; }
                    }
                }
                positions.extend_from_slice(&pa[x..]);
                positions.extend_from_slice(&pb[y..]);

                result.push(Posting::with_weight(
                    a[i].doc_id,
                    positions,
                    a[i].weight.max(b[j].weight),
                ));

                i += 1;
                j += 1;
            }
        }
    }

    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);

    PostingList::from_sorted(result)
}

#[timed(search)]
pub fn linear_search(left: &PostingList, right: &PostingList) -> PostingList {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;

    let a = left.items();
    let b = right.items();

    while i < a.len() && j < b.len() {
        match a[i].doc_id.cmp(&b[j].doc_id) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i].clone());
                i += 1;
                j += 1;
            }
        }
    }

    PostingList::from_sorted(result)
}
#[timed(search)]
pub fn intersection(left: &PostingList, right: &PostingList) -> PostingList {
    let a = left.items();
    let b = right.items();

    let (short_len, long_len) = (a.len().min(b.len()), a.len().max(b.len()));

    // cost of galloping: short * log2(long/short + 1)
    let gallop_cost = short_len.saturating_mul(
        (long_len / short_len.max(1)).max(1).ilog2() as usize + 1
    );
    let merge_cost = a.len() + b.len();

    if gallop_cost < merge_cost {
        exponential_search(left, right)
    } else {
        linear_search(left, right) // your existing loop
    }
}
pub fn exponential_search(left: &PostingList, right: &PostingList) -> PostingList{
    let a = left.items();
    let b = right.items();

    // Drive the search from the shorter list.
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };

    let mut result = Vec::new();
    let mut cursor = 0usize; // position in `long`

    for item in short {
        if cursor >= long.len() {
            break;
        }

        cursor = exponent_search_to(long, cursor, item.doc_id);

        if cursor < long.len() && long[cursor].doc_id == item.doc_id {
            result.push(item.clone());
            cursor += 1; // advance past the match
        }
    }

    PostingList::from_sorted(result)
}
fn exponent_search_to(slice: &[Posting], start: usize, target: DocId) -> usize {
    // If we're already past the target, nothing to gallop.
    if start >= slice.len() || slice[start].doc_id >= target {
        return start;
    }

    let mut bound = 1;
    let mut lo = start;

    // Double the step until we overshoot the target.
    while lo + bound < slice.len() && slice[lo + bound].doc_id < target {
        bound <<= 1;
    }

    // Now the target (if present) is in (lo, lo + bound].
    let hi = (lo + bound + 1).min(slice.len());
    lo += 1; // we know slice[lo] < target, so start the search after it

    // Binary search within [lo, hi).
    let mut left = lo;
    let mut right = hi;
    while left < right {
        let mid = left + (right - left) / 2;
        if slice[mid].doc_id < target {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}
#[timed(search)]
pub fn union_many<'a>(lists: impl IntoIterator<Item = &'a PostingList>) -> PostingList {
    let sources: Vec<&[Posting]> = lists
        .into_iter()
        .map(|l| l.items())
        .filter(|s| !s.is_empty())
        .collect();

    match sources.len() {
        0 => return PostingList::default(),
        1 => return PostingList::from_sorted(sources[0].to_vec()),
        _ => {}
    }

    let total: usize = sources.iter().map(|s| s.len()).sum();
    let mut out: Vec<Posting> = Vec::with_capacity(total);

    // (doc_id, source index, offset) — Reverse for min-heap on doc_id.
    let mut heap: BinaryHeap<Reverse<(DocId, usize, usize)>> =
        BinaryHeap::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        heap.push(Reverse((s[0].doc_id, i, 0)));
    }

    while let Some(Reverse((_, i, offset))) = heap.pop() {
        out.push(sources[i][offset].clone());
        let next = offset + 1;
        if next < sources[i].len() {
            heap.push(Reverse((sources[i][next].doc_id, i, next)));
        }
    }

    // Output is sorted by doc_id but the same doc can appear in several
    // segments, so duplicates are real here — from_sorted merges them.
    PostingList::from_sorted(out)
}
/// Restricts `postings` to the doc_ids in `candidates`, keeping
/// `postings`' own positions and weights. Driven from `candidates`
/// (expected to be the smaller side) galloping into `postings`.
///
/// Distinct from `intersection`: the output always carries the
/// left/`postings` side's payload, never the candidate side's.
#[timed(search)]
pub fn restrict_to(postings: &PostingList, candidates: &[DocId]) -> PostingList {
    let p = postings.items();
    let mut result = Vec::with_capacity(candidates.len().min(p.len()));
    let mut cursor = 0usize;

    for &doc_id in candidates {
        if cursor >= p.len() {
            break;
        }
        cursor = exponent_search_to(p, cursor, doc_id);
        if cursor < p.len() && p[cursor].doc_id == doc_id {
            result.push(p[cursor].clone());
            cursor += 1;
        }
    }

    PostingList::from_sorted(result)
}
/// Doc-id-only intersection for candidate generation. Carries no
/// positions or weights, so no per-match position cloning.
#[timed(search)]
pub fn intersect_ids(ids: &[DocId], postings: &PostingList) -> Vec<DocId> {
    let p = postings.items();
    let mut result = Vec::with_capacity(ids.len().min(p.len()));
    let mut cursor = 0usize;

    for &doc_id in ids {
        if cursor >= p.len() {
            break;
        }
        cursor = exponent_search_to(p, cursor, doc_id);
        if cursor < p.len() && p[cursor].doc_id == doc_id {
            result.push(doc_id);
            cursor += 1;
        }
    }

    result
}