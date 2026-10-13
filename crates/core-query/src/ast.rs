use core_index::{fuzzy::FuzzySpec, numeric_values::NumericRange};
use core_protocol::command_reponse_definitions::Fuzziness;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Wildcard(String),

    Wand(Vec<Query>),

    And(Vec<Query>),
    Or(Vec<Query>),
    Phrase(Vec<String>),
    Exact(String),
    Fuzzy(String, Fuzziness, FuzzySpec),
    // Forms from the synonym dictionary that stand for one part of the query. The first is what
    // the user typed; a document is scored by the best weighted form it contains.
    Synonym(Vec<SynonymAlternative>),

    // Not(Box<Query>),
    //
    Range(NumericRange),
    SameElement(Vec<Query>),
    Bool(bool),
    MatchAll,
}

/// One form of a `Query::Synonym`, with the share of its score a match counts for, in
/// thousandths (1000 = as much as the typed form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynonymAlternative {
    pub query: Query,
    pub weight: u16,
}
