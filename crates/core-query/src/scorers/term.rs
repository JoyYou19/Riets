//! A single term's posting list in one field: the leaf of the scorer tree.
use core_index::{
    posting::cursor::TermCursor,
    search::SearchStats,
    types::{DocId, XPathId},
};

use crate::{
    bm25::{bm25_idf, bm25_score_scaled, bm25_upper_bound},
    scorer::Scorer,
};

pub struct TermScorer<'a> {
    cursor: TermCursor<'a>,
    xpath: XPathId,
    doc_freq: u32,
    max_weight: u16,
    doc_count: u64,
    avg_doc_len: f32,
    idf: f32,
}

impl<'a> TermScorer<'a> {
    //Wraps a posting-list cursor and caches the field stats BM25 needs.
    pub fn new(
        cursor: TermCursor<'a>,
        xpath: XPathId,
        doc_freq: u32,
        max_weight: u16,
        stats: &dyn SearchStats,
    ) -> Self {
        let doc_count = stats.doc_count(xpath);
        let avg_doc_len = stats.avg_doc_len(xpath);
        Self {
            cursor,
            xpath,
            doc_freq,
            max_weight,
            doc_count,
            avg_doc_len,
            idf: bm25_idf(doc_freq, doc_count),
        }
    }
}

impl Scorer for TermScorer<'_> {
    fn doc(&self) -> Option<DocId> {
        self.cursor.doc_id()
    }

    fn next(&mut self) {
        self.cursor.next();
    }

    fn advance_to(&mut self, target: DocId) {
        self.cursor.advance_to(target);
    }

    fn score(&mut self, stats: &dyn SearchStats, doc: DocId) -> u64 {
        //Only the document we are sitting on can be scored.
        if self.cursor.doc_id() != Some(doc) {
            return 0;
        }
        let doc_len = stats
            .doc_len(doc, self.xpath)
            .unwrap_or(self.avg_doc_len as u32);
        bm25_score_scaled(
            self.cursor.term_freq(),
            self.cursor.weight(),
            doc_len,
            self.avg_doc_len,
            self.doc_count,
            self.doc_freq,
        )
    }

    //A safe ceiling for WAND the whole-list best case for the term's max weight.
    fn upper_bound(&self) -> u64 {
        bm25_upper_bound(self.max_weight, self.doc_count, self.doc_freq)
    }

    //The term's idf, used by the coverage threshold.
    fn weight(&self) -> f32 {
        self.idf
    }
}
