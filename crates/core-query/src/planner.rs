//! The match cascade: a plain query runs as a series of tiers from most to least accurate.
//! Every hit keeps the best tier that found it, and hits order by tier before score.

use core_index::analyzer::Analyzer;
use core_index::fuzzy::{DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec};
use core_protocol::command_reponse_definitions::Fuzziness;
use core_timing::timed;

use crate::ast::{PhraseTerm, Query};

/// "All but one word" runs for this many words at most: it expands to one AND per word left out.
pub const MAX_ALL_BUT_ONE_WORDS: usize = 6;
/// Score multiplier added at full density (every word at its ideal distance): ×1.5.
/// Twice as spread out gets half of it. basically makes words that are closer score higher 
pub const DENSITY_BOOST: f32 = 0.5;
#[derive(Debug, Clone, PartialEq)]
pub enum Tier {
    /// The exact (case-preserving) field, text as typed.
    Exact(Query),
    /// Every typed word in order. Matches are scored with `score_with`.
    Phrase { terms: Vec<PhraseTerm>, score_with: Query },
    /// The query as resolved: every word, or one of its synonyms.
    All(Query),
    /// Every word but one.
    AllButOne(Query),
    /// At least one word.
    Any(Query),
    /// Words within a few edits of the typed ones.
    Fuzzy(Query),
}

#[derive(Debug, Clone,Default, PartialEq)]
pub struct RankPlan {
    pub tiers: Vec<Tier>,
    pub stop_after:usize,
}

impl RankPlan {
    /// A query written with explicit operators runs exactly as written.
    pub fn single(query: Query) -> Self {
        Self { tiers: vec![Tier::All(query)],stop_after:0 }
    }
    pub fn stopping_after(mut self, results: usize) -> Self {
        self.stop_after = results;
        self
    }
    /// Every query the plan runs, for preparing posting lists and statistics up front.
    pub fn queries(&self) -> impl Iterator<Item = &Query> {
        self.tiers.iter().map(|tier| match tier {
            Tier::Exact(q) | Tier::All(q) | Tier::AllButOne(q) | Tier::Any(q) | Tier::Fuzzy(q) => q,
            Tier::Phrase { score_with, .. } => score_with,
        })
    }
    

    /// `raw` is the text as typed; `resolved` is it after synonym expansion and analysis.
    #[timed(search)]
    pub fn cascade(raw: &str, resolved: Query, analyzer: &Analyzer, has_exact_field: bool) -> Self {
        let mut tiers = Vec::with_capacity(6);

        if has_exact_field && raw.chars().any(char::is_uppercase) {
            tiers.push(Tier::Exact(Query::Exact(raw.trim().to_string())));
        }

        let parts: Vec<Query> = match &resolved {
            Query::And(parts) => parts.clone(),
            _ => Vec::new(),
        };

        if parts.len() >= 2 {
            let terms = terms_from_text(raw, analyzer);
            if terms.len() >= 2 {
                tiers.push(Tier::Phrase { terms, score_with: resolved.clone() });
            }
        }

        tiers.push(Tier::All(resolved.clone()));

        tiers.push(Tier::Fuzzy(Query::Fuzzy(
            raw.trim().to_string(),
            Fuzziness::Auto,
            FuzzySpec {
                prefix_length: DEFAULT_PREFIX_LENGTH,
                max_expansions: DEFAULT_MAX_EXPANSIONS,
            },
        )));

        if (3..=MAX_ALL_BUT_ONE_WORDS).contains(&parts.len()) {
            let alternatives: Vec<Query> = (0..parts.len())
                .map(|left_out| {
                    Query::And(
                        parts
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| *index != left_out)
                            .map(|(_, part)| part.clone())
                            .collect(),
                    )
                })
                .collect();
            tiers.push(Tier::AllButOne(Query::Or(alternatives)));
        }

        if parts.len() >= 2 {
            tiers.push(Tier::Any(Query::Wand(parts)));
        }

        

        Self { tiers, stop_after:0}
    }
    //this is same as the one below almost but this reads just back 
    pub fn phrase_terms(&self) -> Option<&[PhraseTerm]> {
        self.tiers.iter().find_map(|tier| match tier {
            Tier::Phrase { terms, .. } => Some(terms.as_slice()),
            _ => None,
        })
    }
}


/// Builds the terms from the typed text, once, when the plan is made
fn terms_from_text(raw: &str, analyzer: &Analyzer) -> Vec<PhraseTerm> {
    let tokens = analyzer.analyze_query(raw);
    let Some(base) = tokens.first().map(|token| token.position) else {
        return Vec::new();
    };
    tokens
        .into_iter()
        .map(|token| PhraseTerm { offset: token.position - base, text: token.text })
        .collect()
}