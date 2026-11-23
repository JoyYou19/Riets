//Conjunction: every child must be on the document.
use core_index::{search::SearchStats, types::DocId};

use crate::scorer::{BoxScorer, Scorer};

//a document matches only when every child is on it.
pub(crate) struct AndScorer<'a> {
    children: Vec<BoxScorer<'a>>,
    current: Option<DocId>,
    upper_bound: u64,
}

impl<'a> AndScorer<'a> {
    //Builds the conjunction and lines the children up on their first common document.
    pub(crate) fn new(children: Vec<BoxScorer<'a>>) -> Self {
        let upper_bound = children.iter().map(|child| child.upper_bound()).sum();
        let mut this = Self {
            children,
            current: None,
            upper_bound,
        };
        this.current = this.next_common(None);
        this
    }

    //The next document every child is on, after `after`.
    fn next_common(&mut self, after: Option<DocId>) -> Option<DocId> {
        if after.is_some() {
            for child in self.children.iter_mut() {
                child.next();
            }
        }
        loop {
            let mut target = None;
            for child in self.children.iter() {
                let doc = child.doc()?;
                target = Some(target.map_or(doc, |known: DocId| known.max(doc)));
            }
            let target = target?;
            for child in self.children.iter_mut() {
                child.advance_to(target);
            }
            if self
                .children
                .iter()
                .all(|child| child.doc() == Some(target))
            {
                return Some(target);
            }
        }
    }
}

impl Scorer for AndScorer<'_> {
    fn doc(&self) -> Option<DocId> {
        self.current
    }

    //Move past the current common document.
    fn next(&mut self) {
        self.current = self.next_common(self.current);
    }

    //Move forward to the first common document >= `target`.
    fn advance_to(&mut self, target: DocId) {
        if self.current.is_some_and(|doc| doc < target) {
            for child in self.children.iter_mut() {
                child.advance_to(target);
            }
            self.current = self.next_common(None);
        }
    }

    fn weight(&self) -> f32 {
        self.children.iter().map(|child| child.weight()).sum()
    }

    //Sum every child's score on `doc`.
    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        self.children
            .iter_mut()
            .map(|child| child.score(stats, doc))
            .sum()
    }

    //A safe ceiling: every child's ceiling added up.
    fn upper_bound(&self) -> u64 {
        self.upper_bound
    }
}
