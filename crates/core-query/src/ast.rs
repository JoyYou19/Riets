use core_index::fuzzy::FuzzySpec;
use core_protocol::command_reponse_definitions::Fuzziness;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Wildcard(String),

    Search(Vec<Query>),

    And(Vec<Query>),
    Or(Vec<Query>),
    Phrase(Vec<String>),
    Exact(String),
    Fuzzy(String, Fuzziness, FuzzySpec),
    // Not(Box<Query>),
}
