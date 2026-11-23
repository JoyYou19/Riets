//! A synonym group: the union of its forms, scored by the best form per field.
use core_index::{search::SearchStats, types::DocId};

use crate::{
    bm25::best_per_field,
    scorer::{BoxScorer, Scorer},
};

/// A clause: one or more forms; the best form in each field wins.
pub(crate) struct SynonymScorer<'a> {
    forms: Vec<(usize, BoxScorer<'a>)>,
    upper_bound: u64,
    weight: f32,
}

impl<'a> SynonymScorer<'a> {
    /// `None` when no form can match; otherwise the union of the forms' documents.
    pub(crate) fn new(forms: Vec<(usize, BoxScorer<'a>)>) -> Option<Self> {
        if forms.is_empty() {
            return None;
        }
        let upper_bound = best_per_field(
            forms
                .iter()
                .map(|(field, form)| (*field, form.upper_bound())),
        );
        let largest = forms
            .iter()
            .map(|(_, form)| form.doc_count())
            .max()
            .unwrap_or(0);
        let weight = forms
            .iter()
            .filter(|(_, form)| form.doc_count() == largest)
            .map(|(_, form)| form.weight())
            .reduce(f32::min)
            .unwrap_or(0.0);
        Some(Self {
            forms,
            upper_bound,
            weight,
        })
    }
}

impl Scorer for SynonymScorer<'_> {
    fn doc(&self) -> Option<DocId> {
        self.forms.iter().filter_map(|(_, form)| form.doc()).min()
    }

    fn next(&mut self) {
        if let Some(doc) = self.doc() {
            for (_, form) in self.forms.iter_mut() {
                if form.doc() == Some(doc) {
                    form.next();
                }
            }
        }
    }

    fn advance_to(&mut self, target: DocId) {
        for (_, form) in self.forms.iter_mut() {
            form.advance_to(target);
        }
    }

    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        best_per_field(
            self.forms
                .iter_mut()
                .filter(|(_, form)| form.doc() == Some(doc))
                .map(|(field, form)| (*field, form.score(stats, doc))),
        )
    }

    fn upper_bound(&self) -> u64 {
        self.upper_bound
    }

    fn weight(&self) -> f32 {
        self.weight
    }
}
