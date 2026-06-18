#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Term(String),
    Prefix(String),
    Wildcard(String),
    And(Vec<Query>),
    Or(Vec<Query>),
    Phrase(Vec<String>),
    // Not(Box<Query>),
}
