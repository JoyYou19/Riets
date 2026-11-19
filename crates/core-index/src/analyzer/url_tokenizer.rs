use std::collections::HashSet;

use crate::analyzer::small_tantivy::{BoxTokenStream, Token, TokenStream, Tokenizer};

// "https://www.Example.com/path" -> "Example.com"
fn domain_of(url: &str) -> Option<(&str, usize)> {
    let scheme_end = url.find("://").map_or(0, |i| i + 3);
    let after_scheme = &url[scheme_end..];

    // the host is the prefix of after_scheme up to the first '/', '?' or '#'
    let host = after_scheme.split(['/', '?', '#']).next()?;

    // strip "user:pass@"; host_after_at is the last-'@' suffix of `host`
    let host_after_at = host.rsplit('@').next().unwrap_or(host);
    if host_after_at.is_empty() {
        return None;
    }

    // `host` starts at 0 within after_scheme, so host_after_at starts at
    // (host.len() - host_after_at.len()) within it
    let host_start = scheme_end + (host.len() - host_after_at.len());

    const WWW_DOT: &str = "www.";
    if host_after_at
        .get(..WWW_DOT.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(WWW_DOT))
    {
        return Some((&host_after_at[WWW_DOT.len()..], host_start + WWW_DOT.len()));
    }
    Some((host_after_at, host_start))
}

// "https://www.Example.com/path" -> whole URL + "Example.com" + parts
#[derive(Clone)]
pub struct UrlTokenizer {
    literal_symbols: HashSet<char>,
    preserve_original: bool,
}

impl UrlTokenizer {
    pub fn new(literal_symbols: impl IntoIterator<Item = char>, preserve_original: bool) -> Self {
        Self {
            literal_symbols: literal_symbols.into_iter().collect(),
            preserve_original,
        }
    }

    fn emit(&self, chunk: &str, from: usize, position: &mut usize, tokens: &mut Vec<Token>) {
        let is_delim = |c: char| !c.is_alphanumeric() && !self.literal_symbols.contains(&c);

        // (text, start_byte, end_byte) within `chunk`, so each token carries its own span
        let mut parts: Vec<(String, usize, usize)> = Vec::new();
        let mut part_start = 0usize;
        for (idx, c) in chunk.char_indices() {
            if is_delim(c) {
                if part_start < idx {
                    parts.push((chunk[part_start..idx].to_string(), part_start, idx));
                }
                part_start = idx + c.len_utf8();
            }
        }
        if part_start < chunk.len() {
            parts.push((chunk[part_start..].to_string(), part_start, chunk.len()));
        }

        let span = parts.len().max(1);
        let start = *position;

        let mut push =
            |text: &str, start_off: usize, end_off: usize, pos: usize, pos_len: usize| {
                tokens.push(Token {
                    offset_from: from + start_off,
                    offset_to: from + end_off,
                    position: pos,
                    text: text.to_string(),
                    position_length: pos_len,
                });
            };

        if self.preserve_original {
            push(chunk, 0, chunk.len(), start, span); // whole URL
        }
        if let Some((domain, domain_offset)) = domain_of(chunk)
            && domain != chunk
        {
            push(
                domain,
                domain_offset,
                domain_offset + domain.len(),
                start,
                span,
            );
        }
        for (index, (part, p_start, p_end)) in parts.iter().enumerate() {
            push(part, *p_start, *p_end, start + index, 1);
        }

        *position = start + span;
    }
}

impl Tokenizer for UrlTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        let mut tokens = Vec::new();
        let mut position = 0usize;
        let mut chunk_start: Option<usize> = None;

        for (idx, ch) in text.char_indices() {
            if ch.is_whitespace() {
                if let Some(s) = chunk_start.take() {
                    self.emit(&text[s..idx], s, &mut position, &mut tokens);
                }
            } else if chunk_start.is_none() {
                chunk_start = Some(idx);
            }
        }
        if let Some(s) = chunk_start {
            self.emit(&text[s..], s, &mut position, &mut tokens);
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

    fn tokens(t: &mut UrlTokenizer, s: &str) -> Vec<String> {
        let mut stream = t.token_stream(s);
        let mut out = Vec::new();
        while let Some(tok) = stream.next() {
            out.push(tok.text.clone());
        }
        out
    }

    #[test]
    fn url_splits_and_keeps_domain() {
        let mut t = UrlTokenizer::new(['_'], true);
        assert_eq!(
            tokens(
                &mut t,
                "https://www.Example.com/path/to/page.html?id=42#top"
            ),
            vec![
                "https://www.Example.com/path/to/page.html?id=42#top",
                "Example.com",
                "https",
                "www",
                "Example",
                "com",
                "path",
                "to",
                "page",
                "html",
                "id",
                "42",
                "top"
            ]
        );
    }

    #[test]
    fn bare_domain_no_subdomain() {
        let mut t = UrlTokenizer::new(['_'], true);
        assert_eq!(
            tokens(&mut t, "example.com"),
            vec!["example.com", "example", "com"]
        );
    }

    #[test]
    fn domain_keeps_multi_label_and_ip() {
        assert_eq!(domain_of("example.co.uk"), Some(("example.co.uk", 0)));
        assert_eq!(domain_of("192.168.1.1"), Some(("192.168.1.1", 0)));
        assert_eq!(
            domain_of("https://www.example.co.uk/path"),
            Some(("example.co.uk", 12))
        );
    }

    #[test]
    fn url_tokens_carry_their_own_offsets() {
        let mut t = UrlTokenizer::new(['_'], true);
        let s = "https://www.example.com/path";
        let mut stream = t.token_stream(s);
        let mut got = Vec::new();
        while let Some(tok) = stream.next() {
            got.push((tok.text.clone(), tok.offset_from, tok.offset_to));
        }
        let path_start = s.find("path").unwrap();
        assert!(got.iter().any(|(text, from, to)| text == "path"
            && *from == path_start
            && *to == path_start + 4));
    }
}
