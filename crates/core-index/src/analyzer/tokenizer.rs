use crate::analyzer::token::RawToken;

/*
* Splits a simple text into raw normalized tokens
*/
#[derive(Debug, Clone, Default)]
pub struct SimpleTokenizer;

impl SimpleTokenizer {
    pub fn tokenize(&self, input: &str) -> Vec<RawToken> {
        let mut tokens = Vec::new();
        let mut start = None;
        let mut position = 0;

        for (idx, ch) in input.char_indices() {
            if ch.is_alphanumeric() {
                if start.is_none() {
                    start = Some(idx);
                }
            } else if let Some(s) = start.take() {
                tokens.push(RawToken {
                    text: input[s..idx].to_string(),
                    position,
                    start_byte: s,
                    end_byte: idx,
                });

                position += 1;
            }
        }

        if let Some(s) = start {
            tokens.push(RawToken {
                text: input[s..].to_string(),
                position,
                start_byte: s,
                end_byte: input.len(),
            });
        }

        tokens
    }
}
