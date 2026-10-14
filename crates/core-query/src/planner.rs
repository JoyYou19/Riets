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
    /// A name field (a field with `$exact`) whose whole value is the query or one of its
    /// dictionary forms: the article itself, before the ones that only mention it. `keys` are
    /// name keys (`Analyzer::name_key`). Matches are scored with `score_with`.
    Name { keys: Vec<String>, score_with: Query },
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
    /// The same plan without its fuzzy tier.
    pub fn without_fuzzy(&self) -> Self {
        Self {
            tiers: self
                .tiers
                .iter()
                .filter(|tier| !matches!(tier, Tier::Fuzzy(_)))
                .cloned()
                .collect(),
            stop_after: self.stop_after,
        }
    }

    pub fn stopping_after(mut self, results: usize) -> Self {
        self.stop_after = results;
        self
    }
    /// Every query the plan runs, for preparing posting lists and statistics up front.
    pub fn queries(&self) -> impl Iterator<Item = &Query> {
        self.tiers.iter().map(|tier| match tier {
            Tier::Exact(q) | Tier::All(q) | Tier::AllButOne(q) | Tier::Any(q) | Tier::Fuzzy(q) => q,
            Tier::Phrase { score_with, .. } | Tier::Name { score_with, .. } => score_with,
        })
    }
    

    /// `raw` is the text as typed; `resolved` is it after synonym expansion and analysis.
    #[timed(search)]
    pub fn cascade(raw: &str, resolved: Query, analyzer: &Analyzer, has_exact_field: bool) -> Self {
        let mut tiers = Vec::with_capacity(7);

        if has_exact_field {
            let keys = name_keys(raw, &resolved, analyzer);
            if !keys.is_empty() {
                tiers.push(Tier::Name { keys, score_with: resolved.clone() });
            }
        }

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


/// At most this many name keys: the dictionary forms of every word multiply.
const MAX_NAME_KEYS: usize = 32;

/// The name keys a whole-value match is looked up by: the typed text, then every way of writing
/// it with the dictionary's forms ("world war 2" -> "world war ii", "second world war", "wwii").
fn name_keys(raw: &str, resolved: &Query, analyzer: &Analyzer) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    if let Some(typed) = analyzer.name_key(raw) {
        keys.push(typed);
    }
    // the resolved words lost the stopwords between them: "king of the hill" is not "king hill"
    if keys.first().is_some_and(|typed| typed.contains('_')) {
        return keys;
    }
    for key in written_forms(resolved) {
        if keys.len() >= MAX_NAME_KEYS {
            break;
        }
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Every word sequence `query` matches as a whole, in analyzed words; empty when it can match
/// something no fixed words describe (a prefix, a fuzzy word).
fn written_forms(query: &Query) -> Vec<String> {
    match query {
        Query::Term(term) => vec![term.clone()],
        // a stopword the phrase skips stays a gap, as in Analyzer::name_key
        Query::Phrase(terms) => {
            let mut key = String::new();
            let mut next = 0;
            for term in terms {
                if term.offset > next {
                    key.push_str("_ ");
                }
                key.push_str(&term.text);
                key.push(' ');
                next = term.offset + 1;
            }
            key.pop();
            vec![key]
        }
        // the case-preserving copy of a typed form, already covered by that form
        Query::Exact(_) => Vec::new(),
        Query::Synonym(forms) => {
            let mut out: Vec<String> = Vec::new();
            for form in forms {
                for written in written_forms(&form.query) {
                    if out.len() >= MAX_NAME_KEYS {
                        return out;
                    }
                    if !out.contains(&written) {
                        out.push(written);
                    }
                }
            }
            out
        }
        Query::And(parts) => {
            let mut out: Vec<String> = vec![String::new()];
            for part in parts {
                let forms = written_forms(part);
                if forms.is_empty() {
                    return Vec::new();
                }
                let mut next = Vec::with_capacity(out.len() * forms.len());
                for head in &out {
                    for form in &forms {
                        if next.len() >= MAX_NAME_KEYS {
                            break;
                        }
                        next.push(if head.is_empty() { form.clone() } else { format!("{head} {form}") });
                    }
                }
                out = next;
            }
            out
        }
        _ => Vec::new(),
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