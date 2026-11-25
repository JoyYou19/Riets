use crate::{
    posting::{ Posting, PostingList },
    types::DocId,
};
use core_timing::timed;
use std::{ cmp::Reverse, collections::BinaryHeap };

// #[timed(search)]
// pub(crate) fn linear_search(left: &PostingList, right: &PostingList) -> PostingList {
//     let mut result = Vec::new();
//     let mut a = left.items();
//     let mut b = right.items();

//     while let (Some((x, rest_a)), Some((y, rest_b))) = (a.split_first(), b.split_first()) {
//         match x.doc_id.cmp(&y.doc_id) {
//             Ordering::Less => a = rest_a,
//             Ordering::Greater => b = rest_b,
//             Ordering::Equal => {
//                 result.push(x.clone());
//                 a = rest_a;
//                 b = rest_b;
//             }
//         }
//     }

//     PostingList::from_sorted(result)
// }

// #[timed(search)]
// pub(crate) fn intersection(left: &PostingList, right: &PostingList) -> PostingList {
//     let a = left.items();
//     let b = right.items();

//     let (short_len, long_len) = (a.len().min(b.len()), a.len().max(b.len()));

//     // cost of galloping: short * log2(long/short + 1)
//     let ratio_log2 = usize
//         ::try_from((long_len / short_len.max(1)).max(1).ilog2())
//         .unwrap_or(usize::MAX);
//     let gallop_cost = short_len.saturating_mul(ratio_log2.saturating_add(1));
//     let merge_cost = a.len().saturating_add(b.len());

//     if gallop_cost < merge_cost {
//         exponential_search(left, right)
//     } else {
//         linear_search(left, right)
//     }
// }

#[must_use]
pub fn exponential_search(left: &PostingList, right: &PostingList) -> PostingList {
    let a = left.items();
    let b = right.items();

    // Drive the search from the shorter list.
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };

    let mut result = Vec::new();
    let mut cursor = 0usize; // position in `long`

    for item in short {
        cursor = exponent_search_to(long, cursor, item.doc_id);

        match long.get(cursor) {
            Some(found) if found.doc_id == item.doc_id => {
                result.push(item.clone());
                cursor += 1; // advance past the match
            }
            Some(_) => {}
            None => {
                break;
            }
        }
    }

    PostingList::from_sorted(result)
}

/// First index at or after `start` whose doc id is `>= target` (or `slice.len()`).
fn exponent_search_to(slice: &[Posting], start: usize, target: DocId) -> usize {
    let Some(tail) = slice.get(start..) else {
        return start;
    };

    // If we're already at or past the target, nothing to gallop.
    if tail.first().is_none_or(|first| first.doc_id >= target) {
        return start;
    }

    // Double the step until we overshoot the target. `tail[0] < target` is known.
    let mut bound = 1usize;
    while tail.get(bound).is_some_and(|posting| posting.doc_id < target) {
        bound <<= 1;
    }

    // `tail[bound / 2] < target`, so the answer is in `bound / 2 + 1 ..= bound`.
    let from = bound / 2 + 1;
    let end = tail.len().min(bound.saturating_add(1));
    let window = tail.get(from..end).unwrap_or_default();

    start + from + window.partition_point(|posting| posting.doc_id < target)
}

#[timed(search)]
pub fn union_many<'a>(lists: impl IntoIterator<Item = &'a PostingList>) -> PostingList {
    let sources: Vec<&[Posting]> = lists
        .into_iter()
        .map(PostingList::items)
        .filter(|s| !s.is_empty())
        .collect();

    match sources.as_slice() {
        [] => {
            return PostingList::default();
        }
        [only] => {
            return PostingList::from_sorted(only.to_vec());
        }
        _ => {}
    }

    let total: usize = sources.iter().map(|s| s.len()).sum();
    let mut out: Vec<Posting> = Vec::with_capacity(total);

    // (doc_id, source index, offset) — Reverse for min-heap on doc_id.
    let mut heap: BinaryHeap<Reverse<(DocId, usize, usize)>> = BinaryHeap::with_capacity(
        sources.len()
    );
    for (index, source) in sources.iter().enumerate() {
        if let Some(first) = source.first() {
            heap.push(Reverse((first.doc_id, index, 0)));
        }
    }

    while let Some(Reverse((_, index, offset))) = heap.pop() {
        let Some(source) = sources.get(index) else {
            continue;
        };
        let Some(posting) = source.get(offset) else {
            continue;
        };
        out.push(posting.clone());
        let next = offset + 1;
        if let Some(following) = source.get(next) {
            heap.push(Reverse((following.doc_id, index, next)));
        }
    }

    // Output is sorted by doc_id but the same doc can appear in several
    // segments, so duplicates are real here — from_sorted merges them.
    PostingList::from_sorted(out)
}