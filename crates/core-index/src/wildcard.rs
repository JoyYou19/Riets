/*
* For many queries we will want to support
* things like architect* to also return architecture and architects instead of just the plain
* architect, so we create wildcard tokens to support those features
*/
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WildcardToken {
    Literal(String),
    Any,
    One,
    OneOf(Vec<char>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardPattern {
    tokens: Vec<WildcardToken>,
}

impl WildcardPattern {
    pub fn parse(input: &str) -> Self {
        let mut tokens = Vec::new();
        let mut literal = String::new();
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '*' => {
                    if !literal.is_empty() {
                        tokens.push(WildcardToken::Literal(std::mem::take(&mut literal)));
                    }
                    if !matches!(tokens.last(), Some(WildcardToken::Any)) {
                        tokens.push(WildcardToken::Any);
                    }
                }
                '?' => {
                    if !literal.is_empty() {
                        tokens.push(WildcardToken::Literal(std::mem::take(&mut literal)));
                    }
                    tokens.push(WildcardToken::One);
                }
                '[' => {
                    if !literal.is_empty() {
                        tokens.push(WildcardToken::Literal(std::mem::take(&mut literal)));
                    }

                    let mut set = Vec::new();
                    for inner in chars.by_ref() {
                        if inner == ']' {
                            break;
                        }
                        set.push(inner);
                    }

                    tokens.push(WildcardToken::OneOf(set));
                }
                _ => literal.push(ch),
            }
        }

        if !literal.is_empty() {
            tokens.push(WildcardToken::Literal(literal));
        }

        Self { tokens }
    }

    pub fn prefix(&self) -> &str {
        match self.tokens.first() {
            Some(WildcardToken::Literal(s)) => s,
            _ => "",
        }
    }

    pub fn is_prefix_only(&self) -> bool {
        matches!(
            self.tokens.as_slice(),
            [WildcardToken::Literal(_), WildcardToken::Any]
        )
    }

    pub fn matches(&self, word: &str) -> bool {
        let chars: Vec<char> = word.chars().collect();
        self.matches_from(0, 0, &chars)
    }

    fn matches_from(&self, token_i: usize, char_i: usize, chars: &[char]) -> bool {
        if token_i == self.tokens.len() {
            return char_i == chars.len();
        }

        match &self.tokens[token_i] {
            WildcardToken::Literal(lit) => {
                let lit_chars: Vec<char> = lit.chars().collect();

                if chars.len() < char_i + lit_chars.len() {
                    return false;
                }

                if chars[char_i..char_i + lit_chars.len()] == lit_chars[..] {
                    self.matches_from(token_i + 1, char_i + lit_chars.len(), chars)
                } else {
                    false
                }
            }

            WildcardToken::Any => {
                for next_i in char_i..=chars.len() {
                    if self.matches_from(token_i + 1, next_i, chars) {
                        return true;
                    }
                }
                false
            }

            WildcardToken::One => {
                if char_i < chars.len() {
                    self.matches_from(token_i + 1, char_i + 1, chars)
                } else {
                    false
                }
            }

            WildcardToken::OneOf(set) => {
                if char_i < chars.len() && set.contains(&chars[char_i]) {
                    self.matches_from(token_i + 1, char_i + 1, chars)
                } else {
                    false
                }
            }
        }
    }
}
