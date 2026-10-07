use std::ops::Range;

use core_timing::timed;

use crate::Query;

/// Extra credit when all neighbouring typed words of a query appear in a field as one phrase:
/// the phrase words' own score in that field, times this, is added to the document.
pub const PHRASE_BOOST: f32 = 5.0;

/// The same for every two neighbouring words, in queries of three or more words, so a
/// document with part of the phrase still ranks above one with the words scattered.
pub const PAIR_BOOST: f32 = 2.0;

/// How far apart two neighbouring query words may stand and still count as together: each
/// must come after the previous one, at most this many positions later. Removed stopwords
/// keep their positions, so "Nutrition and Health" has its two words 2 apart and "List of
/// ambassadors of the United Kingdom" up to 3; a reversed or scattered order never counts.
pub const MAX_GAP: u32 = 3;

#[derive(Debug, Clone)]
pub struct QuerySignal {
    // Query to execute
    pub query: Query,
    // Which clauses of the top-level AND it covers, in order
    pub clauses: Range<usize>,
    // How much this contributes
    pub boost: f32,
    // If true documents must have it
    pub required: bool,
    // boost never filters
    pub rerank_only: bool,
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub retrieval: Query,
    pub signals: Vec<QuerySignal>,
}

pub struct QueryPlanner;

impl QueryPlanner {
    #[timed(search)]
    pub fn plan(query: Query) -> QueryPlan {
        let signals = Self::signals(&query);
        QueryPlan {
            retrieval: query,
            signals,
        }
    }

    /// Phrase signals for a query that is an AND of words. Every run of neighbouring words
    /// (a synonym clause counts as the word that was typed) gets one signal for the whole run
    /// and, from three words on, one for each pair of neighbours.
    ///
    /// Signals for single terms, prefixes, wildcards and whole OR queries are not produced:
    /// they would add a fixed share of a score every match already has, which cannot change
    /// the order, and each would cost another evaluation. Words next to each other is the
    /// one thing retrieval does not score.
    pub fn signals(query: &Query) -> Vec<QuerySignal> {
        let Query::And(parts) = query else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut index = 0;
        while index < parts.len() {
            let start = index;
            let mut words: Vec<String> = Vec::new();
            while let Some(word) = parts.get(index).and_then(typed_word) {
                words.push(word.to_string());
                index += 1;
            }
            if words.len() >= 2 {
                out.push(QuerySignal {
                    query: Query::Phrase(words.clone()),
                    clauses: start..index,
                    boost: PHRASE_BOOST,
                    required: false,
                    rerank_only: true,
                });
                if words.len() >= 3 {
                    for offset in 0..words.len() - 1 {
                        out.push(QuerySignal {
                            query: Query::Phrase(words[offset..offset + 2].to_vec()),
                            clauses: start + offset..start + offset + 2,
                            boost: PAIR_BOOST,
                            required: false,
                            rerank_only: true,
                        });
                    }
                }
            }
            //a clause that is not a word ends the run and is skipped
            if index == start {
                index += 1;
            }
        }
        out
    }
}

/// The word a clause stands for: a term, or a synonym group's typed form when that is a word.
fn typed_word(part: &Query) -> Option<&str> {
    match part {
        Query::Term(term) => Some(term),
        Query::Synonym(alternatives) =>
            match alternatives.first()? {
                Query::Term(term) => Some(term),
                _ => None,
            }
        _ => None,
    }
}

