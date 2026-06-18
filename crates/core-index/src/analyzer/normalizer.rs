use crate::analyzer::token::{RawToken, Token};

/*
* Normalized currently means that we lowercase it,
* might remove filtered words, punctuation if we want to
*/
#[derive(Debug, Clone, Default)]
pub struct Normalizer;

impl Normalizer {
    pub fn normalize(&self, token: RawToken) -> Token {
        Token {
            text: token.text.to_lowercase(),
            position: token.position,
            start_byte: token.start_byte,
            end_byte: token.end_byte,
        }
    }
}
