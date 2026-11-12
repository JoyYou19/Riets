use std::io;

use crate::{
    disk::block_cursor::BlockCursor, posting::{DeleteSet, Posting, PostingList, cursor}, types::DocId,
};

pub enum TermCursor<'a>{
    Empty,
    //a list in memory
    Slice(PostingCursor<'a>),
    //one disk 
    Block(Box<BlockCursor<'a>>),
    /// Several sources merged by doc id, with deleted documents skipped.
    Union(Box<UnionCursor<'a>>),
}
impl <'a> TermCursor<'a> {
    pub fn is_exhausted(&self) -> bool{
        match self {
            Self::Slice(cursor)=>cursor.is_exhausted(),
            Self::Block(cursor)=>cursor.is_exhausted(),
            Self::Union(cursor)=>cursor.current.is_none(),
            Self::Empty=>true,
        }
    }
    pub fn doc_id(&self) -> Option<DocId> {
        match self {
            Self::Slice(cursor) => cursor.doc_id(),
            Self::Block(cursor) => cursor.doc_id(),
            Self::Union(cursor) => cursor.current,
            Self::Empty=> None
        }
    }

    pub fn term_freq(&self) -> u32 {
        match self {
            Self::Slice(cursor) => cursor.current().map_or(0, |posting| {
                u32::try_from(posting.positions.len()).unwrap_or(u32::MAX)
            }),
            Self::Block(cursor) => cursor.term_freq(),
            Self::Union(cursor) => cursor.on_current().map(TermCursor::term_freq).max().unwrap_or(0),
            Self::Empty=>0
        }
    }

    pub fn weight(&self) -> u16 {
        match self {
            Self::Slice(cursor) => cursor.current().map_or(0, |posting| posting.weight),
            Self::Block(cursor) => cursor.weight(),
            Self::Union(cursor) => cursor.on_current().map(TermCursor::weight).max().unwrap_or(0),
            Self::Empty=>0
        }
    }
    pub fn next(&mut self) {
        match self {
            Self::Slice(cursor) => cursor.next(),
            Self::Block(cursor) => cursor.next(),
            Self::Union(cursor) => cursor.next(),
            Self::Empty=> {}
        }
    }

    pub fn advance_to(&mut self, target: DocId) {
        match self {
            Self::Slice(cursor) => cursor.advance_to(target),
            Self::Block(cursor) => cursor.advance_to(target),
            Self::Union(cursor) => cursor.advance_to(target),
            Self::Empty=>{}
        }
    }

    pub fn positions_into(&self, out: &mut Vec<u32>) -> io::Result<()> {
        match self {
            Self::Slice(cursor) => {
                out.clear();
                if let Some(posting) = cursor.current() {
                    out.extend_from_slice(&posting.positions);
                }
                Ok(())
            }
            Self::Block(cursor) => cursor.positions_into(out),
            Self::Union(cursor) => match cursor.on_current().next() {
                Some(child) => child.positions_into(out),
                None => {
                    out.clear();
                    Ok(())
                }
            },
            Self::Empty=>{
                out.clear();
                Ok(())
            }
        }
    }

    pub fn offsets_into(&self, out: &mut Vec<(u32, u32)>) -> io::Result<()> {
        match self {
            Self::Slice(cursor) => {
                out.clear();
                if let Some(posting) = cursor.current() {
                    out.extend_from_slice(&posting.offsets);
                }
                Ok(())
            }
            Self::Block(cursor) => cursor.offsets_into(out),
            Self::Union(cursor) => match cursor.on_current().next() {
                Some(child) => child.offsets_into(out),
                None => {
                    out.clear();
                    Ok(())
                }
            },
            Self::Empty=>{
                out.clear();
                Ok(())
            }
        }
    }
}

pub struct UnionCursor<'a>{
    children: Vec<TermCursor<'a>>,
    deleted: &'a DeleteSet,
    current: Option<DocId>,
}
impl <'a> UnionCursor<'a>{
    pub fn new(mut children: Vec<TermCursor<'a>>, deleted: &'a DeleteSet) -> Self {
        children.retain(|child| !child.is_exhausted());
        let mut cursor = Self {
            children,
            deleted,
            current: None,
        };
        cursor.settle();
        cursor
    }

    /// The children on the current document, in source order (memtable first).
    fn on_current(&self) -> impl Iterator<Item = &TermCursor<'a>> {
        let current = self.current;
        self.children
            .iter()
            .filter(move |child| current.is_some() && child.doc_id() == current)
    }

    /// Sets `current` to the smallest live doc id among the children.
    fn settle(&mut self) {
        loop {
            self.current = self.children.iter().filter_map(TermCursor::doc_id).min();
            match self.current {
                Some(doc) if self.deleted.contains(doc) => self.step_past(doc),
                _ => return,
            }
        }
    }

    fn step_past(&mut self, doc: DocId) {
        for child in &mut self.children {
            if child.doc_id() == Some(doc) {
                child.next();
            }
        }
        self.children.retain(|child| !child.is_exhausted());
    }

    fn next(&mut self) {
        if let Some(doc) = self.current {
            self.step_past(doc);
            self.settle();
        }
    }

    fn advance_to(&mut self, target: DocId) {
        if self.current.is_some_and(|doc| doc < target) {
            for child in &mut self.children {
                child.advance_to(target);
            }
            self.children.retain(|child| !child.is_exhausted());
            self.settle();
        }
    }
}
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
