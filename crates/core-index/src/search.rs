use ahash::{HashSet, HashSetExt};
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder};
use roaring::RoaringTreemap;

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions, candidates_within_one, split_prefix},
    numeric_values::{NumericBound, NumericValue},
    posting::PostingList,
    types::{ArrayRowId, DocId, XPathId},
    wildcard::WildcardPattern,
};

#[derive(Debug, Clone)]
pub struct TermPostings {
    pub postings: PostingList,
    pub doc_freq: u32,
    pub max_weight: u16,
}
//vec Search
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub doc_id: DocId,
    /// Cosine similarity in [-1, 1]; higher is closer.
    pub score: f32,
}
/// Keeps the `k` highest-scoring hits, sorted best first.
pub(crate) struct TopK {
    k: usize,
    hits: Vec<VectorHit>,
}

impl TopK {
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            hits: Vec::with_capacity(k.saturating_add(1)),
        }
    }

    pub(crate) fn offer(&mut self, doc_id: DocId, score: f32) {
        if self.k == 0 || score.is_nan() {
            return;
        }
        if self.hits.len() == self.k && score <= self.hits[self.k - 1].score {
            return;
        }
        let pos = self.hits.partition_point(|hit| hit.score >= score);
        self.hits.insert(pos, VectorHit { doc_id, score });
        self.hits.truncate(self.k);
    }

    pub(crate) fn into_sorted(self) -> Vec<VectorHit> {
        self.hits
    }
}

/// Combines per-segment (or per-shard) result lists into one top-k list.
/// If a document appears more than once, its best score is kept.
pub fn merge_hits(lists: Vec<Vec<VectorHit>>, k: usize) -> Vec<VectorHit> {
    let mut all: Vec<VectorHit> = lists.into_iter().flatten().collect();
    all.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id)));
    let mut seen = ahash::HashSet::default();
    all.retain(|hit| seen.insert(hit.doc_id));
    all.truncate(k);
    all
}

// Every searchable segment should implement this, simply functions that we are going to need for
// every type of segment either it is Immutable, Snapshot or in Memory
pub trait SearchIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList;
    fn lookup_term(&self, term: &str, xpath: XPathId) -> TermPostings {
        let postings = self.lookup(term, xpath);
        TermPostings {
            doc_freq: postings.len() as u32,
            max_weight: postings.max_weight(),
            postings,
        }
    }
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList;
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList;

    //All indexed terms for a field (for the fuzzy automaton scan).
    fn terms(&self, xpath: XPathId) -> Vec<String>;
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32;

    fn is_deleted(&self, _doc_id: DocId) -> bool {
        false
    }

    fn resolve_array_rows(&self, rows: &HashSet<ArrayRowId>) -> HashSet<DocId> {
        let _ = rows;
        HashSet::new()
    }
    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId>;
    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId>;

    fn bool_true_ids(&self, xpath: XPathId) -> RoaringTreemap;

    fn bool_false_ids(&self, xpath: XPathId) -> RoaringTreemap;

    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool> {
        if self.bool_true_ids(xpath).contains(doc_id) {
            return Some(true);
        }
        if self.bool_false_ids(xpath).contains(doc_id) {
            return Some(false);
        }
        None
    }

    //words within max_edits of input
    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions,
    ) -> Vec<FuzzyExpansion> {
        //woodoo veids kaa defineet mazy funkkciju
        let existing = |term: String, edits: u8| -> Option<FuzzyExpansion> {
            let doc_freq = self.doc_freq(&term, xpath);

            (doc_freq > 0).then(|| FuzzyExpansion::new(term, edits, doc_freq))
        };

        let mut out = Vec::new();

        //theres a slight, minimal chance that our user wrote "batman" correctly
        out.extend(existing(term.to_string(), 0));

        if opts.max_edits == 0 {
            return out;
        }

        //butman -> b   utman
        let (prefix, suffix) = split_prefix(term, opts.prefix_length);

        if suffix.is_empty() {
            return out;
        }

        //randomly guessing by one character cheap since its like 26*(word_len+2) or sum
        if opts.max_edits == 1 {
            for candidate in candidates_within_one(suffix) {
                out.extend(existing(format!("{prefix}{candidate}"), 1));
            }
        } else {
            //if distance>2 then we compare the already existing words and how far they are from
            //"utman"
            let builder = LevenshteinAutomatonBuilder::new(opts.max_edits, true);
            let dfa = builder.build_dfa(suffix);

            for t in self.terms(xpath) {
                if let Some(rest) = t.strip_prefix(prefix)
                    && let Distance::Exact(edits) = dfa.eval(rest)
                {
                    out.extend(existing(t, edits));
                }
            }
        }

        //sort correctly
        out.sort_by(|a, b| a.term.cmp(&b.term).then_with(|| a.edits.cmp(&b.edits)));
        out.dedup_by(|a, b| a.term == b.term);

        out
    }

    //Docs whose indexed term is within `opts.max_edits` of `term`.
    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let mut items = Vec::new();

        for expansion in self.fuzzy_expansions(term, xpath, opts) {
            //here it checks wether b + "atman"/ "utman"/ "rtman" is in the dictionary (rtman would
            //be skipped)
            items.extend_from_slice(self.lookup(&expansion.term, xpath).items());
        }

        PostingList::from_items(items)
    }
}

// How are these documents going to be scored? Used for BM25, is needed for the math equation
pub trait SearchStats {
    fn doc_count(&self, xpath: XPathId) -> u64;
    fn total_doc_len(&self, xpath: XPathId) -> u64;
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32>;
    fn doc_range(&self) -> Option<(DocId, DocId)>;
    fn avg_doc_len(&self, xpath: XPathId) -> f32 {
        let count = self.doc_count(xpath);

        if count == 0 {
            return 0.0;
        }

        (self.total_doc_len(xpath) as f32) / (count as f32)
    }
}

pub trait SearchNumeric {
    //docs within the [lo, hi], ascending
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> PostingList;

    //quick docid+xpath->numeric_value
    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue>;

    //for min max value for order-blended
    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>);
}

pub trait SearchReader: SearchIndex + SearchStats + SearchNumeric {}

impl<T> SearchReader for T where T: SearchIndex + SearchStats + SearchNumeric {}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_keeps_best_in_order() {
        let mut top = TopK::new(2);
        for (doc, score) in [(1, 0.1), (2, 0.9), (3, 0.5), (4, f32::NAN), (5, 0.7)] {
            top.offer(doc, score);
        }
        let docs: Vec<DocId> = top.into_sorted().iter().map(|h| h.doc_id).collect();
        assert_eq!(docs, vec![2, 5]);
    }

    #[test]
    fn top_k_zero_returns_nothing() {
        let mut top = TopK::new(0);
        top.offer(1, 1.0);
        assert!(top.into_sorted().is_empty());
    }

    #[test]
    fn merge_hits_dedups_and_limits() {
        let a = vec![VectorHit { doc_id: 1, score: 0.9 }, VectorHit { doc_id: 2, score: 0.4 }];
        let b = vec![VectorHit { doc_id: 1, score: 0.8 }, VectorHit { doc_id: 3, score: 0.6 }];
        let merged = merge_hits(vec![a, b], 2);
        assert_eq!(merged, vec![
            VectorHit { doc_id: 1, score: 0.9 },
            VectorHit { doc_id: 3, score: 0.6 },
        ]);
    }
}