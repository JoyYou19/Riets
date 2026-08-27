use core_protocol::errors::CorelamoError;
use core_timing::timed;

use crate::ast::Query;
use core_index::{analyzer::Analyzer, wildcard::WildcardPattern};

//TODO: pielikt search komandai kko lidzigu sim:
//  "highlight": {
//   "fields": {
//     "content": {
//       "fragment_size": 150,
//       "number_of_fragments": 3,
//       "pre_tags": ["<em>"],
//       "post_tags": ["</em>"]
//     }
//   }
// }

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    LBrace, // {
    RBrace, // }
    LParen, // (
    RParen, // )
    //  "new york" -> ["new", "york"]
    Phrase(Vec<String>),

    // A plain chunk of text: rust, dat*, ma[py]
    Word(String),
}

// Turn the raw string into tokens.
// `{ } ( ) "` are special. Everything else, including ? * [ ], is just word text.
// Errors if a quote is opened but never closed.
#[timed(search)]
fn tokenize(input: &str) -> Result<Vec<Token>, CorelamoError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            c if c.is_whitespace() => {
                chars.next();
            }
            '{' => {
                chars.next();
                tokens.push(Token::LBrace);
            }
            '}' => {
                chars.next();
                tokens.push(Token::RBrace);
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '"' => {
                //INFO: everything here is considered a word
                chars.next();
                let mut buf = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '"' {
                        closed = true;
                        break;
                    }
                    buf.push(c);
                }
                if !closed {
                    return Err(CorelamoError::InvalidData(
                        "unterminated \"quotes\" in query".to_string(),
                    ));
                }
                let words = buf.split_whitespace().map(str::to_string).collect();
                tokens.push(Token::Phrase(words));
            }
            _ => {
                let mut buf = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '{' | '}' | '(' | ')' | '"') {
                        break;
                    }
                    buf.push(c);
                    chars.next();
                }
                tokens.push(Token::Word(buf));
            }
        }
    }

    Ok(tokens)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Closer {
    Eof,
    Brace,
    Paren,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    #[timed(search)]
    fn parse_sequence(&mut self, closer: Closer) -> Result<Vec<Query>, CorelamoError> {
        let mut items = Vec::new();

        while self.pos < self.tokens.len() {
            let token = self.tokens[self.pos].clone();
            match token {
                Token::RBrace => {
                    self.pos += 1;
                    if closer == Closer::Brace {
                        return Ok(items);
                    }
                    return Err(CorelamoError::InvalidData(
                        "unexpected '}' in query".to_string(),
                    ));
                }
                Token::RParen => {
                    self.pos += 1;
                    if closer == Closer::Paren {
                        return Ok(items);
                    }
                    return Err(CorelamoError::InvalidData(
                        "unexpected ')' in query".to_string(),
                    ));
                }
                Token::LBrace => {
                    self.pos += 1;
                    let inner = self.parse_sequence(Closer::Brace)?;
                    items.push(make_or(inner));
                }
                Token::LParen => {
                    self.pos += 1;
                    let inner = self.parse_sequence(Closer::Paren)?;
                    items.push(make_and(inner));
                }
                Token::Phrase(words) => {
                    items.push(Query::Phrase(words));
                    self.pos += 1;
                }
                Token::Word(word) => {
                    items.push(classify_word(&word));
                    self.pos += 1;
                }
            }
        }

        if closer != Closer::Eof {
            return Err(CorelamoError::InvalidData(
                "unclosed parenthesese in query".to_string(),
            ));
        }

        Ok(items)
    }
}

fn make_and(mut items: Vec<Query>) -> Query {
    match items.len() {
        0 => Query::And(Vec::new()),
        1 => items.pop().unwrap(),
        _ => Query::And(items),
    }
}

fn make_or(mut items: Vec<Query>) -> Query {
    match items.len() {
        0 => Query::And(Vec::new()),
        1 => items.pop().unwrap(),
        _ => Query::Or(items),
    }
}

//decide between prefix (dat*), a wildcard (da?ab*e), or just a word
#[timed(search)]
fn classify_word(word: &str) -> Query {
    let pattern = WildcardPattern::parse(word);
    if pattern.is_prefix_only() {
        Query::Prefix(pattern.prefix().to_string())
    } else if has_wildcard(word) {
        Query::Wildcard(word.to_string())
    } else {
        Query::Term(word.to_string())
    }
}

// has wildcard?
fn has_wildcard(word: &str) -> bool {
    word.chars().any(|c| matches!(c, '*' | '?' | '['))
}

#[timed(search)]
pub fn parse_query(input: &str) -> Result<Option<Query>, CorelamoError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Ok(None);
    }

    let mut parser = Parser::new(tokens);
    let items = parser.parse_sequence(Closer::Eof)?;

    //finally we only have xxx AND xxx ADN xxx
    //TODO: elastic offers a default operator to be AND/OR
    let query = make_and(items);

    // "" () {} count as emtpy/invalid
    match &query {
        Query::And(inner) | Query::Or(inner) if inner.is_empty() => Ok(None),
        _ => Ok(Some(query)),
    }
}

//INFO: after string->ast we still need to analyze each word there the same way when indexing
#[timed(search)]
pub fn analyze_query(query: Query, analyzer: &Analyzer) -> Option<Query> {
    match query {
        Query::Term(word) => analyze_term(&word, analyzer),
        Query::Phrase(words) => analyze_phrase(&words, analyzer),

        //for these just lowercase cuz like "ca[tnb]" is not a real word lmao
        Query::Prefix(p) => non_empty(p.to_lowercase()).map(Query::Prefix),
        Query::Wildcard(p) => non_empty(p.to_lowercase()).map(Query::Wildcard),

        Query::And(subs) => combine(subs, analyzer, Query::And),
        Query::Or(subs) => combine(subs, analyzer, Query::Or),
    }
}

#[timed(search)]
fn analyze_term(word: &str, analyzer: &Analyzer) -> Option<Query> {
    let mut tokens = analyzer.analyze(word).into_iter().map(|t| t.text);
    //this is also the check for if returned nothing
    let first = tokens.next()?;
    match tokens.next() {
        None => Some(Query::Term(first)),
        Some(second) => {
            //if the analyzer gave us 2+ words from one they all should be in a and
            let mut terms = vec![Query::Term(first), Query::Term(second)];
            terms.extend(tokens.map(Query::Term));
            Some(Query::And(terms))
        }
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

//for and/or to do some recurcursion
#[timed(search)]
fn combine(subs: Vec<Query>, analyzer: &Analyzer, make: fn(Vec<Query>) -> Query) -> Option<Query> {
    let mut kept: Vec<Query> = subs
        .into_iter()
        .filter_map(|q| analyze_query(q, analyzer))
        .collect();
    match kept.len() {
        0 => None,
        1 => kept.pop(),
        _ => Some(make(kept)),
    }
}

#[timed(search)]
fn analyze_phrase(words: &[String], analyzer: &Analyzer) -> Option<Query> {
    let text = words.join(" ");
    let tokens: Vec<String> = analyzer
        .analyze(&text)
        .into_iter()
        .map(|t| t.text)
        .collect();
    match tokens.len() {
        0 => None,
        1 => Some(Query::Term(tokens.into_iter().next().unwrap())),
        _ => Some(Query::Phrase(tokens)),
    }
}

//INFO: main thing to go from string -> parsed+analyzed query
#[timed(search)]
pub fn parse_and_analyze(input: &str, analyzer: &Analyzer) -> Result<Option<Query>, CorelamoError> {
    match parse_query(input)? {
        Some(raw) => Ok(analyze_query(raw, analyzer)),
        None => Ok(None),
    }
}
