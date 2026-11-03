use ahash::AHashMap;
use rust_stemmers::Algorithm;
use tantivy::tokenizer::{Token as TantivyToken, TokenFilter, TokenStream, Tokenizer};

/// Snowball stemmer with a per-instance stem cache.
/// Produces the same output as tantivy's `Stemmer` for the given language.
#[derive(Clone, Debug)]
pub struct CachedStemmer {
    algorithm: Algorithm, // which language to stem
    capacity: usize,      // cache size limit
}

impl CachedStemmer {
    pub fn new(algorithm: Algorithm, capacity: usize) -> Self {
        Self {
            algorithm,
            capacity: capacity.max(1),
        }
    }
}

impl TokenFilter for CachedStemmer {
    type Tokenizer<T: Tokenizer> = CachedStemmerFilter<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> CachedStemmerFilter<T> {
        // wrap tokenizer with stemmer
        CachedStemmerFilter {
            inner: tokenizer,
            algorithm: self.algorithm,
            cache: StemCache::new(self.capacity),
        }
    }
}

struct StemCache {
    capacity: usize,
    stems: AHashMap<String, String>, // word → stemmed
}

impl StemCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            stems: AHashMap::new(),
        }
    }
}

pub struct CachedStemmerFilter<T> {
    inner: T,             // upstream tokenizer
    algorithm: Algorithm, // stemmer language
    cache: StemCache,     // per-instance stem cache
}

impl<T: Clone> Clone for CachedStemmerFilter<T> {
    fn clone(&self) -> Self {
        //fresh cache per clone
        Self {
            inner: self.inner.clone(),
            algorithm: self.algorithm,
            cache: StemCache::new(self.cache.capacity),
        }
    }
}

impl<T: Tokenizer> Tokenizer for CachedStemmerFilter<T> {
    type TokenStream<'a> = CachedStemmerTokenStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CachedStemmerTokenStream {
            tail: self.inner.token_stream(text),
            stemmer: rust_stemmers::Stemmer::create(self.algorithm),
            cache: &mut self.cache,
        }
    }
}

pub struct CachedStemmerTokenStream<'a, S> {
    tail: S,                         // inner token stream
    stemmer: rust_stemmers::Stemmer, // does the stemming
    cache: &'a mut StemCache,        // shared word cache
}

impl<'a, S: TokenStream> TokenStream for CachedStemmerTokenStream<'a, S> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let token = self.tail.token_mut();

        if let Some(stemmed) = self.cache.stems.get(token.text.as_str()) {
            // cache hit: reuse stem
            if *stemmed != token.text {
                token.text.clear();
                token.text.push_str(stemmed);
            }
            return true;
        }

        let stemmed = self.stemmer.stem(&token.text).into_owned();
        // cache miss: stem, store
        if self.cache.stems.len() >= self.cache.capacity {
            self.cache.stems.clear(); // full → evict all
        }
        self.cache.stems.insert(token.text.clone(), stemmed.clone());
        token.text = stemmed;
        true
    }

    fn token(&self) -> &TantivyToken {
        self.tail.token()
    }

    fn token_mut(&mut self) -> &mut TantivyToken {
        self.tail.token_mut()
    }
}
