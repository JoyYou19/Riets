use ahash::{ HashSet, HashSetExt };
use levenshtein_automata::{ Distance, LevenshteinAutomatonBuilder };
use roaring::RoaringTreemap;

use crate::{
    fuzzy::{
        DEFAULT_MAX_CANDIDATES,
        FuzzyExpansion,
        FuzzyOptions,
        LevenshteinAutomaton,
        select_expansions,
        split_prefix,
    },
    numeric_values::{ NumericBound, NumericValue },
    posting::PostingList,
    types::{ ArrayRowId, DocId, XPathId },
    wildcard::WildcardPattern,
};

#[derive(Debug, Clone)]
pub struct TermPostings {
    pub postings: PostingList,
    pub doc_freq: u32,
    pub max_weight: u16,
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
        opts: FuzzyOptions
    ) -> Vec<FuzzyExpansion> {
        //one automaton for k = 0, 1 and 2, so all three use the same rules
        //(transpositions, unicode, prefix lock)
        let aut = LevenshteinAutomaton::new(&[], term, opts.max_edits, opts.prefix_length);

        let mut candidates: Vec<(String, u8, u32)> = Vec::new();
        for t in self.terms(xpath) {
            let t: &str = &t;
            let Some(edits) = aut.distance_to(t) else {
                continue;
            };
            let doc_freq = self.doc_freq(t, xpath);
            if doc_freq == 0 {
                continue;
            }
            candidates.push((t.to_owned(), edits, doc_freq));
            if candidates.len() >= DEFAULT_MAX_CANDIDATES {
                break;
            }
        }

        //keeps the exact term if it exists, then the best corrections within
        //SCORE_MARGIN of the top score, up to max_expansions
        candidates
            .into_iter()
            .map(|(t, edits, doc_freq)| FuzzyExpansion::new(t, edits, doc_freq))
            .collect()
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
        hi: Option<NumericBound>
    ) -> PostingList;

    //quick docid+xpath->numeric_value
    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue>;

    //for min max value for order-blended
    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>);
}

pub trait SearchReader: SearchIndex + SearchStats + SearchNumeric {}

impl<T> SearchReader for T where T: SearchIndex + SearchStats + SearchNumeric {}
