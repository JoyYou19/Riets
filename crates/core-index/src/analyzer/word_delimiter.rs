use std::collections::HashSet;

//INFO: if a symbol is not in the literal_symbols it gets split into multiple words:
//examples: spider-man -> spider man spiderman spider-man
//(default it "_") spider_man -> spider_man

//made to be compatible with tantivys analyzer

pub fn expand_word_delimiters(word: &str, literal_symbols: &HashSet<char>) -> Vec<String> {
    let has_delim = word
        .chars()
        .any(|c| !c.is_alphanumeric() && !literal_symbols.contains(&c));

    if !has_delim {
        return vec![word.to_string()];
    }

    let parts: Vec<String> = word
        .split(|c: char| !c.is_alphanumeric() && !literal_symbols.contains(&c))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();

    let mut out = Vec::with_capacity(parts.len() + 2);
    out.push(word.to_string()); // original
    out.extend(parts.iter().cloned()); // split parts
    out.push(parts.concat()); // catenated
    out
}

use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

#[derive(Clone)]
pub struct WordDelimiterTokenizer {
    literal_symbols: HashSet<char>,
}

impl WordDelimiterTokenizer {
    pub fn new(literal_symbols: impl IntoIterator<Item = char>) -> Self {
        Self {
            literal_symbols: literal_symbols.into_iter().collect(),
        }
    }

    fn emit(
        &self,
        chunk: &str,
        from: usize,
        to: usize,
        position: &mut usize,
        tokens: &mut Vec<Token>,
    ) {
        for expanded in expand_word_delimiters(chunk, &self.literal_symbols) {
            tokens.push(Token {
                offset_from: from,
                offset_to: to,
                position: *position,
                text: expanded,
                position_length: 1,
            });
            *position += 1;
        }
    }
}

impl Tokenizer for WordDelimiterTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let mut tokens = Vec::new();
        let mut position = 0usize;
        let mut chunk_start: Option<usize> = None;

        for (idx, ch) in text.char_indices() {
            if ch.is_whitespace() {
                if let Some(s) = chunk_start.take() {
                    self.emit(&text[s..idx], s, idx, &mut position, &mut tokens);
                }
            } else if chunk_start.is_none() {
                chunk_start = Some(idx);
            }
        }
        if let Some(s) = chunk_start {
            self.emit(&text[s..], s, text.len(), &mut position, &mut tokens);
        }

        BoxTokenStream::new(VecTokenStream { tokens, idx: 0 })
    }
}

struct VecTokenStream {
    tokens: Vec<Token>,
    idx: usize,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.idx < self.tokens.len() {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.idx - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syms(s: &[char]) -> HashSet<char> {
        s.iter().copied().collect()
    }

    #[test]
    fn literal_symbol_stays_whole() {
        assert_eq!(
            expand_word_delimiters("spider_man", &syms(&['_'])),
            vec!["spider_man".to_string()]
        );
    }

    #[test]
    fn hyphen_splits_catenates_preserves() {
        assert_eq!(
            expand_word_delimiters("spider-man", &syms(&['_'])),
            vec!["spider-man", "spider", "man", "spiderman"]
        );
    }

    #[test]
    fn colon_splits_catenates_preserves() {
        assert_eq!(
            expand_word_delimiters("Ca:P", &syms(&['_'])),
            vec!["Ca:P", "Ca", "P", "CaP"]
        );
    }

    #[test]
    fn no_delimiter_unchanged() {
        assert_eq!(
            expand_word_delimiters("spiderman", &syms(&['_'])),
            vec!["spiderman".to_string()]
        );
    }
}
