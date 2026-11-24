//! Concrete `Scorer` implementations, one per query node.
use core_index::search::SearchStats;

use crate::{scorer::BoxScorer, wand::FormRef};

mod and;
mod or;
mod synonym;
mod term;

pub(crate) use and::AndScorer;
pub(crate) use or::OrScorer;
pub(crate) use synonym::SynonymScorer;
pub(crate) use term::TermScorer;

// Builds a clause scorer: its forms as `TermScorer`s, the best one per field winning.
pub(crate) fn clause_scorer<'a, S: SearchStats>(
    stats: &S,
    forms: Vec<FormRef<'a>>,
) -> Option<BoxScorer<'a>> {
    let mut built: Vec<(usize, BoxScorer<'a>)> = Vec::with_capacity(forms.len());
    for form in forms {
        let field = form.field;
        if let Some(term) = TermScorer::new(stats, form) {
            built.push((field, Box::new(term)));
        }
    }
    SynonymScorer::new(built).map(|synonym| -> BoxScorer<'a> { Box::new(synonym) })
}
