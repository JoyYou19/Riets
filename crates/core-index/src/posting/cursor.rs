use crate::{
    posting::{Posting, PostingList},
    types::DocId,
};

/// cursor that can move only forwards over a posting list
///
/// WAND and other posting list algorithms need to move through multiple sorted posting lists
/// independently without creating new postinglists
///
/// I sort them by doc_id the cursor can never move backwards
pub struct PostingCursor<'a> {
    postings: &'a [Posting],
    index: usize,
}

impl<'a> PostingCursor<'a> {
    pub fn new(list: &'a PostingList) -> Self {
        Self {
            postings: list.items(),
            index: 0,
        }
    }

    // Returns whether the cursor has moved past the final posting
    pub fn is_exhausted(&self) -> bool {
        self.index >= self.postings.len()
    }

    // current doc_id of the cursor
    pub fn doc_id(&self) -> Option<DocId> {
        self.postings.get(self.index).map(|posting| posting.doc_id)
    }

    // Returns the complete posting at the current cursor position,
    // gives the specific posting instead of just doc_id
    pub fn current(&self) -> Option<&'a Posting> {
        self.postings.get(self.index)
    }

    // Advances one posting to the right
    pub fn next(&mut self) {
        if !self.is_exhausted() {
            self.index += 1;
        }
    }

    // Advances to the first posting which has a doc_id >= target
    //
    // This means that we skip things used by WAND. Instead of going through each of the postings
    // between the cursor document and the target one we first use the galloping that works very
    // similarily to the one created by the Betona Kokteila man and search to find a range
    // containing the target and then we simply binary search that range. I remember hearing some
    // talk about binary search here being unoptimized, we keep it for now.
    //
    // If the current document is already bigger or equal than the target nothing happens
    // If no document is bigger or equal to the target the cursor exhausts
    //
    // This cursor once again cannot ever be moved backwards
    pub fn advance_to(&mut self, target: DocId) {
        if self.is_exhausted() {
            return;
        }

        if self.postings[self.index].doc_id >= target {
            return;
        }

        let start = self.index;
        let mut bound = 1usize;

        // grow the search range exponentially until we either pass target
        // or reach the end of the postings
        while start + bound < self.postings.len() && self.postings[start + bound].doc_id < target {
            bound = bound.saturating_mul(2);
        }

        // We now know tha tth efirst posting >= target if it exists it lies somewhere here
        let mut left = start + bound / 2;
        let mut right = (start + bound + 1).min(self.postings.len());

        // find the first posting with doc_id >= target
        while left < right {
            let mid = left + (right - left) / 2;

            if self.postings[mid].doc_id < target {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        self.index = left;
    }
}
