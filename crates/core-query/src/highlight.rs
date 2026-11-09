use ahash::{HashMap, HashMapExt, HashSet};
use core_index::analyzer::Analyzer;
use core_index::types::position_start;
use core_protocol::command_reponse_definitions::HighlightFieldOptions;
use core_timing::timed;
use simd_json::OwnedValue;

use crate::MatchedTerm;

use core_index::document::policy::ARRAY_GAP;

//we need to re-analyze flattened arrays cause the position and offsets dont match in them
#[timed(highlight)]
pub fn highlight_field_array(
    texts: &[String],
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let want: HashSet<u32> = matched
        .iter()
        .flat_map(|m| m.positions.iter().map(|&p| position_start(p)))
        .collect();
    if want.is_empty() {
        return Vec::new();
    }
    highlight_flattened_texts(texts, analyzer, &want, opts)
}

//                                  this bs:
//like ummm cast: {name, surname, aliases["", "", ""],}
#[timed(highlight)]
pub fn highlight_field_row_keyed_flattened(
    value: &OwnedValue,
    full_path: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut by_row: HashMap<Vec<u32>, HashSet<u32>> = HashMap::new();
    for m in matched {
        by_row
            .entry(m.row_path.clone())
            .or_default()
            .extend(m.positions.iter().map(|&p| position_start(p)));
    }

    let mut out = Vec::new();
    for (row_path, want) in by_row {
        let Some(OwnedValue::Array(items)) = value_for_row(value, full_path, &row_path) else {
            continue;
        };
        let texts: Vec<String> = items
            .iter()
            .filter_map(|v| match v {
                OwnedValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        if texts.is_empty() {
            continue;
        }
        out.extend(highlight_flattened_texts(&texts, analyzer, &want, opts));
    }
    out
}

//Builds highlighted fragments for one field value from the spans that matched.
//Fast path (offsets stored) wraps byte spans on `text` directly; slow path
//(char-filtered field, no offsets) re-analyzes and maps positions to spans on the
//filtered text.
#[timed(highlight)]
pub fn highlight_field(
    text: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    if matched.is_empty() {
        return Vec::new();
    }

    let fast = matched.iter().any(|m| !m.offsets.is_empty());

    if fast {
        let ranges: Vec<(usize, usize)> = matched
            .iter()
            .flat_map(|m| m.offsets.iter().map(|&(s, e)| (s as usize, e as usize)))
            .collect();
        if ranges.is_empty() {
            return Vec::new();
        }
        let ranges = merge_ranges(ranges);
        return if opts.sentence {
            fragments_by_sentence(text, &ranges, opts)
        } else {
            fragments_by_chars(text, &ranges, opts)
        };
    }

    //slow branch: re-analyze once then map each matched position to its byte span
    let (filtered, tokens) = analyzer.analyze_text(text);
    let want: HashSet<u32> = matched
        .iter()
        .flat_map(|m| m.positions.iter().map(|&p| position_start(p)))
        .collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for token in &tokens {
        if want.contains(&token.position) {
            ranges.push((token.start_byte, token.end_byte));
        }
    }
    if ranges.is_empty() {
        return Vec::new();
    }
    let ranges = merge_ranges(ranges);
    if opts.sentence {
        fragments_by_sentence(&filtered, &ranges, opts)
    } else {
        fragments_by_chars(&filtered, &ranges, opts)
    }
}

#[timed(highlight)]
fn highlight_flattened_texts(
    texts: &[String],
    analyzer: &Analyzer,
    want: &HashSet<u32>,
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut base = 0u32;
    for text in texts {
        let (filtered, tokens) = analyzer.analyze_text(text);
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut span = 0u32;
        for token in &tokens {
            if want.contains(&(base + token.position)) {
                ranges.push((token.start_byte, token.end_byte));
            }
            span = span.max(token.position + token.position_length.max(1));
        }
        if !ranges.is_empty() {
            let ranges = merge_ranges(ranges);
            if opts.sentence {
                out.extend(fragments_by_sentence(&filtered, &ranges, opts));
            } else {
                out.extend(fragments_by_chars(&filtered, &ranges, opts));
            }
        }
        base += span + ARRAY_GAP;
    }
    out
}

//rewalks the json to get the precise element from the [index] of the array
#[timed(highlight)]
pub fn highlight_field_row_keyed(
    value: &OwnedValue,
    full_path: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    //group positions by row, so each array element is highlighted once
    let mut by_row: HashMap<Vec<u32>, HashSet<u32>> = HashMap::new();
    for m in matched {
        by_row
            .entry(m.row_path.clone())
            .or_default()
            .extend(m.positions.iter().map(|&p| position_start(p)));
    }

    let mut out = Vec::new();
    for (row_path, want) in by_row {
        let Some(OwnedValue::String(text)) = value_for_row(value, full_path, &row_path) else {
            continue;
        };
        let (filtered, tokens) = analyzer.analyze_text(&text);
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for token in &tokens {
            if want.contains(&token.position) {
                ranges.push((token.start_byte, token.end_byte));
            }
        }
        if ranges.is_empty() {
            continue;
        }
        let ranges = merge_ranges(ranges);
        if opts.sentence {
            out.extend(fragments_by_sentence(&filtered, &ranges, opts));
        } else {
            out.extend(fragments_by_chars(&filtered, &ranges, opts));
        }
    }
    out
}

//recursive walkt to get the Balue in the precise row and precise [index]
//basically: "given those indices, what Value is actually there?"
#[timed(highlight)]
fn value_for_row<'a>(
    value: &'a OwnedValue,
    path: &str,
    row_path: &[u32],
) -> Option<&'a OwnedValue> {
    let segs: Vec<&str> = path.split('/').collect();
    let mut cur = value;
    let mut row_iter = row_path.iter();
    for &seg in &segs {
        if let OwnedValue::Array(items) = cur {
            cur = items.get(*row_iter.next()? as usize)?;
        }
        let OwnedValue::Object(obj) = cur else {
            return None;
        };
        cur = obj.get(seg)?;
    }
    Some(cur)
}

fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.0 <= last.1 => {
                last.1 = last.1.max(r.1);
            }
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
