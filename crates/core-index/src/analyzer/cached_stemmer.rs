use ahash::AHashMap;
use tantivy::tokenizer::{Token as TantivyToken, TokenFilter, TokenStream, Tokenizer};

/// English Snowball stemmer with a per-instance stem cache.
/// Produces exactly the same output as tantivy's `Stemmer::new(Language::English)`.
#[derive(Clone, Debug)]
pub struct CachedStemmer {
    capacity: usize,
}

impl CachedStemmer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1) }
    }
}

impl TokenFilter for CachedStemmer {
    type Tokenizer<T: Tokenizer> = CachedStemmerFilter<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> CachedStemmerFilter<T> {
        CachedStemmerFilter {
            inner: tokenizer,
            cache: StemCache::new(self.capacity),
        }
    }
}

struct StemCache {
    capacity: usize,
    stems: AHashMap<String, String>,
}

impl StemCache {
    fn new(capacity: usize) -> Self {
        Self { capacity, stems: AHashMap::new() }
    }
}

pub struct CachedStemmerFilter<T> {
    inner: T,
    cache: StemCache,
}

impl<T: Clone> Clone for CachedStemmerFilter<T> {
    // Clones start with an empty cache: each thread-local analyzer owns its own.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            cache: StemCache::new(self.cache.capacity),
        }
    }
}

impl<T: Tokenizer> Tokenizer for CachedStemmerFilter<T> {
    type TokenStream<'a> = CachedStemmerTokenStream<'a, T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CachedStemmerTokenStream {
            tail: self.inner.token_stream(text),
            stemmer: rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English),
            cache: &mut self.cache,
        }
    }
}

pub struct CachedStemmerTokenStream<'a, S> {
    tail: S,
    stemmer: rust_stemmers::Stemmer,
    cache: &'a mut StemCache,
}

impl<'a, S: TokenStream> TokenStream for CachedStemmerTokenStream<'a, S> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let token = self.tail.token_mut();

        if let Some(stemmed) = self.cache.stems.get(token.text.as_str()) {
            if *stemmed != token.text {
                token.text.clear();
                token.text.push_str(stemmed);
            }
            return true;
        }

        let stemmed = self.stemmer.stem(&token.text).into_owned();
        if self.cache.stems.len() >= self.cache.capacity {
            self.cache.stems.clear();
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