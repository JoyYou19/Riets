use std::collections::HashSet;

use core_timing::timed;
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, Stemmer, StopWordFilter, TextAnalyzer, TokenStream,
};

use crate::analyzer::{token::Token, word_delimiter::WordDelimiterTokenizer};

#[derive(Clone)]
pub struct Analyzer {
    analyzer: TextAnalyzer,
    literal_symbols: HashSet<char>,
}
use std::cell::RefCell;

thread_local! {
    static LOCAL_ANALYZER: RefCell<Option<TextAnalyzer>> = RefCell::new(None);
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analyzer").finish_non_exhaustive()
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        //TODO: configurable
        Self::with_literal_symbols(['_'])
    }

    pub fn with_literal_symbols(symbols: impl IntoIterator<Item = char>) -> Self {
        let literal_symbols: HashSet<char> = symbols.into_iter().collect();
        //TODO: configurable
        let stopwords: HashSet<String> = [
            "a", "an", "the", "and", "or", "of", "is", "it", "this", "that", "he", "she", "you",
            "i", "am", "are", "was", "were", "be", "been", "being", "to", "in", "on", "for",
            "with", "as", "by", "at", "from", "but", "not", "his", "her", "their", "they", "we",
            "my", "your", "our", "who", "what", "when", "where", "why", "how",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let analyzer = TextAnalyzer::builder(WordDelimiterTokenizer::new(literal_symbols.clone()))
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(StopWordFilter::remove(stopwords))
            .filter(Stemmer::new(Language::English))
            .build();

        Self {
            analyzer,
            literal_symbols,
        }
    }

    #[timed(indexing_documents)]
    pub fn analyze(&self, input: &str) -> Vec<Token> {
         LOCAL_ANALYZER.with(|cell| {
            let mut local = cell.borrow_mut();
        let  analyzer = local.get_or_insert_with(|| self.analyzer.clone());
        let mut stream = analyzer.token_stream(input);
        let mut output = Vec::new();

        while let Some(token) = stream.next() {
            output.push(Token {
                text: token.text.clone(),
                position: token.position as u32,
                start_byte: token.offset_from,
                end_byte: token.offset_to,
            });
        }

        output
    })
    }

    //For search same thing like spider-man spiderman the same thing is in index so it would match
    #[timed(search)]
    pub fn analyze_query(&self, input: &str) -> Vec<Token> {
        let canonical: String = input
            .chars()
            .filter(|c| {
                c.is_alphanumeric() || c.is_whitespace() || self.literal_symbols.contains(c)
            })
            .collect();
        self.analyze(&canonical)
    }
}
