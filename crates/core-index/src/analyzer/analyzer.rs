use std::collections::HashSet;

use core_timing::timed;
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer,
    TokenStream,
};

use crate::analyzer::token::Token;

#[derive(Clone)]
pub struct Analyzer {
    analyzer: TextAnalyzer,
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
        //TODO: configurable:
        let stopwords: HashSet<String> = [
            "a", "an", "the", "and", "or", "of", "is", "it", "this", "that", "he", "she", "you",
            "i", "am", "are", "was", "were", "be", "been", "being", "to", "in", "on", "for",
            "with", "as", "by", "at", "from", "but", "not", "his", "her", "their", "they", "we",
            "my", "your", "our", "who", "what", "when", "where", "why", "how",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(StopWordFilter::remove(stopwords))
            .filter(Stemmer::new(Language::English))
            .build();

        Self { analyzer }
    }

    #[timed(indexing_documents)]
    pub fn analyze(&self, input: &str) -> Vec<Token> {
        let mut analyzer = self.analyzer.clone();
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
    }
}
