use std::collections::BTreeMap;

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
#[must_use]
pub fn highlight_field_array(
    texts: &[String],
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    let (exact, regular): (Vec<&MatchedTerm>, Vec<&MatchedTerm>) =
        matched.iter().copied().partition(|m| m.exact);

    if !regular.is_empty() {
        let want = positions_with_weight(&regular);
        if !want.is_empty() {
            out.extend(highlight_flattened_texts(texts, analyzer, &want, opts));
        }
    }

    if !exact.is_empty() {
        let mut base = 0u32;
        for text in texts {
            let mut spans = exact_spans(text, base, &exact);
            if !spans.is_empty() {
                spans = merge_ranges(spans);
                if opts.sentence {
                    out.extend(fragments_by_sentence(text, &spans, opts));
                } else {
                    out.extend(fragments_by_chars(text, &spans, opts));
                }
            }
            let tokens = u32::try_from(exact_token_spans(text).len()).unwrap_or(u32::MAX);
            base = base
                .saturating_add(tokens)
                .saturating_add(1)
                .saturating_add(ARRAY_GAP);
        }
    }

    out
}

//                                  this bs:
//like ummm cast: {name, surname, aliases["", "", ""],}
#[timed(highlight)]
#[must_use]
pub fn highlight_field_row_keyed_flattened(
    value: &OwnedValue,
    full_path: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    let (exact, regular): (Vec<&MatchedTerm>, Vec<&MatchedTerm>) =
        matched.iter().copied().partition(|m| m.exact);

    if !regular.is_empty() {
        for (row_path, want) in wants_by_row(&regular) {
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
    }

    if !exact.is_empty() {
        let mut by_row: BTreeMap<Vec<u32>, Vec<&MatchedTerm>> = BTreeMap::new();
        for m in exact {
            by_row.entry(m.row_path.clone()).or_default().push(m);
        }
        for (row_path, terms) in by_row {
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

            let mut base = 0u32;
            for text in &texts {
                let mut spans = exact_spans(text, base, &terms);
                if !spans.is_empty() {
                    spans = merge_ranges(spans);
                    if opts.sentence {
                        out.extend(fragments_by_sentence(text, &spans, opts));
                    } else {
                        out.extend(fragments_by_chars(text, &spans, opts));
                    }
                }
                let tokens = u32::try_from(exact_token_spans(text).len()).unwrap_or(u32::MAX);
                base = base
                    .saturating_add(tokens)
                    .saturating_add(1)
                    .saturating_add(ARRAY_GAP);
            }
        }
    }

    out
}

//Builds highlighted fragments for one field value from the spans that matched.
//Fast path (offsets stored) wraps byte spans on `text` directly; slow path
//(char-filtered field, no offsets) re-analyzes and maps positions to spans on the
//filtered text.
#[timed(highlight)]
#[must_use]
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
            if let Some(&weight) = want.get(&base.saturating_add(token.position)) {
                ranges.push(Span {
                    start: token.start_byte,
                    end: token.end_byte,
                    weight,
                });
            }
            span = span.max(token.position.saturating_add(token.position_length.max(1)));
        }
        if !ranges.is_empty() {
            let ranges = merge_ranges(ranges);
            if opts.sentence {
                out.extend(fragments_by_sentence(&filtered, &ranges, opts));
            } else {
                out.extend(fragments_by_chars(&filtered, &ranges, opts));
            }
        }
        base = base.saturating_add(span).saturating_add(ARRAY_GAP);
    }
    out
}

//rewalks the json to get the precise element from the [index] of the array
#[timed(highlight)]
#[must_use]
pub fn highlight_field_row_keyed(
    value: &OwnedValue,
    full_path: &str,
    analyzer: &Analyzer,
    matched: &[&MatchedTerm],
    opts: &HighlightFieldOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    let (exact, regular): (Vec<&MatchedTerm>, Vec<&MatchedTerm>) =
        matched.iter().copied().partition(|m| m.exact);

    if !regular.is_empty() {
        for (row_path, want) in wants_by_row(&regular) {
            let Some(OwnedValue::String(text)) = value_for_row(value, full_path, &row_path) else {
                continue;
            };
            let (filtered, tokens) = analyzer.analyze_text(text);
            let ranges: Vec<Span> = tokens
                .iter()
                .filter_map(|token| {
                    want.get(&token.position).map(|&weight| Span {
                        start: token.start_byte,
                        end: token.end_byte,
                        weight,
                    })
                })
                .collect();
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
    }

    if !exact.is_empty() {
        let mut by_row: BTreeMap<Vec<u32>, Vec<&MatchedTerm>> = BTreeMap::new();
        for m in exact {
            by_row.entry(m.row_path.clone()).or_default().push(m);
        }
        for (row_path, terms) in by_row {
            let Some(OwnedValue::String(text)) = value_for_row(value, full_path, &row_path) else {
                continue;
            };
            let mut spans = exact_spans(text, 0, &terms);
            if spans.is_empty() {
                continue;
            }
            spans = merge_ranges(spans);
            if opts.sentence {
                out.extend(fragments_by_sentence(text, &spans, opts));
            } else {
                out.extend(fragments_by_chars(text, &spans, opts));
            }
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
    let mut cur = value;
    let mut row_iter = row_path.iter();
    for seg in path.split('/') {
        if let OwnedValue::Array(items) = cur {
            let idx = usize::try_from(*row_iter.next()?).ok()?;
            cur = items.get(idx)?;
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
        add_weights(&mut want, m);
    }
    want
}

//keeps the strongest weight per token start position
fn add_weights(want: &mut HashMap<u32, f32>, m: &MatchedTerm) {
    for &p in &m.positions {
        let cur = want.entry(position_start(p)).or_insert(m.weight);
        *cur = cur.max(m.weight);
    }
}

fn wants_by_row(matched: &[&MatchedTerm]) -> BTreeMap<Vec<u32>, HashMap<u32, f32>> {
    let mut by_row: BTreeMap<Vec<u32>, HashMap<u32, f32>> = BTreeMap::new();
    for m in matched {
        add_weights(by_row.entry(m.row_path.clone()).or_default(), m);
    }
    by_row
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
        if span.end <= ws || span.start >= we {
            continue;
        }
        let s = span.start.max(ws);
        let e = span.end.min(we);
        out.push_str(text.get(pos..s).unwrap_or_default());
        out.push_str(pre);
        out.push_str(text.get(s..e).unwrap_or_default());
        out.push_str(post);
        pos = e;
    }

    out.push_str(text.get(pos..we).unwrap_or_default());
    out.trim().to_owned()
}

fn fragments_by_chars(text: &str, ranges: &[Span], opts: &HighlightFieldOptions) -> Vec<String> {
    if opts.number_of_fragments == 0 {
        return Vec::new();
    }
    if text.len() <= opts.fragment_size {
        return vec![build_snippet(text, 0, text.len(), ranges, opts)];
    }
    let windows = fragment_windows(text, ranges, opts.fragment_size);
    let top = top_regions(
        ranges,
        &windows,
        opts.fragment_size,
        opts.number_of_fragments,
    );
    top.into_iter()
        .map(|(ws, we)| build_snippet(text, ws, we, ranges, opts))
        .collect()
}

fn fragments_by_sentence(text: &str, ranges: &[Span], opts: &HighlightFieldOptions) -> Vec<String> {
    if opts.number_of_fragments == 0 {
        return Vec::new();
    }
    let matching: Vec<(usize, usize)> = split_sentences(text)
        .into_iter()
        .filter(|&(start, end)| {
            ranges
                .iter()
                .any(|span| span.start < end && span.end > start)
        })
        .collect();
    let top = top_regions(
        ranges,
        &matching,
        opts.fragment_size,
        opts.number_of_fragments,
    );
    top.into_iter()
        .filter_map(|(start, end)| {
            let (ws, we) = sentence_window(text, ranges, start, end, opts)?;
            Some(build_snippet(text, ws, we, ranges, opts))
        })
        .collect()
}

fn sentence_window(
    text: &str,
    ranges: &[Span],
    start: usize,
    end: usize,
    opts: &HighlightFieldOptions,
) -> Option<(usize, usize)> {
    if end.saturating_sub(start) <= opts.fragment_size {
        return Some((start, end));
    }
    // long sentence: anchor at the sentence boundary and cut to
    // fragment_size at a word boundary; slide only when the best
    // match is further in than one fragment.
    let best = ranges
        .iter()
        .filter(|span| span.start < end && span.end > start)
        .max_by(|a, b| {
            a.weight
                .total_cmp(&b.weight)
                .then_with(|| b.start.cmp(&a.start))
        })?;
    let half = opts.fragment_size / 2;
    let anchored_we = snap_up(text, start.saturating_add(opts.fragment_size).min(end));
    if best.start < anchored_we {
        Some((start, anchored_we))
    } else {
        Some((
            snap_word_down(text, best.start.saturating_sub(half)),
            snap_word_up(text, best.end.saturating_add(half).min(end)),
        ))
    }
}
fn split_sentences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (offset, sentence) in text.split_sentence_bound_indices() {
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lead = sentence.len().saturating_sub(sentence.trim_start().len());
        let start = offset.saturating_add(lead);
        out.push((start, start.saturating_add(trimmed.len())));
    }
    out
}

//these two so that doesnt get cut off mid-word
const fn snap_down(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

const fn snap_up(text: &str, mut i: usize) -> usize {
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn snap_word_down(text: &str, i: usize) -> usize {
    let i = snap_down(text, i);
    text.get(..i)
        .unwrap_or_default()
        .char_indices()
        .rev()
        .find(|&(_, c)| c.is_whitespace())
        .map_or(0, |(idx, c)| idx + c.len_utf8())
}

fn snap_word_up(text: &str, i: usize) -> usize {
    let i = snap_up(text, i);
    text.get(i..)
        .unwrap_or_default()
        .char_indices()
        .find(|&(_, c)| c.is_whitespace())
        .map_or(text.len(), |(idx, _)| i + idx)
}

//stupid smart function that combines spans that are in the same window
fn fragment_windows(text: &str, spans: &[Span], fragment_size: usize) -> Vec<(usize, usize)> {
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for span in spans {
        match windows.last_mut() {
            Some((ws, we)) if span.end.saturating_sub(*ws) <= fragment_size => {
                *we = snap_up(text, span.end.max(*we));
            }
            _ => {
                let ws = snap_word_down(text, span.start);
                let we = snap_word_up(text, (ws + fragment_size).max(span.end).min(text.len()));
                windows.push((ws, we));
            }
        }
    }
    windows
}

#[allow(
    clippy::cast_precision_loss,
    reason = "byte lengths only feed a heuristic closeness score"
)]
fn ratio(num: usize, den: usize) -> f32 {
    num as f32 / den.max(1) as f32
}

const DENSITY_BOOST: f32 = 0.5;
//score for multiple spans in a single region
fn region_score(spans: &[Span], start: usize, end: usize, fragment_size: usize) -> f32 {
    let mut total = 0.0f32;
    let mut first = usize::MAX;
    let mut last = 0usize;
    for span in spans {
        if span.start < end && span.end > start {
            total += span.weight;
            first = first.min(span.start);
            last = last.max(span.end);
        }
    }
    if first == usize::MAX {
        return 0.0;
    }
    let extent = last.saturating_sub(first).max(1);
    let closeness = 1.0 - ratio(extent, fragment_size);
    total * DENSITY_BOOST.mul_add(closeness.max(0.0), 1.0)
}

fn top_regions(
    spans: &[Span],
    regions: &[(usize, usize)],
    fragment_size: usize,
    k: usize,
) -> Vec<(usize, usize)> {
    let mut scored: Vec<((usize, usize), f32)> = regions
        .iter()
        .map(|&(start, end)| {
            let score = region_score(spans, start, end, fragment_size);
            ((start, end), score)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
    scored
        .into_iter()
        .take(k)
        .map(|(region, _)| region)
        .collect()
}

#[must_use]
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

fn exact_token_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for token in text.split_whitespace() {
        let start = text[cursor..].find(token).map_or(cursor, |at| cursor + at);
        let end = start.saturating_add(token.len());
        spans.push((start, end));
        cursor = end;
    }
    spans
}

fn exact_spans(text: &str, base: u32, matched: &[&MatchedTerm]) -> Vec<Span> {
    let tokens = exact_token_spans(text);
    let mut wants: HashMap<usize, f32> = HashMap::new();

    for m in matched {
        for &position in &m.positions {
            let start = position_start(position);
            if start <= base {
                continue;
            }
            let index = start - base - 1;
            let Ok(index) = usize::try_from(index) else {
                continue;
            };
            if index >= tokens.len() {
                continue;
            }
            let weight = wants.entry(index).or_insert(m.weight);
            *weight = weight.max(m.weight);
        }
    }

    wants
        .into_iter()
        .map(|(index, weight)| {
            let (start, end) = tokens[index];
            Span { start, end, weight }
        })
        .collect()
}
