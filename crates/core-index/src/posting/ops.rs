use core_timing::timed;

use crate::posting::{Posting, PostingList};


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
pub fn intersection(left: &PostingList, right: &PostingList) -> PostingList {
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
pub fn union_many<'a>(lists: impl IntoIterator<Item = &'a PostingList>) -> PostingList {
    let mut items = Vec::new();

    for list in lists {
        items.extend_from_slice(list.items());
    }

    PostingList::from_items(items)
}
