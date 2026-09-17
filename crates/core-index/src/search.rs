use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder};

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions, candidates_within_one, split_prefix},
    numeric_columns::{NumericBound, NumericValue},
    posting::PostingList,
    types::{DocId, XPathId},
    wildcard::WildcardPattern,
};

// Every searchable segment should implement this, simply functions that we are going to need for
// every type of segment either it is Immutable, Snapshot or in Memory
pub trait SearchIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList;
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList;
    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList;

    //All indexed terms for a field (for the fuzzy automaton scan).
    fn terms(&self, xpath: XPathId) -> Vec<String>;
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32;

    //Docs whose indexed term is within `opts.max_edits` of `term`.
    fn fuzzy_expansions(
        &self,
        term: &str,
        xpath: XPathId,
        opts: FuzzyOptions,
    ) -> Vec<FuzzyExpansion> {
        let mut out = vec![FuzzyExpansion::new(term, 0, 0)];

        if opts.max_edits == 0 {
            return out;
        }

        let (prefix, suffix) = split_prefix(term, opts.prefix_length);

        if suffix.is_empty() {
            return out;
        }

        if opts.max_edits == 1 {
            // d=1: candidate generation is bounded and fast.
            for candidate in candidates_within_one(suffix) {
                out.push(FuzzyExpansion::new(format!("{prefix}{candidate}"), 1, 0));
            }
        } else {
            let builder = LevenshteinAutomatonBuilder::new(opts.max_edits, true);
            let dfa = builder.build_dfa(suffix);

            for t in self.terms(xpath) {
                if let Some(rest) = t.strip_prefix(prefix)
                    && let Distance::Exact(edits) = dfa.eval(rest)
                {
                    out.push(FuzzyExpansion::new(t, edits, 0));
                }
            }
        }

        out
    }

    //Docs whose indexed term is within `opts.max_edits` of `term`.
    fn lookup_fuzzy(&self, term: &str, xpath: XPathId, opts: FuzzyOptions) -> PostingList {
        let mut items = Vec::new();

        for expansion in self.fuzzy_expansions(term, xpath, opts) {
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

    fn avg_doc_len(&self, xpath: XPathId) -> f32 {
        let count = self.doc_count(xpath);

        if count == 0 {
            return 0.0;
        }

        self.total_doc_len(xpath) as f32 / count as f32
    }
}

pub trait SearchColumns {
    //docs within the [lo, hi]
    fn column_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>,
    ) -> PostingList;

    //all (doc_id, value) pairs in a column sorting pirposes
    fn column_values(&self, xpath: XPathId) -> Vec<(DocId, NumericValue)>;
}

pub trait SearchReader: SearchIndex + SearchStats + SearchColumns {}

impl<T> SearchReader for T where T: SearchIndex + SearchStats + SearchColumns {}
