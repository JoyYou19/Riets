use roaring::RoaringTreemap;

use crate::{
    fuzzy::{
        FuzzyExpansion, FuzzyOptions, LevenshteinAutomaton, rank_candidates,
    }, numeric_values::{NumericBound, NumericValue}, posting::{PostingList, cursor::TermCursor}, types::{ArrayRowId, DocId, XPathId}, wildcard::WildcardPattern,
};

#[derive(Debug, Clone)]
pub struct TermPostings {
    pub postings: PostingList,
    pub doc_freq: u32,
    pub max_weight: u16,
}
pub struct TermCursorInfo<'a> {
    pub cursor: TermCursor<'a>,
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
            doc_freq: u32::try_from(postings.len()).unwrap_or(u32::MAX),
            max_weight: postings.max_weight(),
            postings,
        }
    }
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList;
    fn prefix_terms(&self, prefix: &str, xpath: XPathId) -> Vec<(String, u32)>;
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList;

    fn term_cursor(&self, term: &str, xpath: XPathId) -> Option<TermCursorInfo<'_>>;
    //All indexed terms for a field (for the fuzzy automaton scan).
    fn terms(&self, xpath: XPathId) -> Vec<String>;
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32;

    fn is_deleted(&self, _doc_id: DocId) -> bool {
        false
    }
    /// Every live document id. The default checks each id; an index with a delete set
    /// overrides it with one bulk subtraction.
    fn live_doc_ids(&self) -> Option<RoaringTreemap> {
        None
    }

    fn resolve_array_rows(&self, rows: &RoaringTreemap) -> RoaringTreemap;

    fn element_index_of_row(&self, row: ArrayRowId) -> Option<u32>;

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId>;
    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId>;

    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool>;
    fn union_bool_ids(&self, xpath: XPathId, value: bool, out: &mut RoaringTreemap);
    //words within max_edits of input
    fn fuzzy_expansions(
    &self,
    term: &str,
    xpath: XPathId,
    opts: FuzzyOptions,
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
        if doc_freq > 0 {
            candidates.push((t.to_owned(), edits, doc_freq));
        }
    }
    rank_candidates(candidates)
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
    fn doc_ids_into(&self, term: &str, xpath: XPathId, out: &mut RoaringTreemap) {
        let ids = self
            .lookup(term, xpath)
            .items()
            .iter()
            .map(|p| p.doc_id)
            .collect::<Vec<_>>();
        if let Ok(set) = RoaringTreemap::from_sorted_iter(ids) {
            *out |= set;
        }
    }
}

// How are these documents going to be scored? Used for BM25, is needed for the math equation
pub trait SearchStats {
    fn doc_count(&self, xpath: XPathId) -> u64;
    fn total_doc_len(&self, xpath: XPathId) -> u64;
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32>;
    fn doc_range(&self) -> Option<(DocId, DocId)>;
    fn avg_doc_len(&self, xpath: XPathId) -> f64 {
        let count = self.doc_count(xpath);
        if count == 0 {
            return 0.0;
        }
        crate::num::u64_to_f64(self.total_doc_len(xpath)) / crate::num::u64_to_f64(count)
    }
}

pub trait SearchNumeric {
    //docs within the [lo, hi], ascending
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
        out: &mut RoaringTreemap,
    );
    //quick docid+xpath->numeric_value
    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue>;

    //for min max value for order-blended
    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>);

    //basically used only for "give latest year movies" when no "search" is used
    fn numeric_top(&self, xpath: XPathId, k: usize, descending: bool)
    -> Vec<(DocId, NumericValue)>;
}

pub trait SearchReader: SearchIndex + SearchStats + SearchNumeric {}

impl<T> SearchReader for T where T: SearchIndex + SearchStats + SearchNumeric {}
