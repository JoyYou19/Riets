//! Rewrites a parsed query with the synonym dictionary: every span of words the dictionary knows
//! becomes a `Query::Synonym` of the words as typed and their other forms.

use core_index::analyzer::Analyzer;
use core_timing::timed;

use crate::ast::{Query, SynonymAlternative};
use crate::syn::{Alternative, QueryNode, Synonyms, lookup_key, tokenize};

/// Weight of the typed form and of exact equivalents: the spelled-out form of a typed short form
/// (`NaCl` -> sodium chloride names the one meaning the short form has here).
pub const FULL_WEIGHT: u16 = 1000;

/// Weight of a Roman numeral for a typed number and the other way round (`II` <-> 2).
pub const NUMERAL_WEIGHT: u16 = 1000;

/// Weight of a loose equivalent: a synonym, a broader word (`king` -> monarch), or a short form
/// found for typed words. "united states" -> `US` would also reach US Yachts and every other
/// US title, so such a match must not count as much as the words that were typed.
pub const SYNONYM_WEIGHT: u16 = 700;

type Combinator = fn(Vec<Query>) -> Query;

#[timed(search)]
pub fn expand_synonyms(query: Query, dictionary: &Synonyms, analyzer: &Analyzer) -> Query {
    match query {
        Query::Term(text) if is_multi_word(&text) => expand_multi_word(text, dictionary, analyzer),
        Query::Term(word) => {
            let expanded = expand_run(vec![word], dictionary, analyzer, Query::Wand);
            match <[Query; 1]>::try_from(expanded) {
                Ok([only]) => only,
                Err(many) => Query::Wand(many),
            }
        }
        Query::Wand(subs) => {
            Query::Wand(expand_children(subs, dictionary, analyzer, Query::Wand))
        }
        Query::And(subs) => Query::And(expand_children(subs, dictionary, analyzer, Query::And)),
        Query::Or(subs) => Query::Or(expand_children(subs, dictionary, analyzer, Query::Or)),
        //children map 1:1 onto same_element bindings, so they are never merged
        Query::SameElement(children) => Query::SameElement(
            children
                .into_iter()
                .map(|child| expand_synonyms(child, dictionary, analyzer))
                .collect(),
        ),
        other => other,
    }
}

fn is_multi_word(text: &str) -> bool {
    text.split_whitespace().nth(1).is_some()
}

/// The parser hands a plain string query over as one `Term` holding the whole text, which
/// `analyze_query` later turns into an `And` of its words. Expand it as exactly that `And`, so a
/// match can target single words inside it. Text the dictionary doesn't touch is returned as it
/// was.
fn expand_multi_word(text: String, dictionary: &Synonyms, analyzer: &Analyzer) -> Query {
    let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    let expanded = expand_run(words, dictionary, analyzer, Query::And);
    if expanded.iter().all(|part| matches!(part, Query::Term(_))) {
        return Query::Term(text);
    }
    Query::And(expanded)
}

fn expand_children(
    subs: Vec<Query>,
    dictionary: &Synonyms,
    analyzer: &Analyzer,
    wrap: Combinator,
) -> Vec<Query> {
    let mut out = Vec::with_capacity(subs.len());
    let mut run: Vec<String> = Vec::new();
    for sub in subs {
        match sub {
            Query::Term(word) if !is_multi_word(&word) => run.push(word),
            other => {
                if !run.is_empty() {
                    out.extend(expand_run(std::mem::take(&mut run), dictionary, analyzer, wrap));
                }
                out.push(expand_synonyms(other, dictionary, analyzer));
            }
        }
    }
    if !run.is_empty() {
        out.extend(expand_run(run, dictionary, analyzer, wrap));
    }
    out
}

/// Expands consecutive sibling words, in query order. `wrap` is the parent combinator, used to
/// rebuild the typed words of a multi-word match as one form.
fn expand_run(
    words: Vec<String>,
    dictionary: &Synonyms,
    analyzer: &Analyzer,
    wrap: Combinator,
) -> Vec<Query> {
    let mut surface: Vec<&str> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    let mut word_start: Vec<usize> = Vec::with_capacity(words.len() + 1);
    for (index, word) in words.iter().enumerate() {
        word_start.push(surface.len());
        for token in tokenize(word) {
            surface.push(token);
            owner.push(index);
        }
    }
    word_start.push(surface.len());
    let keys: Vec<String> = surface
        .iter()
        .map(|token| lookup_key(analyzer, token))
        .collect();

    let mut boundary = vec![false; surface.len() + 1];
    for &start in &word_start {
        boundary[start] = true;
    }

    let mut out = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let start = word_start[index];
        let end = word_start[index + 1];
        if start == end {
            //pure punctuation: left to the analyzer exactly as before
            out.push(Query::Term(words[index].clone()));
            index += 1;
            continue;
        }

        if let Some(found) = dictionary.match_at(&surface[start..], &keys[start..], |len| {
            boundary[start + len]
        }) {
            let last = owner[start + found.len - 1];
            let covered = &words[index..=last];
            let several_words = covered.len() > 1 || found.len > 1;
            //the typed words stay exactly what they are without the dictionary, so every document
            //they find is still found and scores at least as much
            let typed = original_form(covered, wrap);
            let typed_words: Vec<String> = surface[start..start + found.len]
                .iter()
                .map(|token| token.to_lowercase())
                .collect();
            let short_form = !several_words && found.typed_short_form;
            let alternatives: Vec<SynonymAlternative> = found
                .alternatives
                .into_iter()
                .filter(|alternative| !is_typed_form(&alternative.node, &typed_words))
                .map(|alternative| {
                    //the form typed with its exact case (`CaP`) is the typed word, more precisely
                    let typed_exact = matches!(
                        (&alternative.node, &found.typed_exact),
                        (QueryNode::Exact(text), Some(typed)) if text == typed
                    );
                    SynonymAlternative {
                        weight: if typed_exact {
                            FULL_WEIGHT
                        } else {
                            weight_of(&alternative, short_form)
                        },
                        query: to_query(alternative.node),
                    }
                })
                .collect();
            out.push(synonym(typed, alternatives));
            index = last + 1;
            continue;
        }

        if end - start == 1
            && let Some(numbers) = dictionary.expand_number(surface[start])
        {
            let typed = Query::Term(words[index].clone());
            let typed_words = vec![surface[start].to_lowercase()];
            let alternatives: Vec<SynonymAlternative> = numbers
                .into_iter()
                .filter(|alternative| !is_typed_form(&alternative.node, &typed_words))
                .map(|alternative| SynonymAlternative {
                    weight: weight_of(&alternative, false),
                    query: to_query(alternative.node),
                })
                .collect();
            out.push(synonym(typed, alternatives));
            index += 1;
            continue;
        }

        out.push(Query::Term(words[index].clone()));
        index += 1;
    }
    out
}

/// The dictionary's lowercase spelling of what was typed only finds what the typed words
/// already find.
fn is_typed_form(node: &QueryNode, typed_words: &[String]) -> bool {
    match node {
        QueryNode::Term(term) => typed_words.len() == 1 && *term == typed_words[0],
        QueryNode::Phrase(words) => words.as_slice() == typed_words,
        QueryNode::Exact(_) => false,
    }
}

fn weight_of(alternative: &Alternative, typed_short_form: bool) -> u16 {
    if alternative.numeral {
        return NUMERAL_WEIGHT;
    }
    let spelled_out = match &alternative.node {
        QueryNode::Phrase(words) => words.len() > 1,
        QueryNode::Exact(text) => is_multi_word(text),
        QueryNode::Term(_) => false,
    };
    if typed_short_form && spelled_out {
        FULL_WEIGHT
    } else {
        SYNONYM_WEIGHT
    }
}

fn to_query(node: QueryNode) -> Query {
    match node {
        QueryNode::Term(word) => Query::Term(word),
         QueryNode::Phrase(words) => Query::Phrase(words),
        QueryNode::Exact(text) => Query::Exact(text),
    }
}

fn original_form(words: &[String], wrap: Combinator) -> Query {
    match words {
        [word] => Query::Term(word.clone()),
        _ => wrap(words.iter().cloned().map(Query::Term).collect()),
    }
}

/// The typed form first, then the others; just the typed form when there are no others.
fn synonym(typed: Query, alternatives: Vec<SynonymAlternative>) -> Query {
    if alternatives.is_empty() {
        return typed;
    }
    let mut forms = Vec::with_capacity(alternatives.len() + 1);
    forms.push(SynonymAlternative {
        query: typed,
        weight: FULL_WEIGHT,
    });
    forms.extend(alternatives);
    Query::Synonym(forms)
}
