use std::{collections::HashMap, sync::Arc};

use core_index::{
    posting::PostingList,
    search::SearchStats,
    types::{DocId, XPathId},
};
use core_timing::timed;

use crate::ScoredPosting;

/*
* Turns postings into scored postings based on whatever criteria
*/

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;
const SCORE_SCALE: f32 = 1000.0;

pub fn bm25_score(
    tf: u32,
    weight: u16,
    doc_len: u32,
    avg_doc_len: f32,
    doc_count: u64,
    doc_frequency: u32,
) -> f32 {
    if tf == 0 || doc_count == 0 || doc_frequency == 0 || avg_doc_len <= 0.0 {
        return 0.0;
    }

    let n = doc_count as f32;
    let df = doc_frequency as f32;
    let tf = tf as f32;
    let dl = doc_len as f32;

    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

    let norm = 1.0 - BM25_B + BM25_B * (dl / avg_doc_len);

    let tf_component = (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm);

    weight as f32 * idf * (tf_component + 1.0)
}

// I just put this function here so we automatically multiply, I can already see ways of me messing
// this up in the future
pub fn bm25_score_scaled(
    tf: u32,
    weight: u16,
    doc_len: u32,
    avg_doc_len: f32,
    doc_count: u64,
    doc_freq: u32,
) -> u64 {
    (bm25_score(tf, weight, doc_len, avg_doc_len, doc_count, doc_freq) * SCORE_SCALE) as u64
}

// Returns a safe upper bound on the BM25 score that this term can contribute to any document. I
// took this code from our generative friend, and honestly, I have no clue how it measures
// anything, but if this is what they use in ElasticSearch then we use it as well. At least for
// now.
pub fn bm25_upper_bound(max_weight: u16, doc_count: u64, doc_frequency: u32) -> u64 {
    if doc_count == 0 || doc_frequency == 0 || max_weight == 0 {
        return 0;
    }

    let n = doc_count as f32;
    let df = doc_frequency as f32;

    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

    let max_tf_component = BM25_K1 + 1.0;

    let max_score = max_weight as f32 * idf * (max_tf_component + 1.0);

    (max_score * SCORE_SCALE).ceil() as u64
}

// fn trace_bm25() -> bool {
//     std::env::var_os("CORELAMO_TRACE_BM25").is_some()
// }

#[timed(search)]
pub fn score_term_into<S: SearchStats>(
    stats: &S,
    postings: &PostingList,
    xpath: XPathId,
    true_df: f32,
    doc_len: &mut HashMap<DocId, f32>,
    out: &mut Vec<ScoredPosting>,
) {
    let doc_count = stats.doc_count(xpath);
    let doc_frequency = true_df as u32;
    let avg_doc_len = stats.avg_doc_len(xpath);

    for posting in postings.items().iter().filter(|p| !p.positions.is_empty()) {
        let dl = *doc_len.entry(posting.doc_id).or_insert_with(|| {
            stats
                .doc_len(posting.doc_id, xpath)
                .unwrap_or(avg_doc_len as u32) as f32
        });

        let score = bm25_score_scaled(
            posting.positions.len() as u32,
            posting.weight,
            dl as u32,
            avg_doc_len,
            doc_count,
            doc_frequency,
        );

        out.push(ScoredPosting {
            doc_id: posting.doc_id,
            positions: Arc::from(posting.positions.as_slice()),
            score,
            matched_terms: 1,
            density: 1.0,
        });
    }
}

#[must_use]
#[timed(search)]
pub fn score_term_hybrid<S: SearchStats>(
    stats: &S,
    postings: &PostingList,
    xpath: XPathId,
    true_df: f32,
) -> Vec<ScoredPosting> {
    let mut out = Vec::with_capacity(postings.len());
    let mut doc_len = HashMap::new();

    score_term_into(stats, postings, xpath, true_df, &mut doc_len, &mut out);

    out
}

pub fn fuzzy_decay(edits: u8) -> f32 {
    1.0 / (1.0 + edits as f32)
}
