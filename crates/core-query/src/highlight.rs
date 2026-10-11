use core_index::{analyzer::Analyzer, fuzzy::FuzzyMatcher, wildcard::WildcardPattern};
use core_protocol::command_reponse_definitions::HighlightFieldOptions;
use simd_json::OwnedValue;

use crate::Query;

#[derive(Debug, Clone)]
pub enum HighlightMatcher {
    Prefix(String),
    Wildcard(WildcardPattern),
    Exact(String),
    Phrase(Vec<String>),
    Fuzzy(FuzzyMatcher),
}

use crate::fuzzable_words;

pub fn collect_highlight_matchers(query: &Query, analyzer: &Analyzer) -> Vec<HighlightMatcher> {
    let mut out = Vec::new();
    collect(query, analyzer, &mut out);
    out
}

fn collect(query: &Query, analyzer: &Analyzer, out: &mut Vec<HighlightMatcher>) {
    match query {
        Query::Term(s) | Query::Prefix(s) => {
            out.push(HighlightMatcher::Prefix(s.to_lowercase()));
        }
        Query::Wildcard(p) => {
            out.push(HighlightMatcher::Wildcard(WildcardPattern::parse(
                &p.to_lowercase(),
            )));
        }
        Query::Phrase(words) => {
            out.push(HighlightMatcher::Phrase(
                words.iter().map(|w| w.to_lowercase()).collect(),
            ));
        }
        Query::Exact(s) => out.push(HighlightMatcher::Exact(s.clone())),
        Query::Fuzzy(s, fuzziness, spec) => {
            // split multi-word fuzzy into words, fuzz each — mirrors the executor
            for word in fuzzable_words(analyzer, s) {
                let max_edits = fuzziness.resolve(&word);
                out.push(HighlightMatcher::Fuzzy(FuzzyMatcher::new(
                    &word,
                    max_edits,
                    spec.prefix_length,
                )));
            }
        }
        Query::Wand(children)
        | Query::And(children)
        | Query::Or(children)
        | Query::SameElement(children) => {
            for c in children {
                collect(c, analyzer, out);
            }
        }
        Query::Range(_) | Query::Bool(_) | Query::MatchAll => {}
    }
}

pub fn highlight_text(
    text: &str,
    analyzer: &Analyzer,
    matchers: &[HighlightMatcher],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let ranges = merge_ranges(find_ranges(text, matchers, analyzer));
    if ranges.is_empty() {
        return Vec::new();
    }
    if opts.sentence {
        fragments_by_sentence(text, &ranges, opts)
    } else {
        fragments_by_chars(text, &ranges, opts)
    }
}

fn words(text: &str, analyzer: &Analyzer) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if analyzer.is_word_char(c) {
            if word.is_empty() {
                start = i;
            }
            word.push(c.to_ascii_lowercase());
        } else if !word.is_empty() {
            out.push((std::mem::take(&mut word), start, i));
        }
    }
    if !word.is_empty() {
        out.push((word, start, text.len()));
    }
    out
}

fn find_ranges(
    text: &str,
    matchers: &[HighlightMatcher],
    analyzer: &Analyzer,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let ws = words(text, analyzer);

    // single-word matchers
    for (word, start, end) in &ws {
        for m in matchers {
            let hit = match m {
                HighlightMatcher::Prefix(p) => word.starts_with(p),
                HighlightMatcher::Wildcard(pat) => pat.matches(word),
                HighlightMatcher::Fuzzy(f) => f.matches(word),
                HighlightMatcher::Exact(_) | HighlightMatcher::Phrase(_) => false,
            };
            if hit {
                ranges.push((*start, *end));
                break;
            }
        }
    }

    // phrase matchers — consecutive words → one span
    for m in matchers {
        if let HighlightMatcher::Phrase(phrase) = m {
            if phrase.len() < 2 {
                continue;
            }
            for i in 0..ws.len().saturating_sub(phrase.len() - 1) {
                let ok = phrase
                    .iter()
                    .enumerate()
                    .all(|(k, p)| ws[i + k].0.starts_with(p));
                if ok {
                    ranges.push((ws[i].1, ws[i + phrase.len() - 1].2));
                }
            }
        }
    }

    // exact substring matches
    for m in matchers {
        if let HighlightMatcher::Exact(s) = m {
            for (start, _) in text.match_indices(s.as_str()) {
                ranges.push((start, start + s.len()));
            }
        }
    }

    ranges
}

fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => merged.push(r),
        }
    }
    merged
}

fn tags(opts: &HighlightFieldOptions) -> (&str, &str) {
    let pre = opts.pre_tags.first().map(String::as_str).unwrap_or("");
    let post = opts.post_tags.first().map(String::as_str).unwrap_or("");
    (pre, post)
}

fn build_snippet(
    text: &str,
    ws: usize,
    we: usize,
    ranges: &[(usize, usize)],
    opts: &HighlightFieldOptions,
) -> String {
    let (pre, post) = tags(opts);
    let mut out = String::new();
    let mut pos = ws;
    for &(rs, re) in ranges {
        if re <= ws || rs >= we {
            continue;
        }
        let s = rs.max(ws);
        let e = re.min(we);
        out.push_str(&text[pos..s]);
        out.push_str(pre);
        out.push_str(&text[s..e]);
        out.push_str(post);
        pos = e;
    }
    out.push_str(&text[pos..we]);
    out
}

fn fragments_by_chars(
    text: &str,
    ranges: &[(usize, usize)],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    if text.len() <= opts.fragment_size {
        return vec![build_snippet(text, 0, text.len(), ranges, opts)];
    }
    let mut out = Vec::new();
    for &(start, end) in ranges.iter().take(opts.number_of_fragments) {
        let half = opts.fragment_size / 2;
        let ws = snap_down(text, start.saturating_sub(half));
        let we = snap_up(text, (end + half).min(text.len()));
        out.push(build_snippet(text, ws, we, ranges, opts));
    }
    out
}

fn fragments_by_sentence(
    text: &str,
    ranges: &[(usize, usize)],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    for (start, end) in split_sentences(text) {
        if ranges.iter().any(|&(rs, re)| rs < end && re > start) {
            out.push(build_snippet(text, start, end, ranges, opts));
            if out.len() >= opts.number_of_fragments {
                break;
            }
        }
    }
    out
}

fn split_sentences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '!' | '?') {
            out.push((start, i + c.len_utf8()));
            start = i + c.len_utf8();
        }
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

fn snap_down(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn snap_up(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub fn field_texts(value: &OwnedValue, path: &str) -> Vec<String> {
    match walk_path(value, path) {
        Some(OwnedValue::String(s)) => vec![s.clone()],
        Some(OwnedValue::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                OwnedValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn walk_path<'a>(value: &'a OwnedValue, path: &str) -> Option<&'a OwnedValue> {
    let mut cur = value;
    for seg in path.split('/') {
        let OwnedValue::Object(obj) = cur else {
            return None;
        };
        cur = obj.get(seg)?;
    }
    Some(cur)
}
