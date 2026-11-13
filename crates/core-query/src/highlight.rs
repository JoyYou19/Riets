use crate::MatchedTerm;
use ahash::{HashMap, HashMapExt};
use core_index::analyzer::Analyzer;
use core_index::document::policy::ARRAY_GAP;
use core_index::types::position_start;
use core_protocol::command_reponse_definitions::HighlightFieldOptions;
use core_timing::timed;
use simd_json::OwnedValue;
use unicode_segmentation::UnicodeSegmentation;

//we need to re-analyze flattened arrays cause the position and offsets dont match in them
#[timed(highlight)]
pub fn highlight_field_array(
    texts: &[String],
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let want = positions_with_weight(matched);
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
    let mut by_row: HashMap<Vec<u32>, HashMap<u32, f32>> = HashMap::new();
    for m in matched {
        let want = by_row.entry(m.row_path.clone()).or_default();
        for &p in &m.positions {
            let pos = position_start(p);
            let cur = want.entry(pos).or_insert(m.weight);
            if m.weight > *cur {
                *cur = m.weight;
            }
        }
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
        let ranges: Vec<Span> = matched
            .iter()
            .flat_map(|m| {
                let weight = m.weight;
                m.offsets.iter().map(move |&(s, e)| Span {
                    start: s as usize,
                    end: e as usize,
                    weight,
                })
            })
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
    let want = positions_with_weight(matched);
    let mut ranges: Vec<Span> = Vec::new();
    for token in &tokens {
        if let Some(&weight) = want.get(&token.position) {
            ranges.push(Span {
                start: token.start_byte,
                end: token.end_byte,
                weight,
            });
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
    want: &HashMap<u32, f32>,
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut base = 0u32;
    for text in texts {
        let (filtered, tokens) = analyzer.analyze_text(text);
        let mut ranges: Vec<Span> = Vec::new();
        let mut span = 0u32;
        for token in &tokens {
            if let Some(&weight) = want.get(&(base + token.position)) {
                ranges.push(Span {
                    start: token.start_byte,
                    end: token.end_byte,
                    weight,
                });
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
#[timed(highlight)]
pub fn highlight_field_row_keyed(
    value: &OwnedValue,
    full_path: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut by_row: HashMap<Vec<u32>, HashMap<u32, f32>> = HashMap::new();
    for m in matched {
        let want = by_row.entry(m.row_path.clone()).or_default();
        for &p in &m.positions {
            let pos = position_start(p);
            let cur = want.entry(pos).or_insert(m.weight);
            if m.weight > *cur {
                *cur = m.weight;
            }
        }
    }

    let mut out = Vec::new();
    for (row_path, want) in by_row {
        let Some(OwnedValue::String(text)) = value_for_row(value, full_path, &row_path) else {
            continue;
        };
        let (filtered, tokens) = analyzer.analyze_text(&text);
        let mut ranges: Vec<Span> = Vec::new();
        for token in &tokens {
            if let Some(&weight) = want.get(&token.position) {
                ranges.push(Span {
                    start: token.start_byte,
                    end: token.end_byte,
                    weight,
                });
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

//one match span with relevance, when two matches overlap they merge (no conflict)
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    weight: f32,
}

//TOKENs start position -> strongest weight per field takes the max weight for each doesnt add up so
//that it doesnt inflate like synonym matches yk?
fn positions_with_weight(matched: &[&MatchedTerm]) -> HashMap<u32, f32> {
    let mut want: HashMap<u32, f32> = HashMap::new();
    for m in matched {
        for &p in &m.positions {
            let pos = position_start(p);
            let cur = want.entry(pos).or_insert(m.weight);
            if m.weight > *cur {
                *cur = m.weight;
            }
        }
    }
    want
}

//if ranges overlap we merge them + add weight to one another
fn merge_ranges(mut ranges: Vec<Span>) -> Vec<Span> {
    ranges.sort_unstable_by_key(|s| (s.start, s.end));
    let mut merged: Vec<Span> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                last.end = last.end.max(r.end);
                last.weight += r.weight;
            }
            _ => merged.push(r),
        }
    }
    merged
}

fn tags(opts: &HighlightFieldOptions) -> (&str, &str) {
    let pre = opts.pre_tags.first().map_or("", String::as_str);
    let post = opts.post_tags.first().map_or("", String::as_str);
    (pre, post)
}

fn build_snippet(
    text: &str,
    ws: usize,
    we: usize,
    ranges: &[Span],
    opts: &HighlightFieldOptions,
) -> String {
    let (pre, post) = tags(opts);
    let mut out = String::new();
    let mut pos = ws;
    for span in ranges {
        let rs = span.start;
        let re = span.end;
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
    out.trim().to_string()
}

fn fragments_by_chars(text: &str, ranges: &[Span], opts: &HighlightFieldOptions) -> Vec<String> {
    if text.len() <= opts.fragment_size {
        return vec![build_snippet(text, 0, text.len(), ranges, opts)];
    }
    let mut out = Vec::new();
    for span in ranges.iter().take(opts.number_of_fragments) {
        let half = opts.fragment_size / 2;
        let ws = snap_down(text, span.start.saturating_sub(half));
        let we = snap_up(text, (span.end + half).min(text.len()));
        out.push(build_snippet(text, ws, we, ranges, opts));
    }
    out
}

fn fragments_by_sentence(text: &str, ranges: &[Span], opts: &HighlightFieldOptions) -> Vec<String> {
    let mut out = Vec::new();
    for (start, end) in split_sentences(text) {
        if ranges
            .iter()
            .any(|span| span.start < end && span.end > start)
        {
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
    for (offset, sentence) in text.split_sentence_bound_indices() {
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lead = sentence.len() - sentence.trim_start().len();
        let start = offset + lead;
        out.push((start, start + trimmed.len()));
    }
    out
}

//these two so that doesnt get cut off mid-word
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

/// Non-overlapping, word-aligned windows of ~`fragment_size` bytes covering the
/// matched spans in document order. A span that fits inside the current window
/// extends it; a span that does not fit starts a new window.
fn fragment_windows(text: &str, spans: &[Span], fragment_size: usize) -> Vec<(usize, usize)> {
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        match windows.last_mut() {
            Some((ws, we)) if span.end.saturating_sub(*ws) <= fragment_size => {
                *we = snap_up(text, span.end.max(*we));
            }
            _ => {
                let ws = snap_down(text, span.start);
                let we = snap_up(text, (ws + fragment_size).max(span.end).min(text.len()));
                windows.push((ws, we));
            }
        }
    }
    windows
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
