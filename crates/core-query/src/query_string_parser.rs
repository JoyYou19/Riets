use core_protocol::{ command_reponse_definitions::Fuzziness, errors::CorelamoError };
use core_timing::timed;
use simd_json::{ OwnedValue, base::{ TypedValue, ValueAsScalar } };

use crate::ast::{ PhraseTerm, Query, SynonymAlternative };
use core_index::{
    analyzer::Analyzer,
    fuzzy::{ DEFAULT_MAX_EXPANSIONS, DEFAULT_PREFIX_LENGTH, FuzzySpec },
    wildcard::WildcardPattern,
};

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

//INFO: after string->ast we still need to analyze each word
#[timed(search)]
pub fn analyze_query(query: Query, analyzer: &Analyzer) -> Option<Query> {
    match query {
        Query::Term(word) => analyze_term(&word, analyzer),
        Query::Phrase(terms) => {
            let words: Vec<String> = terms
                .into_iter()
                .map(|term| term.text)
                .collect();
            analyze_phrase(&words, analyzer)
        }

        //for these just lowercase cuz like "ca[tnb]" is not a real word for analasys
        Query::Prefix(p) => non_empty(p.to_lowercase()).map(Query::Prefix),
        Query::Wildcard(p) => non_empty(p.to_lowercase()).map(Query::Wildcard),

        Query::Wand(subs) => combine(subs, analyzer, Query::Wand),
        Query::And(subs) => combine(subs, analyzer, Query::And),
        Query::Or(subs) => combine(subs, analyzer, Query::Or),

        Query::Exact(term) => Some(Query::Exact(term)),
        Query::Fuzzy(term, fuzziness, spec) => Some(Query::Fuzzy(term, fuzziness, spec)),
        Query::Synonym(forms) => analyze_synonym(forms, analyzer),

        Query::Range(range) => Some(Query::Range(range)),
        Query::Bool(b) => Some(Query::Bool(b)),
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

/// The typed form (the first) decides: when the analyzer drops it (a stopword), the whole group
/// goes, as the word alone would. Forms that analyze to the same query are kept once, at the
/// higher weight.
fn analyze_synonym(forms: Vec<SynonymAlternative>, analyzer: &Analyzer) -> Option<Query> {
    let mut forms = forms.into_iter();
    let typed = forms.next()?;
    let mut kept = vec![SynonymAlternative {
        query: analyze_query(typed.query, analyzer)?,
        weight: typed.weight,
    }];
    for form in forms {
        let Some(query) = analyze_query(form.query, analyzer) else {
            continue;
        };
        match kept.iter_mut().find(|known| known.query == query) {
            Some(known) => {
                known.weight = known.weight.max(form.weight);
            }
            None =>
                kept.push(SynonymAlternative {
                    query,
                    weight: form.weight,
                }),
        }
    }
    if kept.len() == 1 {
        return kept.pop().map(|form| form.query);
    }
    Some(Query::Synonym(kept))
}

#[timed(search)]
fn analyze_term(word: &str, analyzer: &Analyzer) -> Option<Query> {
    let mut tokens = analyzer
        .analyze_query(word)
        .into_iter()
        .map(|t| t.text);
    //this is also the check for if returned nothing
    let first = tokens.next()?;
    match tokens.next() {
        None => Some(Query::Term(first)),
        Some(second) => {
            //if the analyzer gave us 2+ words from one they all should be in a AND
            let mut terms = vec![Query::Term(first), Query::Term(second)];
            terms.extend(tokens.map(Query::Term));
            Some(Query::And(terms))
        }
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

//for and/or/wand to do some recurcursion
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
    let tokens = analyzer.analyze_query(&text);
    let base = tokens.first()?.position;
    let phrase_terms: Vec<PhraseTerm> = tokens
        .into_iter()
        .map(|token| PhraseTerm {
            text: token.text,
            offset: token.position - base,
        })
        .collect();
    match phrase_terms.len() {
        0 => None,
        1 => {
            let term = phrase_terms.into_iter().next().unwrap();
            Some(Query::Term(term.text))
        }
        _ => Some(Query::Phrase(phrase_terms)),
    }
}

//main entry point for parsing yeye
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
            let keys: Vec<&str> = obj
                .keys()
                .map(|k| k.as_str())
                .collect();
            Err(
                CorelamoError::InvalidData(
                    format!(
                        "unknown query operator(s): {} — expected one of term/exact/fuzzy/phrase/range/same_element/AND/OR/WAND",
                        keys.join(", ")
                    )
                )
            )
        }
        other =>
            Err(
                CorelamoError::InvalidData(
                    format!("query node must be a string or object, found {}", other.value_type())
                )
            ),
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
                    //recursion
                    other => out.push(parse_json_query(other)?),
                }
            }
            Ok(make(out))
        }
        OwnedValue::String(s) => {
            let items: Vec<Query> = s.split_whitespace().map(classify_word).collect();
            Ok(make(items))
        }
        _ => Err(CorelamoError::InvalidData("AND/OR/WAND must be an array or a string".into())),
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
                return Err(
                    CorelamoError::InvalidData(
                        "term takes one word — combine with AND/OR/WAND for multiple".into()
                    )
                );
            }
            classify_word(s)
        }
    })
}

fn phrase(inner: &OwnedValue) -> Result<Query, CorelamoError> {
    let words: Vec<String> = match inner {
        OwnedValue::String(s) => {
            let words: Vec<String> = s.split_whitespace().map(str::to_string).collect();
            if words.is_empty() {
                return Err(CorelamoError::InvalidData("'phrase' must not be empty".into()));
            }
            words
        }
        OwnedValue::Array(items) => {
            let mut words = Vec::with_capacity(items.len());
            for w in items.iter() {
                let s = w
                    .as_str()
                    .ok_or_else(|| {
                        CorelamoError::InvalidData("'phrase' elements must be single words".into())
                    })?;
                if s.split_whitespace().count() > 1 {
                    return Err(
                        CorelamoError::InvalidData("'phrase' elements must be single words".into())
                    );
                }
                words.push(s.to_string());
            }
            words
        }
        _ => {
            return Err(
                CorelamoError::InvalidData("'phrase' must be a string or an array of words".into())
            );
        }
    };
    //converts to PhraseTerm with sequential offsets for results
    let phrase_terms: Vec<PhraseTerm> = words
        .into_iter()
        .enumerate()
        .map(|(index, text)| PhraseTerm {
            text,
            offset: index as u32,
        })
        .collect();
    Ok(Query::Phrase(phrase_terms))
}

//fuzzy helper 3000
fn fuzzy(inner: &OwnedValue) -> Result<Query, CorelamoError> {
    //fuzzy ALSO allows multiple words now
    if let Some(s) = inner.as_str() {
        return Ok(
            Query::Fuzzy(s.to_string(), Fuzziness::Auto, FuzzySpec {
                prefix_length: DEFAULT_PREFIX_LENGTH,
                max_expansions: DEFAULT_MAX_EXPANSIONS,
            })
        );
    }

    let OwnedValue::Object(obj) = inner else {
        return Err(
            CorelamoError::InvalidData("'fuzzy' must be a string or an object with 'value'".into())
        );
    };
    let value = obj
        .get("value")
        .and_then(OwnedValue::as_str)
        .ok_or_else(|| CorelamoError::InvalidData("'fuzzy' requires a string 'value'".into()))?
        .to_string();
    Ok(
        Query::Fuzzy(
            value,
            obj
                .get("fuzziness")
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
            }
        )
    )
}
