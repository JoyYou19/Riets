use std::collections::HashSet;

use tantivy::tokenizer::{BoxTokenStream, Token, TokenStream, Tokenizer};

// "src/core/shard_db.rs" -> whole path + "src" "core" "shard_db" "rs"
#[derive(Clone)]
pub struct FilepathTokenizer {
    literal_symbols: HashSet<char>,
    preserve_original: bool,
}

impl FilepathTokenizer {
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
        // split on anything non-alphanumeric (/, \, ., :, -) except literal symbols like "_"
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
            push(chunk, start, span); // whole path
        }
        for (index, part) in parts.iter().enumerate() {
            push(part, start + index, 1); // each segment
        }

        *position = start + span;
    }
}

impl Tokenizer for FilepathTokenizer {
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

    fn tokens(t: &mut FilepathTokenizer, s: &str) -> Vec<String> {
        let mut stream = t.token_stream(s);
        let mut out = Vec::new();
        while let Some(tok) = stream.next() {
            out.push(tok.text.clone());
        }
        out
    }

    #[test]
    fn unix_path() {
        let mut t = FilepathTokenizer::new(['_'], true);
        assert_eq!(
            tokens(&mut t, "src/core/shard_db.rs"),
            vec!["src/core/shard_db.rs", "src", "core", "shard_db", "rs"]
        );
    }

    #[test]
    fn windows_path() {
        let mut t = FilepathTokenizer::new(['_'], true);
        assert_eq!(
            tokens(&mut t, "C:\\Users\\foo\\main.rs"),
            vec!["C:\\Users\\foo\\main.rs", "C", "Users", "foo", "main", "rs"]
        );
    }
}
