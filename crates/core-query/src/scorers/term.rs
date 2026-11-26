//! One query form in one field: a term's list, a phrase's lists, or an "any list" set.

use core_index::{
    posting::cursor::TermCursor,
    search::SearchStats,
    types::{DocId, XPathId},
};

use crate::{
    bm25::{bm25_idf, bm25_score_scaled, bm25_upper_bound, weighted},
    executor::phrase_matches,
    scorer::Scorer,
    wand::FormRef,
};

/// One query form in one field, walked as a single cursor.
pub(crate) struct TermScorer<'a> {
    idf: f32,
    cursors: Vec<TermCursor<'a>>,
    doc_freqs: Vec<u32>,
    xpath: XPathId,
    doc_count: u64,
    avg_doc_len: f64,
    weight: u16,
    upper_bound: u64,
    // any list is enough (synonym typed words), or all of them must be on the doc
    any: bool,
    // the document every cursor is on, `None` once there is none left
    doc: Option<DocId>,
    phrase: Option<Vec<u32>>,
    positions: Vec<Vec<u32>>,
}

impl<'a> TermScorer<'a> {
    /// `None` when the form cannot match (no lists, or a required list is empty).
    pub(crate) fn new<S: SearchStats>(stats: &S, form: FormRef<'a>) -> Option<Self> {
        let cannot_match = if form.any {
            form.lists.iter().all(|list| list.cursor.is_exhausted())
        } else {
            form.lists.iter().any(|list| list.cursor.is_exhausted())
        };
        if form.lists.is_empty() || cannot_match {
            return None;
        }
        let doc_count = stats.doc_count(form.xpath);
        let upper_bound = form
            .lists
            .iter()
            .map(|list| bm25_upper_bound(list.max_weight, doc_count, list.doc_freq))
            .fold(0u64, u64::saturating_add);
        let rarity = |doc_freq: u32| bm25_idf(doc_freq, doc_count);
        let idf = form.lists.iter().map(|list| rarity(list.doc_freq)).sum();
        let doc_freqs = form.lists.iter().map(|list| list.doc_freq).collect();
        let mut scorer = Self {
            idf,
            positions: vec![Vec::new(); form.lists.len()],
            phrase: form.phrase,
            cursors: form.lists.into_iter().map(|list| list.cursor).collect(),
            doc_freqs,
            xpath: form.xpath,
            doc_count,
            avg_doc_len: stats.avg_doc_len(form.xpath),
            weight: form.weight,
            upper_bound: weighted(upper_bound, form.weight),
            any: form.any,
            doc: None,
        };
        scorer.align();
        Some(scorer)
    }

    /// Moves to the form's next document (any-form: min, all-form: first doc every list is on).
    /// Moves to the form's next document (any-form: min, all-form: first doc every list is on).
    fn align(&mut self) {
        if self.any {
            self.doc = self.cursors.iter().filter_map(TermCursor::doc_id).min();
            return;
        }
        loop {
            let mut target = None;
            for cursor in &self.cursors {
                let Some(doc) = cursor.doc_id() else {
                    self.doc = None;
                    return;
                };
                target = Some(target.map_or(doc, |known: DocId| known.max(doc)));
            }
            let Some(target) = target else {
                self.doc = None;
                return;
            };
            for cursor in &mut self.cursors {
                cursor.advance_to(target);
            }
            if self
                .cursors
                .iter()
                .all(|cursor| cursor.doc_id() == Some(target))
            {
                let Some(offsets) = &self.phrase else {
                    self.doc = Some(target);
                    return;
                };
                // a phrase form: the words must also be in order here
                let read = self
                    .cursors
                    .iter_mut()
                    .zip(self.positions.iter_mut())
                    .all(|(cursor, buffer)| cursor.positions_into(buffer).is_ok());
                let lists: Vec<&[u32]> = self.positions.iter().map(Vec::as_slice).collect();
                if read && phrase_matches(&lists, offsets) {
                    self.doc = Some(target);
                    return;
                }
                if let Some(first) = self.cursors.first_mut() {
                    first.next();
                }
            }
        }
    }

    /// Moves past `doc` when the form is on it, then aligns again.
    fn step_past(&mut self, doc: DocId) {
        if self.doc == Some(doc) {
            if self.any {
                for cursor in &mut self.cursors {
                    if cursor.doc_id() == Some(doc) {
                        cursor.next();
                    }
                }
            } else if let Some(first) = self.cursors.first_mut() {
                first.next();
            }
            self.align();
        }
    }
}

impl Scorer for TermScorer<'_> {
    fn doc(&self) -> Option<DocId> {
        self.doc
    }

    fn next(&mut self) {
        if let Some(doc) = self.doc {
            self.step_past(doc);
        }
    }

    fn advance_to(&mut self, target: DocId) {
        if self.doc.is_some_and(|doc| doc < target) {
            for cursor in &mut self.cursors {
                cursor.advance_to(target);
            }
            self.align();
        }
    }

    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        let doc_len = stats.doc_len(doc, self.xpath);
        let mut score = 0u64;
        for (cursor, &doc_freq) in self.cursors.iter_mut().zip(&self.doc_freqs) {
            if cursor.doc_id() != Some(doc) {
                continue;
            }
            score = score.saturating_add(bm25_score_scaled(
                cursor.term_freq(),
                cursor.weight(),
                doc_len,
                self.avg_doc_len,
                self.doc_count,
                doc_freq,
            ));
        }
        weighted(score, self.weight)
    }

    fn upper_bound(&self) -> u64 {
        self.upper_bound
    }

    fn weight(&self) -> f32 {
        self.idf
    }

    fn doc_count(&self) -> u64 {
        self.doc_count
    }
    fn block_bound(&self) -> Option<(u64, DocId)> {
        let mut bound = 0u64;
        let mut last: Option<DocId> = None;
        for (cursor, &doc_freq) in self.cursors.iter().zip(&self.doc_freqs) {
            // memory lists, unions and unknown document lengths have no block bound
            let entry = cursor.block_entry().filter(|entry| entry.min_doc_len > 0)?;
            bound = bound.saturating_add(bm25_score_scaled(
                entry.max_tf,
                entry.max_weight,
                Some(entry.min_doc_len),
                self.avg_doc_len,
                self.doc_count,
                doc_freq,
            ));
            last = Some(last.map_or(entry.last_doc_id, |known| known.min(entry.last_doc_id)));
        }
        Some((weighted(bound, self.weight), last?))
    }
}
