//! The query layer's scoring interface.
//! One `Scorer` per query node (term, AND, OR, ...). A scorer is a forward-only stream of
//! documents with a score and a safe upper bound, so the top-k collector can skip cheaply.
use core_index::{search::SearchStats, types::DocId};
use roaring::RoaringTreemap;

use core_index::{
    num::u64_to_f64, posting::PostingList, search::SearchStats, types::{DocId, XPathId},
};
use core_timing::timed;

///A stream of documents with scores — the unit the top-k collector works on.
pub(crate) trait Scorer {
    /// The document the scorer sits on, or `None` when it is exhausted.
    fn doc(&self) -> Option<DocId>;

    /// Move past the current document.
    fn next(&mut self);

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;
const SCORE_SCALE: f64 = 1000.0;

/// BM25's idf, in f64: how rare a term is among `doc_count` documents. f64 keeps rare terms
/// apart in large indexes; in f32 the counts lose their last digits above ~16.7M documents.
fn idf(doc_frequency: u32, doc_count: u64) -> f64 {
    let n = u64_to_f64(doc_count);
    let df = f64::from(doc_frequency).min(n);
    if n <= 0.0 || df <= 0.0 {
        return 0.0;
    }
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

/// BM25's idf: how rare a term is among `doc_count` documents.
pub(crate) fn bm25_idf(doc_frequency: u32, doc_count: u64) -> f32 {
    idf(doc_frequency, doc_count) as f32
}

/// `doc_len` is None when the index has no length for the document: the field's average is
/// used, exactly (it used to be cut to a whole number).
fn bm25_score(
    tf: u32,
    weight: u16,
    doc_len: Option<u32>,
    avg_doc_len: f64,
    doc_count: u64,
    doc_frequency: u32,
) -> f64 {
    if tf == 0 || doc_count == 0 || doc_frequency == 0 || avg_doc_len <= 0.0 {
        return 0.0;
    }
    let tf = f64::from(tf);
    let dl = doc_len.map_or(avg_doc_len, f64::from);
    let norm = 1.0 - BM25_B + BM25_B * (dl / avg_doc_len);
    let tf_component = (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm);

    //INFO: noneemu + 1.0
    f64::from(weight) * idf(doc_frequency, doc_count) * tf_component
}

// I just put this function here so we automatically multiply, I can already see ways of me messing
// this up in the future
pub(crate) fn bm25_score_scaled(
    tf: u32,
    weight: u16,
    doc_len: Option<u32>,
    avg_doc_len: f64,
    doc_count: u64,
    doc_freq: u32,
) -> u64 {
    (bm25_score(tf, weight, doc_len, avg_doc_len, doc_count, doc_freq) * SCORE_SCALE) as u64
}

// A safe upper bound on the BM25 score this term can contribute to any document: the highest
// weight, the term's idf, and the largest the tf part can get (k1 + 1). Uses the same `idf` as
// the real score, so it can never fall below one.
pub(crate) fn bm25_upper_bound(max_weight: u16, doc_count: u64, doc_frequency: u32) -> u64 {
    if doc_count == 0 || doc_frequency == 0 || max_weight == 0 {
        return 0;
    }
    let max_tf_component = BM25_K1 + 1.0;

    //INFO: nonemu + 1.0
    let max_score = f64::from(max_weight) * idf(doc_frequency, doc_count) * max_tf_component;

    (max_score * SCORE_SCALE).ceil() as u64
}

#[timed(search)]
pub(crate) fn score_term_into<S: SearchStats>(
    stats: &S,
    postings: &PostingList,
    xpath: XPathId,
    true_df: u32,
    doc_len: &mut HashMap<DocId, Option<u32>>,
    out: &mut Vec<ScoredPosting>,
) {
    let doc_count = stats.doc_count(xpath);
    let doc_frequency = true_df;
    let avg_doc_len = stats.avg_doc_len(xpath);

    for posting in postings.items().iter().filter(|p| !p.positions.is_empty()) {
         let dl = *doc_len
            .entry(posting.doc_id)
            .or_insert_with(|| stats.doc_len(posting.doc_id, xpath));

        let score = bm25_score_scaled(
            u32::try_from(posting.positions.len()).unwrap_or(u32::MAX),
            posting.weight,
            dl,
            avg_doc_len,
            doc_count,
            doc_frequency,
        );

    /// The top `k` documents, best first (default: walk every one).
    fn top_k(
        &mut self,
        stats: &dyn SearchStats,
        k: usize,
        restrict: Option<&RoaringTreemap>,
    ) -> Vec<WandHit> {
        crate::collector::collect_all(stats, self, k, restrict)
    }
}

/// A boxed scorer borrowing from the index for `'a`.
pub(crate) type BoxScorer<'a> = Box<dyn Scorer + 'a>;
