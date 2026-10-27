use crate::ast::{PhraseTerm, Query};
use core_index::analyzer::Analyzer;
use core_index::analyzer::analyzer::MAX_NAME_WORDS;
use core_index::fuzzy::{DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec};
use core_protocol::command_reponse_definitions::Fuzziness;
use core_timing::timed;
use std::collections::HashSet;

/// "All but one word" runs for this many words at most: it expands to one AND per word left out.
pub const MAX_ALL_BUT_ONE_WORDS: usize = 7;
/// Score multiplier added at full density (every word at its ideal distance): ×1.5.
/// Twice as spread out gets half of it. basically makes words that are closer score higher
pub const DENSITY_BOOST: f32 = 0.5;
/// A document whose name is a run of the query's words has its score multiplied by
/// 1 + NAME_BONUS × the share of the query's weight the run holds: ×2.5 for a run that holds the
/// whole query, ×1.5 for one that holds a third of it.
pub const NAME_BONUS: f32 = 1.5;
/// Ordering values per cascade tier: hits of the plan's `n`-th tier are numbered from
/// `n * TIER_STEP` (see `QueryExecutor::rank_cascade`).
pub const TIER_STEP: usize = 16;
/// Runs of named words are looked for in queries of at most this many words.
const MAX_NAMED_QUERY_WORDS: usize = 32;

/// At most this many name keys: the dictionary forms of every word multiply.
const MAX_NAME_KEYS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierKind {
    Name,
    Exact,
    Phrase,
    Fuzzy,
    All,
    AllButOne,
    Any,
}

impl TierKind {
    /// The order used when the config names none: the cascade as it has always been.
    pub const DEFAULT_ORDER: [TierKind; 7] = [
        TierKind::Name,
        TierKind::Exact,
        TierKind::Phrase,
        TierKind::Fuzzy,
        TierKind::All,
        TierKind::AllButOne,
        TierKind::Any,
    ];
}
#[derive(Debug, Clone, PartialEq)]
pub enum Tier {
    /// A name field (a field with `$exact`) whose whole value is the query or one of its
    /// dictionary forms: the article itself, before the ones that only mention it. exact sentance basically
    Name {
        keys: Vec<String>,
        score_with: Query,
    },
    /// The exact (case-preserving) field, text as typed.
    Exact(Query),
    /// Every typed word in order. Matches are scored with `score_with`.
    Phrase {
        terms: Vec<PhraseTerm>,
        score_with: Query,
    },
    /// The query as resolved: every word, or one of its synonyms.
    All(Query),
    /// Every word but one.
    AllButOne(Query),
    /// At least one word.
    Any(Query),
    /// Words within a few edits of the typed ones.
    Fuzzy(Query),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankPlan {
    pub tiers: Vec<Tier>,
    pub stop_after: usize,
    /// The name key of every run of the query's words (see `named_runs`).
    pub named_runs: Vec<String>,
    /// The query's words, as name keys write them.
    pub words: Vec<String>,
}

impl RankPlan {
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
            named_runs: self.named_runs.clone(),
            words: self.words.clone(),
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
    pub fn cascade(
        raw: &str,
        resolved: Query,
        analyzer: &Analyzer,
        has_exact_field: bool,
        order: &[TierKind],
    ) -> Self {
        let parts: Vec<Query> = match &resolved {
            Query::And(parts) => parts.clone(),
            _ => Vec::new(),
        };
        let mut tiers = Vec::with_capacity(order.len());
        for kind in order {
            match kind {
                TierKind::Name if has_exact_field => {
                    let keys = name_keys(raw, &resolved, analyzer);
                    if !keys.is_empty() {
                        tiers.push(Tier::Name { keys, score_with: resolved.clone() });
                    }
                }
                TierKind::Exact if has_exact_field && raw.chars().any(char::is_uppercase) => {
                    tiers.push(Tier::Exact(Query::Exact(raw.trim().to_string())));
                }
                TierKind::Phrase if parts.len() >= 2 => {
                    let terms = terms_from_text(raw, analyzer);
                    if terms.len() >= 2 {
                        tiers.push(Tier::Phrase { terms, score_with: resolved.clone() });
                    }
                }
                TierKind::Fuzzy => tiers.push(Tier::Fuzzy(Query::Fuzzy(
                    raw.trim().to_string(),
                    Fuzziness::Auto,
                    FuzzySpec { prefix_length: DEFAULT_PREFIX_LENGTH, max_expansions: DEFAULT_MAX_EXPANSIONS },
                ))),
                TierKind::All => tiers.push(Tier::All(resolved.clone())),
                TierKind::AllButOne if (3..=MAX_ALL_BUT_ONE_WORDS).contains(&parts.len()) => {
                    tiers.push(Tier::AllButOne(Query::Wand(parts.clone())));
                }
                TierKind::Any if parts.len() >= 2 => tiers.push(Tier::Any(Query::Wand(parts.clone()))),
                // the tier's condition does not hold for this query
                _ => {}
            }
        }
        let (named_runs, words) = if has_exact_field {
            named_runs(raw, analyzer)
        } else {
            (Vec::new(), Vec::new())
        };
        Self { tiers, stop_after: 0, named_runs, words }
    }

    /// Hits numbered below this come from the tiers that match the query as typed, as a whole
    /// value or exact text (the name and exact tiers): what the query names. Semantic fusion
    /// keeps them first.
    pub fn named_tier_limit(&self) -> u8 {
        let named = self
            .tiers
            .iter()
            .take_while(|tier| matches!(tier, Tier::Name { .. } | Tier::Exact(_)))
            .count();
        u8::try_from(named * TIER_STEP).unwrap_or(u8::MAX)
    }

    //this is same as the one below almost but this reads just back
    pub fn phrase_terms(&self) -> Option<&[PhraseTerm]> {
        self.tiers.iter().find_map(|tier| match tier {
            Tier::Phrase { terms, .. } => Some(terms.as_slice()),
            _ => None,
        })
    }
}

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
                        next.push(if head.is_empty() {
                            form.clone()
                        } else {
                            format!("{head} {form}")
                        });
                    }
                }
                out = next;
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The name key of every run of the typed words, up to MAX_NAME_WORDS long: a document whose
/// name is one of them is something the query is about ("Vera Wang" in "Vera Wang was born in
/// March"). And the query's words as the keys write them, to weigh how much of the query a run is.
fn named_runs(raw: &str, analyzer: &Analyzer) -> (Vec<String>, Vec<String>) {
    let typed: Vec<&str> = raw.split_whitespace().collect();
    if typed.len() > MAX_NAMED_QUERY_WORDS {
        return (Vec::new(), Vec::new());
    }
    let mut keys: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for start in 0..typed.len() {
        for end in start + 1..=typed.len().min(start + MAX_NAME_WORDS) {
            if let Some(key) = analyzer.name_key(&typed[start..end].join(" ")) {
                if seen.insert(key.clone()) {
                    keys.push(key);
                }
            }
        }
    }
    // the words name_key keeps: one per position, a hyphenated word's parts but not its joined form
    let mut words: Vec<(u32, String)> = Vec::new();
    analyzer.for_each_token(raw, |term, position, length| {
        if length == 1 && words.last().is_none_or(|(last, _)| *last < position) {
            words.push((position, term.to_owned()));
        }
    });
    (keys, words.into_iter().map(|(_, word)| word).collect())
}

/// Builds the terms from the typed text, once, when the plan is made
fn terms_from_text(raw: &str, analyzer: &Analyzer) -> Vec<PhraseTerm> {
    let tokens = analyzer.analyze_query(raw);
    let Some(base) = tokens.first().map(|token| token.position) else {
        return Vec::new();
    };
    tokens
        .into_iter()
        .map(|token| PhraseTerm {
            offset: token.position - base,
            text: token.text,
        })
        .collect()
}

