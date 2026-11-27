use std::io;

use crate::{
    disk::{ block_cursor::BlockCursor, format::SkipEntry },
    posting::{ DeleteSet, Posting, PostingList },
    types::{ DocId, position_start },
};
#[allow(
    variant_size_differences,
    reason = "32 bytes in total; boxing `Slice` would add an allocation per term on the search hot path"
)]
pub enum TermCursor<'a> {
    Empty,
    //a list in memory
    Slice(PostingCursor<'a>),
    //one disk
    Block(Box<BlockCursor<'a>>),
    /// Several sources merged by doc id, with deleted documents skipped.
    Union(Box<UnionCursor<'a>>),
}
impl TermCursor<'_> {
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        match self {
            Self::Slice(cursor) => cursor.is_exhausted(),
            Self::Block(cursor) => cursor.is_exhausted(),
            Self::Union(cursor) => cursor.current.is_none(),
            Self::Empty => true,
        }
    }
    #[must_use]
    pub fn doc_id(&self) -> Option<DocId> {
        match self {
            Self::Slice(cursor) => cursor.doc_id(),
            Self::Block(cursor) => cursor.doc_id(),
            Self::Union(cursor) => cursor.current,
            Self::Empty => None,
        }
    }

    pub fn term_freq(&mut self) -> u32 {
        match self {
            Self::Slice(cursor) =>
                cursor
                    .current()
                    .map_or(0, |posting| {
                        u32::try_from(posting.positions.len()).unwrap_or(u32::MAX)
                    }),
            Self::Block(cursor) => cursor.term_freq(),
            Self::Union(cursor) =>
                cursor.on_current_mut().map(TermCursor::term_freq).max().unwrap_or(0),
            Self::Empty => 0,
        }
    }

    pub fn weight(&mut self) -> u16 {
        match self {
            Self::Slice(cursor) => cursor.current().map_or(0, |posting| posting.weight),
            Self::Block(cursor) => cursor.weight(),
            Self::Union(cursor) =>
                cursor.on_current_mut().map(TermCursor::weight).max().unwrap_or(0),
            Self::Empty => 0,
        }
    }
    pub fn next(&mut self) {
        match self {
            Self::Slice(cursor) => cursor.next(),
            Self::Block(cursor) => cursor.next(),
            Self::Union(cursor) => cursor.next(),
            Self::Empty => {}
        }
    }

    pub fn advance_to(&mut self, target: DocId) {
        match self {
            Self::Slice(cursor) => cursor.advance_to(target),
            Self::Block(cursor) => cursor.advance_to(target),
            Self::Union(cursor) => cursor.advance_to(target),
            Self::Empty => {}
        }
    }

    pub fn positions_into(&mut self, out: &mut Vec<u32>) -> io::Result<()> {
        match self {
            Self::Slice(cursor) => {
                out.clear();
                if let Some(posting) = cursor.current() {
                    out.extend_from_slice(&posting.positions);
                }
                Ok(())
            }
            Self::Block(cursor) => cursor.positions_into(out),
            Self::Union(cursor) => if let Some(child) = cursor.on_current_mut().next() {
                child.positions_into(out)
            } else {
                out.clear();
                Ok(())
            }
            Self::Empty => {
                out.clear();
                Ok(())
            }
        }
    }
    /// The skip entry of the block the cursor is in; `None` for cursors without blocks
    /// (memory lists, unions), which then use the term-wide bound.
    #[must_use]
    pub fn block_entry(&self) -> Option<SkipEntry> {
        match self {
            Self::Block(cursor) if !cursor.is_exhausted() => Some(*cursor.block_entry()),
            _ => None,
        }
    }
    pub fn offsets_into(&mut self, out: &mut Vec<(u32, u32)>) -> io::Result<()> {
        match self {
            Self::Slice(cursor) => {
                out.clear();
                if let Some(posting) = cursor.current() {
                    out.extend_from_slice(&posting.offsets);
                }
                Ok(())
            }
            Self::Block(cursor) => cursor.offsets_into(out),
            Self::Union(cursor) => if let Some(child) = cursor.on_current_mut().next() {
                child.offsets_into(out)
            } else {
                out.clear();
                Ok(())
            }
            Self::Empty => {
                out.clear();
                Ok(())
            }
        }
    }
    pub fn advance_to_position(&mut self, target: u32) -> io::Result<bool> {
        match self {
            Self::Slice(cursor) => Ok(cursor.advance_to_position(target)),
            Self::Block(cursor) => cursor.advance_to_position(target),
            Self::Union(_) | Self::Empty => Ok(false),
        }
    }

    #[must_use]
    pub fn current_position(&self) -> Option<u32> {
        match self {
            Self::Slice(cursor) => cursor.current_position(),
            Self::Block(cursor) => cursor.current_position(),
            Self::Union(_) | Self::Empty => None,
        }
    }
}

pub struct UnionCursor<'a> {
    children: Vec<TermCursor<'a>>,
    deleted: &'a DeleteSet,
    current: Option<DocId>,
}
impl<'a> UnionCursor<'a> {
    #[must_use]
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

    fn on_current_mut(&mut self) -> impl Iterator<Item = &mut TermCursor<'a>> {
        let current = self.current;
        self.children.iter_mut().filter(move |child| current.is_some() && child.doc_id() == current)
    }
    /// Sets `current` to the smallest live doc id among the children.
    fn settle(&mut self) {
        loop {
            self.current = self.children.iter().filter_map(TermCursor::doc_id).min();
            match self.current {
                Some(doc) if self.deleted.contains(doc) => self.step_past(doc),
                _ => {
                    return;
                }
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
    position_index: usize,
}

impl<'a> PostingCursor<'a> {
    #[must_use]
    pub fn new(list: &'a PostingList) -> Self {
        Self {
            postings: list.items(),
            index: 0,
            position_index: 0,
        }
    }

    pub const fn next(&mut self) {
        if !self.is_exhausted() {
            self.index += 1;
            self.position_index = 0;
        }
    }

    // Returns whether the cursor has moved past the final posting
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.index >= self.postings.len()
    }

    // current doc_id of the cursor
    #[must_use]
    pub fn doc_id(&self) -> Option<DocId> {
        self.postings.get(self.index).map(|posting| posting.doc_id)
    }

    // Returns the complete posting at the current cursor position,
    // gives the specific posting instead of just doc_id
    #[must_use]
    pub fn current(&self) -> Option<&'a Posting> {
        self.postings.get(self.index)
    }

    pub fn advance_to(&mut self, target: DocId) {
        let Some(rest) = self.postings.get(self.index..) else {
            return;
        };
        if rest.first().is_none_or(|posting| posting.doc_id >= target) {
            return;
        }

        // grow the search range exponentially until we pass target or reach the end
        let mut bound = 1usize;
        while rest.get(bound).is_some_and(|posting| posting.doc_id < target) {
            bound = bound.saturating_mul(2);
        }

        // the first posting >= target lies in rest[bound / 2 ..= bound]
        let window = rest.get(bound / 2..rest.len().min(bound + 1)).unwrap_or_default();
        self.index += bound / 2 + window.partition_point(|posting| posting.doc_id < target);
        self.position_index = 0;
    }
    pub fn advance_to_position(&mut self, target: u32) -> bool {
        let Some(posting) = self.postings.get(self.index) else {
            return false;
        };
        while let Some(&position) = posting.positions.get(self.position_index) {
            if position_start(position) >= target {
                return true;
            }
            self.position_index += 1;
        }
        false
    }

    /// Returns the current position without advancing.
    #[must_use]
    pub fn current_position(&self) -> Option<u32> {
        self.postings
            .get(self.index)
            .and_then(|p| p.positions.get(self.position_index))
            .copied()
    }
}
