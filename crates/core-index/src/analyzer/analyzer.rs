use std::{collections::{HashMap, HashSet}, sync::atomic::{AtomicU64, Ordering}};


use core_timing::timed;
use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, Stemmer, StopWordFilter, TextAnalyzer, TokenStream,
};

use crate::analyzer::{token::Token, word_delimiter::WordDelimiterTokenizer};

#[derive(Clone)]
pub struct Analyzer {
    id:u64,
    analyzer: TextAnalyzer,
    literal_symbols: HashSet<char>,
}
use std::cell::RefCell;


//back to local analyzers 
const MAX_CACHED_ANALYZERS_PER_THREAD: usize = 64;
static NEXT_ANALYZER_ID: AtomicU64 = AtomicU64::new(0);
thread_local! {
    // tantivy's token_stream needs &mut TextAnalyzer, so each thread keeps its own
    // instance per analyzer configuration (keyed by Analyzer::id).
    static LOCAL_ANALYZERS: RefCell<HashMap<u64, TextAnalyzer>> = RefCell::new(HashMap::new());
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
     #[timed(indexing_documents)]
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
            id: NEXT_ANALYZER_ID.fetch_add(1, Ordering::Relaxed),
            analyzer,
            literal_symbols,
        }
    }

    fn with_text_analyzer<R>(&self, f: impl FnOnce(&mut TextAnalyzer) -> R) -> R {
        LOCAL_ANALYZERS.with(|cell| {
            let mut cache = cell.borrow_mut();
            if !cache.contains_key(&self.id) && cache.len() >= MAX_CACHED_ANALYZERS_PER_THREAD {
                cache.clear();
            }
            let analyzer = cache.entry(self.id).or_insert_with(|| self.analyzer.clone());
            f(analyzer)
        })
    }

    /// Streams every token's text and position to `f` without allocating per token.
    /// Returns the number of tokens produced (the field's document length).
    #[timed(indexing_documents)]
    pub fn for_each_token(&self, input: &str, mut f: impl FnMut(&str, u32)) -> u32 {
        self.with_text_analyzer(|analyzer| {
            let mut stream = analyzer.token_stream(input);
            let mut count: u32 = 0;
            while let Some(token) = stream.next() {
                f(&token.text, token.position as u32);
                count = count.saturating_add(1);
            }
            count
        })
    }
    #[timed(indexing_documents)]
    pub fn analyze(&self, input: &str) -> Vec<Token> {
        self.with_text_analyzer(|analyzer| {
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
#[cfg(test)]
mod pipeline_cost {
    use super::*;
    use std::time::Instant;
    use tantivy::tokenizer::TokenStream;

    fn sample() -> String {
        std::env::var("ANALYZER_SAMPLE")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_else(|| {
                "Toy Story is a 1995 American computer-animated comedy film produced by Pixar \
                 Animation Studios and released by Walt Disney Pictures. The film follows a group \
                 of anthropomorphic toys who pretend to be lifeless whenever humans are present. "
                    .repeat(20_000)
            })
    }

    fn time_pipeline(name: &str, mut analyzer: TextAnalyzer, text: &str) {
        let started = Instant::now();
        let mut stream = analyzer.token_stream(text);
        let mut tokens = 0u64;
        while stream.advance() {
            tokens += 1;
        }
        let elapsed = started.elapsed();
        println!(
            "{name:<28} {tokens:>10} tokens  {:>10.2?}  {:>7.1} ns/token",
            elapsed,
            elapsed.as_nanos() as f64 / tokens.max(1) as f64
        );
    }

    #[test]
    #[ignore]
    fn analyzer_stage_costs() {
        let text = sample();
        let symbols: HashSet<char> = ['_'].into_iter().collect();
        let stopwords: HashSet<String> = ["a", "an", "the", "and", "or", "of", "is", "to", "in"]
            .into_iter()
            .map(str::to_string)
            .collect();

        time_pipeline(
            "tokenizer only",
            TextAnalyzer::builder(WordDelimiterTokenizer::new(symbols.clone())).build(),
            &text,
        );
        time_pipeline(
            "+ long, lower, stopwords",
            TextAnalyzer::builder(WordDelimiterTokenizer::new(symbols.clone()))
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(StopWordFilter::remove(stopwords.clone()))
                .build(),
            &text,
        );
        time_pipeline(
            "+ stemmer (full pipeline)",
            TextAnalyzer::builder(WordDelimiterTokenizer::new(symbols))
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(StopWordFilter::remove(stopwords))
                .filter(Stemmer::new(Language::English))
                .build(),
            &text,
        );
    }
}