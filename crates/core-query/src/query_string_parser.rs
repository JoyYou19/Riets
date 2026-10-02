use core_protocol::{command_reponse_definitions::Fuzziness, errors::CorelamoError};
use core_timing::timed;
use simd_json::{
    OwnedValue,
    base::{TypedValue, ValueAsScalar},
};

use crate::ast::Query;
use core_index::{
    analyzer::Analyzer,
    fuzzy::{DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec},
    wildcard::WildcardPattern,
};

//TODO: pielikt search komandai kko lidzigu sim preks highlight:
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

fn make_wand(mut items: Vec<Query>) -> Query {
    match items.len() {
        0 => Query::Wand(Vec::new()),
        1 => items.pop().unwrap(),
        _ => Query::Wand(items),
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

    // Plain whitespace separated queries are relevance searches
    // Parentheses () are now the ones that specify strict AND braces explicitly handle OR
    let query = make_wand(items);

    // "" () {} count as emtpy/invalid
    match &query {
        Query::Wand(inner) | Query::And(inner) | Query::Or(inner) if inner.is_empty() => Ok(None),
        _ => Ok(Some(query)),
    }
}

//INFO: after string->ast we still need to analyze each word
//WARN: spider-man situation?
#[timed(search)]
pub fn analyze_query(query: Query, analyzer: &Analyzer) -> Option<Query> {
    match query {
        Query::Term(word) => analyze_term(&word, analyzer),
        Query::Phrase(words) => analyze_phrase(&words, analyzer),

        //for these just lowercase cuz like "ca[tnb]" is not a real word lmao
        Query::Prefix(p) => non_empty(p.to_lowercase()).map(Query::Prefix),
        Query::Wildcard(p) => non_empty(p.to_lowercase()).map(Query::Wildcard),

        Query::Wand(subs) => combine(subs, analyzer, Query::Wand),
        Query::And(subs) => combine(subs, analyzer, Query::And),
        Query::Or(subs) => combine(subs, analyzer, Query::Or),

        Query::Exact(term) => Some(Query::Exact(term)),
        Query::Fuzzy(term, fuzziness, spec) => Some(Query::Fuzzy(term, fuzziness, spec)),

        Query::Range(range) => Some(Query::Range(range)),
        Query::SameElement(children) => {
            let kept: Vec<Query> = children
                .into_iter()
                .filter_map(|q| analyze_query(q, analyzer))
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(Query::SameElement(kept))
            }
        }
        Query::MatchAll => Some(Query::MatchAll),
    }
}

#[timed(search)]
fn analyze_term(word: &str, analyzer: &Analyzer) -> Option<Query> {
    let mut tokens = analyzer.analyze_query(word).into_iter().map(|t| t.text);
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
        .analyze_query(&text)
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

// --- JSON AST parser -----------------------------------------------------
//
// Turns the raw `query`/`filters` JSON (OwnedValue) into the pure Query AST.
// `range` and `same_element` are NOT handled here — they need policy/field
// context, so the resolver's build_node intercepts them before calling this.
// Leaves are single-word by design; combine with AND/OR/WAND for multi-term.

#[timed(search)]
pub fn parse_json_query(v: &OwnedValue) -> Result<Query, CorelamoError> {
    match v {
        OwnedValue::String(s) if s.trim() == "match_all" => Ok(Query::MatchAll),
        OwnedValue::String(s) => Ok(classify_word(s)),
        OwnedValue::Object(obj) => {
            if let Some(inner) = obj.get("AND") {
                return combinator(inner, Query::And);
            }
            if let Some(inner) = obj.get("OR") {
                return combinator(inner, Query::Or);
            }
            if let Some(inner) = obj.get("WAND") {
                return combinator(inner, Query::Wand);
            }
            if let Some(inner) = obj.get("term") {
                return leaf(inner, Leaf::Term);
            }
            if let Some(inner) = obj.get("exact") {
                return leaf(inner, Leaf::Exact);
            }
            if let Some(inner) = obj.get("phrase") {
                return phrase(inner);
            }
            if let Some(inner) = obj.get("fuzzy") {
                return fuzzy(inner);
            }
            let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
            Err(CorelamoError::InvalidData(format!(
                "unknown query operator(s): {} — expected one of term/exact/fuzzy/phrase/range/same_element/AND/OR/WAND",
                keys.join(", ")
            )))
        }
        other => Err(CorelamoError::InvalidData(format!(
            "query node must be a string or object, found {}",
            other.value_type()
        ))),
    }
}

enum Leaf {
    Term,
    Exact,
}

fn combinator(inner: &OwnedValue, make: fn(Vec<Query>) -> Query) -> Result<Query, CorelamoError> {
    match inner {
        OwnedValue::Array(items) => {
            let mut out = Vec::new();
            for item in items.iter() {
                match item {
                    OwnedValue::String(s) => {
                        out.extend(s.split_whitespace().map(classify_word));
                    }
                    other => out.push(parse_json_query(other)?),
                }
            }
            Ok(make(out))
        }
        OwnedValue::String(s) => {
            let items: Vec<Query> = s.split_whitespace().map(classify_word).collect();
            Ok(make(items))
        }
        _ => Err(CorelamoError::InvalidData(
            "AND/OR/WAND must be an array or a string".into(),
        )),
    }
}
fn leaf(inner: &OwnedValue, kind: Leaf) -> Result<Query, CorelamoError> {
    let s = inner
        .as_str()
        .ok_or_else(|| CorelamoError::InvalidData("leaf value must be a string".into()))?;
    Ok(match kind {
        //exact allows multiple words too
        Leaf::Exact => Query::Exact(s.to_string()),
        Leaf::Term => {
            if s.split_whitespace().count() > 1 {
                return Err(CorelamoError::InvalidData(
                    "term takes one word — combine with AND/OR/WAND for multiple".into(),
                ));
            }
            classify_word(s)
        }
    })
}

fn phrase(inner: &OwnedValue) -> Result<Query, CorelamoError> {
    let OwnedValue::Array(items) = inner else {
        return Err(CorelamoError::InvalidData(
            "'phrase' must be an array of words".into(),
        ));
    };
    let mut words = Vec::with_capacity(items.len());
    for w in items.iter() {
        let s = w.as_str().ok_or_else(|| {
            CorelamoError::InvalidData("'phrase' elements must be single words".into())
        })?;
        if s.split_whitespace().count() > 1 {
            return Err(CorelamoError::InvalidData(
                "'phrase' elements must be single words".into(),
            ));
        }
        words.push(s.to_string());
    }
    Ok(Query::Phrase(words))
}

fn fuzzy(inner: &OwnedValue) -> Result<Query, CorelamoError> {
    //fuzzy ALSO allows multiple words now
    if let Some(s) = inner.as_str() {
        return Ok(Query::Fuzzy(
            s.to_string(),
            Fuzziness::Auto,
            FuzzySpec {
                prefix_length: DEFAULT_PREFIX_LENGTH,
                max_expansions: DEFAULT_MAX_EXPANSIONS,
            },
        ));
    }

    let OwnedValue::Object(obj) = inner else {
        return Err(CorelamoError::InvalidData(
            "'fuzzy' must be a string or an object with 'value'".into(),
        ));
    };
    let value = obj
        .get("value")
        .and_then(OwnedValue::as_str)
        .ok_or_else(|| CorelamoError::InvalidData("'fuzzy' requires a string 'value'".into()))?
        .to_string();
    Ok(Query::Fuzzy(
        value,
        obj.get("fuzziness")
            .map(Fuzziness::from_owned)
            .transpose()
            .map_err(CorelamoError::InvalidData)?
            .unwrap_or(Fuzziness::Auto),
        FuzzySpec {
            prefix_length: obj
                .get("prefix_length")
                .and_then(OwnedValue::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_PREFIX_LENGTH),
            max_expansions: obj
                .get("max_expansions")
                .and_then(OwnedValue::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_EXPANSIONS),
        },
    ))
}
