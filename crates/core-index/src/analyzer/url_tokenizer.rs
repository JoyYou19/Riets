use std::collections::HashSet;

use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

// "https://www.Example.com/path" -> "Example.com"
fn domain_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme
        .split(|c| matches!(c, '/' | '?' | '#'))
        .next()?;
    let host = host.rsplit('@').next().unwrap_or(host); // strip "user:pass@"

    if host.is_empty() {
        return None;
    }

    // drop the leading subdomain only when there is one ("www.example.com" -> "example.com")
    if host.matches('.').count() >= 2 {
        Some(
            host.split_once('.')
                .map(|(_, domain)| domain.to_string())
                .unwrap_or_else(|| host.to_string()),
        )
    } else {
        Some(host.to_string())
    }
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

    fn emit(
        &self,
        chunk: &str,
        from: usize,
        to: usize,
        position: &mut usize,
        tokens: &mut Vec<Token>,
    ) {
        let is_delim = |c: char| !c.is_alphanumeric() && !self.literal_symbols.contains(&c);
        let parts: Vec<String> = chunk
            .split(is_delim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        let span = parts.len().max(1);
        let start = *position;

        let mut push = |text: &str, position: usize, position_length: usize| {
            tokens.push(Token {
                offset_from: from,
                offset_to: to,
                position,
                text: text.to_string(),
                position_length,
            });
        };

        if self.preserve_original {
            push(chunk, start, span); // whole URL
        }
        if let Some(domain) = domain_of(chunk) {
            if domain != chunk {
                push(&domain, start, span); // "example.com"
            }
        }
        for (index, part) in parts.iter().enumerate() {
            push(part, start + index, 1); // everything split
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
}
