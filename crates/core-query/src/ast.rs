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
    Phrase(Vec<PhraseTerm>),
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
//Lai King of the hill ir ar pareizam vietam
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PhraseTerm {
    pub text: String,
    pub offset: u32,  // position relative to first word
}