/*
* For many queries we will want to support
* things like architect* to also return architecture and architects instead of just the plain
* architect, so we create wildcard tokens to support those features
*/
use core_timing::timed;

use crate::posting::PostingList;

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
pub const MAX_PATTERN_TERMS: usize = 128;
impl WildcardPattern {
    #[timed(search)]
    pub fn parse(input: &str) -> Self {
        let mut tokens = Vec::new();
        let mut literal = String::new();
        let mut chars = input.chars();

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

    pub const fn is_prefix_only(&self) -> bool {
        matches!(
            self.tokens.as_slice(),
            [WildcardToken::Literal(_), WildcardToken::Any]
        )
    }

    #[timed(search)]
    pub fn matches(&self, word: &str) -> bool {
        let chars: Vec<char> = word.chars().collect();
        self.matches_from(0, 0, &chars)
    }

        fn matches_from(&self, token_i: usize, char_i: usize, chars: &[char]) -> bool {
        let Some(token) = self.tokens.get(token_i) else {
            return char_i == chars.len();
        };
        let rest = chars.get(char_i..).unwrap_or_default();

        match token {
            WildcardToken::Literal(lit) => {
                let len = lit.chars().count();
                let Some(window) = rest.get(..len) else {
                    return false;
                };
                window.iter().copied().eq(lit.chars())
                    && self.matches_from(token_i + 1, char_i + len, chars)
            }
            WildcardToken::Any => (char_i..=chars.len())
                .any(|next_i| self.matches_from(token_i + 1, next_i, chars)),
            WildcardToken::One => {
                !rest.is_empty() && self.matches_from(token_i + 1, char_i + 1, chars)
            }
            WildcardToken::OneOf(set) => {
                rest.first().is_some_and(|ch| set.contains(ch))
                    && self.matches_from(token_i + 1, char_i + 1, chars)
            }
        }
    }
}
/// The postings of the `MAX_PATTERN_TERMS` terms in the most documents, as one list.
/// Ties are broken by term so the result doesn't depend on map order.
pub(crate) fn capped_union(mut matches: Vec<(&str, &PostingList)>) -> PostingList {
    matches.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    matches.truncate(MAX_PATTERN_TERMS);

    let mut items = Vec::new();
    for (_, postings) in matches {
        items.extend_from_slice(postings.items());
    }
    PostingList::from_items(items)
}
