use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

use core_timing::timed;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, StopWordFilter, TextAnalyzer, TokenStream};

use crate::analyzer::{token::Token, word_delimiter::WordDelimiterTokenizer};

#[derive(Clone)]
pub struct Analyzer {
    id: u64,
    analyzer: TextAnalyzer,
    literal_symbols: HashSet<char>,
}
use std::cell::RefCell;

use crate::analyzer::cached_stemmer::CachedStemmer;

/// Per-thread stem cache size; cleared when full.
const STEM_CACHE_CAPACITY: usize = 100_000;
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
            .filter(CachedStemmer::new(STEM_CACHE_CAPACITY))
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
            let analyzer = cache
                .entry(self.id)
                .or_insert_with(|| self.analyzer.clone());
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

    pub fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.literal_symbols.contains(&c)
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
mod cached_stemmer_equivalence {
    use super::*;
    use crate::analyzer::cached_stemmer::CachedStemmer;
    use tantivy::tokenizer::{Language, Stemmer, TokenStream};

    fn pipeline_tokens(analyzer: &mut TextAnalyzer, text: &str) -> Vec<(String, usize)> {
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while let Some(token) = stream.next() {
            out.push((token.text.clone(), token.position));
        }
        out
    }

    #[test]
    fn cached_stemmer_matches_tantivy_stemmer() {
        let symbols: HashSet<char> = ['_'].into_iter().collect();
        let mut reference = TextAnalyzer::builder(WordDelimiterTokenizer::new(symbols.clone()))
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(Stemmer::new(Language::English))
            .build();
        let mut cached = TextAnalyzer::builder(WordDelimiterTokenizer::new(symbols))
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(CachedStemmer::new(8)) // tiny capacity also exercises eviction
            .build();

        let text = std::env::var("ANALYZER_SAMPLE")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_else(|| {
                "Running runners ran; generalizations generalized generously. Toy Story's \
                 sequels, organizations organized, happily happiness, caresses ponies ties \
                 spider_man Spider-Man ÜNICODE naïve café 2026"
                    .to_string()
            });

        // Two passes: the first fills the cache (misses), the second hits it.
        for pass in 0..2 {
            assert_eq!(
                pipeline_tokens(&mut cached, &text),
                pipeline_tokens(&mut reference, &text),
                "mismatch on pass {pass}"
            );
        }
    }
}
