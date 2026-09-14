use core_index::fuzzy::FuzzyOptions;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Wildcard(String),
    And(Vec<Query>),
    Or(Vec<Query>),
    Phrase(Vec<String>),
    Exact(String),
    Fuzzy(String, FuzzyOptions),
    // Not(Box<Query>),
}
