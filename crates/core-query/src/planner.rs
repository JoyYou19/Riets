//! Ranking signals applied after retrieval. A plan never adds or removes documents: it only
//! re-orders the candidates retrieval found, so recall is exactly what the query gives.

use core_index::analyzer::Analyzer;
use core_timing::timed;

use crate::ast::PhraseTerm;

/// Multiplier added for a document containing every typed word in order (the score doubles).
pub const FULL_PHRASE_BOOST: f32 = 1.0;
/// Multiplier added for each pair of neighbouring typed words found next to each other.
pub const PAIR_BOOST: f32 = 0.25;
/// Candidates re-ranked for every result asked for, and the bounds on that window.
pub const RERANK_FACTOR: usize = 4;
pub const MIN_RERANK_WINDOW: usize = 100;
pub const MAX_RERANK_WINDOW: usize = 1000;

/// Words that should appear at these relative positions.
#[derive(Debug, Clone, PartialEq)]
pub struct ProximitySignal {
    pub terms: Vec<PhraseTerm>,
    pub boost: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankPlan {
    pub proximity: Vec<ProximitySignal>,
}

impl RankPlan {
    pub fn is_empty(&self) -> bool {
        self.proximity.is_empty()
    }

    /// How many candidates retrieval returns so `k` results can be re-ranked.
    pub fn window(&self, k: usize) -> usize {
        if self.is_empty() {
            return k;
        }
        k.saturating_mul(RERANK_FACTOR)
            .clamp(MIN_RERANK_WINDOW, MAX_RERANK_WINDOW)
            .max(k)
    }

    /// Plans from the text as typed, before it becomes an `And` of words, so the stopword gaps
    /// the index has are kept ("king of the hill" -> king@0, hill@3).
    #[timed(search)]
    pub fn for_text(text: &str, analyzer: &Analyzer) -> Self {
        let tokens = analyzer.analyze_query(text);
        let Some(base) = tokens.first().map(|token| token.position) else {
            return Self::default();
        };
        let terms: Vec<PhraseTerm> = tokens
            .into_iter()
            .map(|token| PhraseTerm {
                offset: token.position - base,
                text: token.text,
            })
            .collect();
        if terms.len() < 2 {
            return Self::default();
        }

        let mut proximity = Vec::with_capacity(terms.len());
        proximity.push(ProximitySignal {
            terms: terms.clone(),
            boost: FULL_PHRASE_BOOST,
        });
        // with two words the pair is the whole phrase: one signal is enough
        if terms.len() > 2 {
            for pair in terms.windows(2) {
                proximity.push(ProximitySignal {
                    terms: vec![
                        PhraseTerm {
                            text: pair[0].text.clone(),
                            offset: 0,
                        },
                        PhraseTerm {
                            text: pair[1].text.clone(),
                            offset: pair[1].offset - pair[0].offset,
                        },
                    ],
                    boost: PAIR_BOOST,
                });
            }
        }
        Self { proximity }
    }
}