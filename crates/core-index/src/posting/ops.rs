use crate::posting::{Posting, PostingList};

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
                let mut positions = a[i].positions.clone();
                positions.extend_from_slice(&b[j].positions);
                positions.sort_unstable();
                positions.dedup();

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

    PostingList::from_items(result)
}

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

    PostingList::from_items(result)
}

pub fn difference(left: &PostingList, right: &PostingList) -> PostingList {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;

    let a = left.items();
    let b = right.items();

    while i < a.len() {
        if j >= b.len() {
            result.extend_from_slice(&a[i..]);
            break;
        }

        match a[i].doc_id.cmp(&b[j].doc_id) {
            std::cmp::Ordering::Less => {
                result.push(a[i].clone());
                i += 1;
            }
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }

    PostingList::from_items(result)
}

pub fn union_many<'a>(lists: impl IntoIterator<Item = &'a PostingList>) -> PostingList {
    let mut items = Vec::new();

    for list in lists {
        items.extend_from_slice(list.items());
    }

    PostingList::from_items(items)
}

#[test]
fn from_items_merges_positions_and_weight_for_same_doc() {
    let list = PostingList::from_items(vec![
        Posting::with_weight(1, vec![3, 1], 10),
        Posting::with_weight(1, vec![2, 3], 20),
    ]);

    assert_eq!(list.items()[0].positions, vec![1, 2, 3]);
    assert_eq!(list.items()[0].weight, 20);
}
