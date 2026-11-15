//! Word-delimiter tokenizer: turns one chunk of text into several searchable
//! forms so different spellings of the same thing all match.
//!
//! Text is first split on whitespace into "chunks". Each chunk is then expanded
//! into up to three kinds of term:
//!
//!   original — the chunk exactly as written        ("spider-man")
//!   parts    — the individual fragments            ("spider", "man")
//!   joined   — all fragments glued together        ("spiderman")
//!
//! # Where a chunk is split
//!   1. delimiters   — any non-alphanumeric char NOT in `literal_symbols`
//!                     (default literal symbol is `_`, so `_` stays whole)
//!   2. case change  — camelCase boundaries (only when `split_case`)
//!   3. digit change — letter ↔ digit (only when `split_numerics`)
//!
//!   - all fragments are emitted (including 1-char); length filtering is a
//!     separate post-split filter (`min_length` / `max_length`)
//!
//! # Examples (`literal_symbols` = `_`)
//!
//! | input             | flags          | tokens                                        |
//! |-------------------|----------------|-----------------------------------------------|
//! | `spider-man`      | —              | `spider-man` `spider` `man` `spiderman`       |
//! | `spider_man`      | —              | `spider_man`                                  |
//! | `king's`          | —              | `king's` `king` `kings`                       |
//! | `o'clock`         | —              | `o'clock` `clock` `oclock`                    |
//! | `C++`             | —              | `C++` `C`                                     |
//! | `getHTTPResponse` | `split_case`     | `getHTTPResponse` `get` `HTTP` `Response`     |
//! | `XMLHttpRequest`  | `split_case`     | `XMLHttpRequest` `XML` `Http` `Request`       |
//! | `utf8`            | `split_numerics` | `utf8` `utf` '8'                              |
//! | `page42`          | `split_numerics` | `page42` `page` `42`                          |
//!
//! # Position layout
//! `original` and `joined` sit at the chunk's position and span all parts;
//! each `part` gets its own successive position. For `spider-man`:
//!
//!   pos 0: `spider-man` `spiderman` `spider`
//!   pos 1: `man`
//!
//! So a phrase search for "spider man" matches, while a single-term search for
//! "spiderman" also matches — both spellings are in the index.

use std::collections::HashSet;

use crate::analyzer::small_tantivy::{BoxTokenStream, Token, TokenStream, Tokenizer};

fn split_where(
    segment: &str,
    boundary: impl Fn(char, char, Option<char>) -> bool,
) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = segment.char_indices().collect();
    if chars.len() < 2 {
        return vec![(0, segment.len())];
    }

    let mut bounds = vec![0usize];
    for i in 1..chars.len() {
        let prev = chars[i - 1].1;
        let curr = chars[i].1;
        let next = chars.get(i + 1).map(|&(_, c)| c);
        if boundary(prev, curr, next) {
            bounds.push(chars[i].0);
        }
    }

    let mut out = Vec::with_capacity(bounds.len());
    for w in bounds.windows(2) {
        out.push((w[0], w[1]));
    }
    out.push((*bounds.last().unwrap(), segment.len()));
    out
}

//getHTTPResponse -> get HTTP Response getHTTPResponse
fn split_on_case(segment: &str) -> Vec<(usize, usize)> {
    split_where(segment, |prev, curr, next| {
        (prev.is_lowercase() && curr.is_uppercase())
            || (prev.is_uppercase() && curr.is_uppercase() && next.is_some_and(char::is_lowercase))
    })
}

//utf8 -> utf8 utf 8
fn split_on_numerics(segment: &str) -> Vec<(usize, usize)> {
    split_where(segment, |prev, curr, _| {
        prev.is_alphabetic() != curr.is_alphabetic()
    })
}

fn split_on_delims(chunk: &str, is_delim: impl Fn(char) -> bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in chunk.char_indices() {
        if is_delim(ch) {
            if let Some(s) = start.take() {
                out.push((s, idx));
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(s) = start {
        out.push((s, chunk.len()));
    }
    out
}

fn split_forms(
    chunk: &str,
    literal_symbols: &HashSet<char>,
    split_case: bool,
    split_numerics: bool,
) -> (String, Vec<(String, usize, usize)>, String) {
    let is_delim = |c: char| !c.is_alphanumeric() && !literal_symbols.contains(&c);

    let mut parts: Vec<(String, usize, usize)> = Vec::new();
    for (seg_start, seg_end) in split_on_delims(chunk, is_delim) {
        let mut sub: Vec<(usize, usize)> = vec![(seg_start, seg_end)];
        //yeah \/
        if split_case {
            sub = sub
                .into_iter()
                .flat_map(|(s, e)| {
                    split_on_case(&chunk[s..e])
                        .into_iter()
                        .map(move |(a, b)| (s + a, s + b))
                })
                .collect();
        }
        if split_numerics {
            sub = sub
                .into_iter()
                .flat_map(|(s, e)| {
                    split_on_numerics(&chunk[s..e])
                        .into_iter()
                        .map(move |(a, b)| (s + a, s + b))
                })
                .collect();
        }
        for (s, e) in sub {
            parts.push((chunk[s..e].to_string(), s, e));
        }
    }

    let joined: String = parts.iter().map(|(p, _, _)| p.as_str()).collect();
    (chunk.to_string(), parts, joined)
}

pub fn expand_word_delimiters(
    word: &str,
    literal_symbols: &HashSet<char>,
    split_case: bool,
    split_numerics: bool,
) -> Vec<String> {
    let (original, parts, joined) = split_forms(word, literal_symbols, split_case, split_numerics);

    if parts.len() <= 1 {
        return vec![original];
    }

    let mut out = vec![original];
    out.extend(parts.iter().map(|(p, _, _)| p.clone()));

    if !joined.is_empty() && joined != out[0] {
        out.push(joined);
    }
    out
}

#[derive(Clone)]
pub struct WordDelimiterTokenizer {
    literal_symbols: HashSet<char>,
    split_case: bool,
    split_numerics: bool,
    preserve_original: bool,
}

impl WordDelimiterTokenizer {
    pub fn new(literal_symbols: impl IntoIterator<Item = char>) -> Self {
        Self::with_options(literal_symbols, false, false, true)
    }

    pub fn with_options(
        literal_symbols: impl IntoIterator<Item = char>,
        split_case: bool,
        split_numerics: bool,
        preserve_original: bool,
    ) -> Self {
        Self {
            literal_symbols: literal_symbols.into_iter().collect(),
            split_case,
            split_numerics,
            preserve_original,
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
        let (original, parts, joined) = split_forms(
            chunk,
            &self.literal_symbols,
            self.split_case,
            self.split_numerics,
        );

        let parts_to_emit: Vec<&(String, usize, usize)> =
            if parts.len() > 1 || !self.preserve_original {
                parts.iter().collect()
            } else {
                Vec::new()
            };
        let span = parts_to_emit.len().max(1);
        let start = *position;

        let mut push =
            |text: &str, from: usize, to: usize, position: usize, position_length: usize| {
                tokens.push(Token {
                    offset_from: from,
                    offset_to: to,
                    position,
                    text: text.to_string(),
                    position_length,
                });
            };

        if self.preserve_original {
            push(&original, from, to, start, span);
        }

        if !joined.is_empty() && joined != original {
            let j_from = from + parts.first().map(|(_, s, _)| *s).unwrap_or(0);
            let j_to = from + parts.last().map(|(_, _, e)| *e).unwrap_or(0);
            push(&joined, j_from, j_to, start, span);
        }

        for (index, part) in parts_to_emit.iter().enumerate() {
            push(&part.0, from + part.1, from + part.2, start + index, 1);
        }

        *position = start + span;
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
    // moves to the next token
    fn advance(&mut self) -> bool {
        if self.idx < self.tokens.len() {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    // current token
    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }

    // current token (mutable)
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
            expand_word_delimiters("spider_man", &syms(&['_']), false, false),
            vec!["spider_man".to_string()]
        );
    }

    #[test]
    fn hyphen_splits_catenates_preserves() {
        assert_eq!(
            expand_word_delimiters("spider-man", &syms(&['_']), false, false),
            vec!["spider-man", "spider", "man", "spiderman"]
        );
    }

    #[test]
    fn no_delimiter_unchanged() {
        assert_eq!(
            expand_word_delimiters("spiderman", &syms(&['_']), false, false),
            vec!["spiderman".to_string()]
        );
    }

    #[test]
    fn case_split() {
        assert_eq!(
            expand_word_delimiters("getHTTPResponse", &syms(&['_']), true, false),
            vec!["getHTTPResponse", "get", "HTTP", "Response"]
        );
    }

    #[test]
    fn numeric_split() {
        assert_eq!(
            expand_word_delimiters("page42", &syms(&['_']), false, true),
            vec!["page42", "page", "42"]
        );
    }

    #[test]
    fn code_split_drops_one_char_fragment() {
        assert_eq!(
            expand_word_delimiters("utf8", &syms(&['_']), true, true),
            vec!["utf8", "utf", "8"]
        );
    }

    #[test]
    fn split_forms_offsets_delimiter() {
        let syms = syms(&['_']);
        let (_orig, parts, _joined) = split_forms("spider-man", &syms, false, false);
        assert_eq!(
            parts,
            vec![("spider".to_string(), 0, 6), ("man".to_string(), 7, 10),]
        );
    }

    #[test]
    fn split_forms_offsets_case() {
        let syms = syms(&['_']);
        let (_orig, parts, _joined) = split_forms("getHTTPResponse", &syms, true, false);
        assert_eq!(
            parts,
            vec![
                ("get".to_string(), 0, 3),
                ("HTTP".to_string(), 3, 7),
                ("Response".to_string(), 7, 15),
            ]
        );
    }

    #[test]
    fn split_forms_offsets_utf8_bytes() {
        let syms = syms(&['_']);
        // 'ģ' is 2 UTF-8 bytes, so "ģimene" is bytes 0..7, not 0..6.
        let (_orig, parts, _joined) = split_forms("ģimene-ozols", &syms, false, false);
        assert_eq!(
            parts,
            vec![("ģimene".to_string(), 0, 7), ("ozols".to_string(), 8, 13),]
        );
    }
}
