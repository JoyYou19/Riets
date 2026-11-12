//holly vibe replacement of tantivy's token tokenizer.... only the parts we need, tests so far have shown that outputs match we dont need the rest of tantivvys lib

use std::{collections::HashSet, mem, str::CharIndices, sync::Arc};

use std::borrow::{Borrow, BorrowMut};
use std::ops::{Deref, DerefMut};

use crate::analyzer::ascii_folding_filter::fold;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Byte index of the first character of the token.
    pub offset_from: usize,
    /// Byte index one past the last character of the token.
    pub offset_to: usize,
    /// Position, counted in tokens.
    pub position: usize,
    /// The token text.
    pub text: String,
    /// Length counted in original tokens (almost always 1).
    pub position_length: usize,
}

impl Default for Token {
    fn default() -> Token {
        Token {
            offset_from: 0,
            offset_to: 0,
            position: usize::MAX,
            text: String::new(),
            position_length: 1,
        }
    }
}

impl Token {
    pub fn reset(&mut self) {
        self.offset_from = 0;
        self.offset_to = 0;
        self.position = usize::MAX;
        self.text.clear();
        self.position_length = 1;
    }
}

pub trait Tokenizer: 'static + Clone + Send + Sync {
    type TokenStream<'a>: TokenStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a>;
}

pub trait TokenStream {
    fn advance(&mut self) -> bool;
    fn token(&self) -> &Token;
    fn token_mut(&mut self) -> &mut Token;

    fn next(&mut self) -> Option<&Token> {
        if self.advance() {
            Some(self.token())
        } else {
            None
        }
    }

    fn process(&mut self, sink: &mut dyn FnMut(&Token)) {
        while self.advance() {
            sink(self.token());
        }
    }
}

pub trait TokenFilter: 'static + Send + Sync {
    type Tokenizer<T: Tokenizer>: Tokenizer;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T>;
}

pub struct BoxTokenStream<'a>(Box<dyn TokenStream + 'a>);

impl TokenStream for BoxTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.0.advance()
    }
    fn token(&self) -> &Token {
        self.0.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.0.token_mut()
    }
}

impl<'a> BoxTokenStream<'a> {
    pub fn new<T: TokenStream + 'a>(token_stream: T) -> BoxTokenStream<'a> {
        BoxTokenStream(Box::new(token_stream))
    }
}

impl<'a> Deref for BoxTokenStream<'a> {
    type Target = dyn TokenStream + 'a;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl DerefMut for BoxTokenStream<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

impl<'a> TokenStream for Box<dyn TokenStream + 'a> {
    fn advance(&mut self) -> bool {
        let token_stream: &mut dyn TokenStream = self.borrow_mut();
        token_stream.advance()
    }
    fn token<'b>(&'b self) -> &'b Token {
        let token_stream: &'b (dyn TokenStream + 'a) = self.borrow();
        token_stream.token()
    }
    fn token_mut<'b>(&'b mut self) -> &'b mut Token {
        let token_stream: &'b mut (dyn TokenStream + 'a) = self.borrow_mut();
        token_stream.token_mut()
    }
}

/// A `Tokenizer` with its `TokenStream` type erased, so analyzers can be stored/cloned.
pub trait BoxableTokenizer: 'static + Send + Sync {
    fn box_token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a>;
    fn box_clone(&self) -> Box<dyn BoxableTokenizer>;
}

impl<T: Tokenizer> BoxableTokenizer for T {
    fn box_token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        BoxTokenStream::new(self.token_stream(text))
    }
    fn box_clone(&self) -> Box<dyn BoxableTokenizer> {
        Box::new(self.clone())
    }
}

impl Tokenizer for Box<dyn BoxableTokenizer> {
    type TokenStream<'a> = BoxTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        (**self).box_token_stream(text)
    }
}

impl Clone for Box<dyn BoxableTokenizer> {
    fn clone(&self) -> Self {
        (**self).box_clone()
    }
}

#[derive(Clone)]
pub struct TextAnalyzer {
    tokenizer: Box<dyn BoxableTokenizer>,
}

impl<T: Tokenizer> From<T> for TextAnalyzer {
    fn from(tokenizer: T) -> Self {
        TextAnalyzer::builder(tokenizer).build()
    }
}

impl TextAnalyzer {
    pub fn builder<T: Tokenizer>(tokenizer: T) -> TextAnalyzerBuilder<T> {
        TextAnalyzerBuilder { tokenizer }
    }

    pub fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        self.tokenizer.token_stream(text)
    }
}

pub struct TextAnalyzerBuilder<T = Box<dyn BoxableTokenizer>> {
    tokenizer: T,
}

impl<T: Tokenizer> TextAnalyzerBuilder<T> {
    pub fn filter<F: TokenFilter>(self, token_filter: F) -> TextAnalyzerBuilder<F::Tokenizer<T>> {
        TextAnalyzerBuilder {
            tokenizer: token_filter.transform(self.tokenizer),
        }
    }

    /// Erase the tokenizer type (needed before dynamic filters).
    pub fn dynamic(self) -> TextAnalyzerBuilder {
        let boxed: Box<dyn BoxableTokenizer> = Box::new(self.tokenizer);
        TextAnalyzerBuilder { tokenizer: boxed }
    }

    /// Append a filter and erase the type in one step.
    pub fn filter_dynamic<F: TokenFilter>(self, token_filter: F) -> TextAnalyzerBuilder {
        self.filter(token_filter).dynamic()
    }

    pub fn build(self) -> TextAnalyzer {
        TextAnalyzer {
            tokenizer: Box::new(self.tokenizer),
        }
    }
}

// ---------------------------------------------------------------------------
// LowerCaser
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct LowerCaser;

impl TokenFilter for LowerCaser {
    type Tokenizer<T: Tokenizer> = LowerCaserFilter<T>;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        LowerCaserFilter {
            tokenizer,
            buffer: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct LowerCaserFilter<T> {
    tokenizer: T,
    buffer: String,
}

impl<T: Tokenizer> Tokenizer for LowerCaserFilter<T> {
    type TokenStream<'a> = LowerCaserTokenStream<'a, T::TokenStream<'a>>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.buffer.clear();
        LowerCaserTokenStream {
            tail: self.tokenizer.token_stream(text),
            buffer: &mut self.buffer,
        }
    }
}

pub struct LowerCaserTokenStream<'a, T> {
    buffer: &'a mut String,
    tail: T,
}

fn to_lowercase_unicode(text: &str, output: &mut String) {
    output.clear();
    output.reserve(50);
    for c in text.chars() {
        output.extend(c.to_lowercase());
    }
}

impl<T: TokenStream> TokenStream for LowerCaserTokenStream<'_, T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        if self.token_mut().text.is_ascii() {
            self.token_mut().text.make_ascii_lowercase();
        } else {
            to_lowercase_unicode(&self.tail.token().text, self.buffer);
            mem::swap(&mut self.tail.token_mut().text, self.buffer);
        }
        true
    }
    fn token(&self) -> &Token {
        self.tail.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

// ---------------------------------------------------------------------------
// RemoveLongFilter
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RemoveLongFilter {
    length_limit: usize,
}

impl RemoveLongFilter {
    pub fn limit(length_limit: usize) -> RemoveLongFilter {
        RemoveLongFilter { length_limit }
    }
}

impl TokenFilter for RemoveLongFilter {
    type Tokenizer<T: Tokenizer> = RemoveLongFilterWrapper<T>;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> RemoveLongFilterWrapper<T> {
        RemoveLongFilterWrapper {
            length_limit: self.length_limit,
            inner: tokenizer,
        }
    }
}

#[derive(Clone)]
pub struct RemoveLongFilterWrapper<T: Tokenizer> {
    length_limit: usize,
    inner: T,
}

impl<T: Tokenizer> Tokenizer for RemoveLongFilterWrapper<T> {
    type TokenStream<'a> = RemoveLongFilterStream<T::TokenStream<'a>>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        RemoveLongFilterStream {
            token_length_limit: self.length_limit,
            tail: self.inner.token_stream(text),
        }
    }
}

pub struct RemoveLongFilterStream<T> {
    token_length_limit: usize,
    tail: T,
}

impl<T: TokenStream> TokenStream for RemoveLongFilterStream<T> {
    fn advance(&mut self) -> bool {
        while self.tail.advance() {
            if self.tail.token().text.len() < self.token_length_limit {
                return true;
            }
        }
        false
    }
    fn token(&self) -> &Token {
        self.tail.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

// ---------------------------------------------------------------------------
// StopWordFilter (only `remove`; the language-based `new` lived in tantivy)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct StopWordFilter {
    words: Arc<HashSet<String>>,
}

impl StopWordFilter {
    pub fn remove<W: IntoIterator<Item = String>>(words: W) -> StopWordFilter {
        StopWordFilter {
            words: Arc::new(words.into_iter().collect()),
        }
    }
}

impl TokenFilter for StopWordFilter {
    type Tokenizer<T: Tokenizer> = StopWordFilterWrapper<T>;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> StopWordFilterWrapper<T> {
        StopWordFilterWrapper {
            words: self.words,
            inner: tokenizer,
        }
    }
}

#[derive(Clone)]
pub struct StopWordFilterWrapper<T> {
    words: Arc<HashSet<String>>,
    inner: T,
}

impl<T: Tokenizer> Tokenizer for StopWordFilterWrapper<T> {
    type TokenStream<'a> = StopWordFilterStream<T::TokenStream<'a>>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        StopWordFilterStream {
            words: self.words.clone(),
            tail: self.inner.token_stream(text),
        }
    }
}

pub struct StopWordFilterStream<T> {
    words: Arc<HashSet<String>>,
    tail: T,
}

impl<T: TokenStream> TokenStream for StopWordFilterStream<T> {
    fn advance(&mut self) -> bool {
        while self.tail.advance() {
            if !self.words.contains(&self.tail.token().text) {
                return true;
            }
        }
        false
    }
    fn token(&self) -> &Token {
        self.tail.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

// ---------------------------------------------------------------------------
// WhitespaceTokenizer / SimpleTokenizer / RawTokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct WhitespaceTokenizer {
    token: Token,
}

pub struct WhitespaceTokenStream<'a> {
    text: &'a str,
    chars: CharIndices<'a>,
    token: &'a mut Token,
}

impl Tokenizer for WhitespaceTokenizer {
    type TokenStream<'a> = WhitespaceTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> WhitespaceTokenStream<'a> {
        self.token.reset();
        WhitespaceTokenStream {
            text,
            chars: text.char_indices(),
            token: &mut self.token,
        }
    }
}

impl WhitespaceTokenStream<'_> {
    fn search_token_end(&mut self) -> usize {
        (&mut self.chars)
            .filter(|(_, c)| c.is_ascii_whitespace())
            .map(|(offset, _)| offset)
            .next()
            .unwrap_or(self.text.len())
    }
}

impl TokenStream for WhitespaceTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);
        while let Some((offset_from, c)) = self.chars.next() {
            if !c.is_ascii_whitespace() {
                let offset_to = self.search_token_end();
                self.token.offset_from = offset_from;
                self.token.offset_to = offset_to;
                self.token.text.push_str(&self.text[offset_from..offset_to]);
                return true;
            }
        }
        false
    }
    fn token(&self) -> &Token {
        self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

#[derive(Clone, Default)]
pub struct SimpleTokenizer {
    token: Token,
}

pub struct SimpleTokenStream<'a> {
    text: &'a str,
    chars: CharIndices<'a>,
    token: &'a mut Token,
}

impl Tokenizer for SimpleTokenizer {
    type TokenStream<'a> = SimpleTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> SimpleTokenStream<'a> {
        self.token.reset();
        SimpleTokenStream {
            text,
            chars: text.char_indices(),
            token: &mut self.token,
        }
    }
}

impl SimpleTokenStream<'_> {
    fn search_token_end(&mut self) -> usize {
        (&mut self.chars)
            .filter(|(_, c)| !c.is_alphanumeric())
            .map(|(offset, _)| offset)
            .next()
            .unwrap_or(self.text.len())
    }
}

impl TokenStream for SimpleTokenStream<'_> {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);
        while let Some((offset_from, c)) = self.chars.next() {
            if c.is_alphanumeric() {
                let offset_to = self.search_token_end();
                self.token.offset_from = offset_from;
                self.token.offset_to = offset_to;
                self.token.text.push_str(&self.text[offset_from..offset_to]);
                return true;
            }
        }
        false
    }
    fn token(&self) -> &Token {
        self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

#[derive(Clone, Default)]
pub struct RawTokenizer {
    token: Token,
}

pub struct RawTokenStream<'a> {
    token: &'a mut Token,
    has_token: bool,
}

impl Tokenizer for RawTokenizer {
    type TokenStream<'a> = RawTokenStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &str) -> RawTokenStream<'a> {
        self.token.reset();
        self.token.position = 0;
        self.token.position_length = 1;
        self.token.offset_from = 0;
        self.token.offset_to = text.len();
        self.token.text.clear();
        self.token.text.push_str(text);
        RawTokenStream {
            token: &mut self.token,
            has_token: true,
        }
    }
}

impl TokenStream for RawTokenStream<'_> {
    fn advance(&mut self) -> bool {
        let result = self.has_token;
        self.has_token = false;
        result
    }
    fn token(&self) -> &Token {
        self.token
    }
    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

// ---------------------------------------------------------------------------
// Stemmer (plain rust-stemmers wrapper, used as the test reference)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Stemmer {
    algorithm: rust_stemmers::Algorithm,
}

impl Stemmer {
    pub fn new(algorithm: rust_stemmers::Algorithm) -> Self {
        Self { algorithm }
    }
}

impl TokenFilter for Stemmer {
    type Tokenizer<T: Tokenizer> = StemmerFilter<T>;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        StemmerFilter {
            inner: tokenizer,
            algorithm: self.algorithm,
        }
    }
}

#[derive(Clone)]
pub struct StemmerFilter<T> {
    inner: T,
    algorithm: rust_stemmers::Algorithm,
}

impl<T: Tokenizer> Tokenizer for StemmerFilter<T> {
    type TokenStream<'a> = StemmerStream<T::TokenStream<'a>>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        StemmerStream {
            tail: self.inner.token_stream(text),
            stemmer: rust_stemmers::Stemmer::create(self.algorithm),
        }
    }
}

pub struct StemmerStream<T> {
    tail: T,
    stemmer: rust_stemmers::Stemmer,
}

impl<T: TokenStream> TokenStream for StemmerStream<T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        let stemmed = self.stemmer.stem(&self.tail.token().text).into_owned();
        if stemmed != self.tail.token().text {
            self.tail.token_mut().text = stemmed;
        }
        true
    }
    fn token(&self) -> &Token {
        self.tail.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}

// ---------------------------------------------------------------------------
// AsciiFoldingFilter
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AsciiFoldingFilter;

impl TokenFilter for AsciiFoldingFilter {
    type Tokenizer<T: Tokenizer> = AsciiFoldingFilterWrapper<T>;
    fn transform<T: Tokenizer>(self, tokenizer: T) -> Self::Tokenizer<T> {
        AsciiFoldingFilterWrapper {
            tokenizer,
            buffer: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct AsciiFoldingFilterWrapper<T> {
    tokenizer: T,
    buffer: String,
}

impl<T: Tokenizer> Tokenizer for AsciiFoldingFilterWrapper<T> {
    type TokenStream<'a> = AsciiFoldingFilterTokenStream<'a, T::TokenStream<'a>>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        self.buffer.clear();
        AsciiFoldingFilterTokenStream {
            buffer: &mut self.buffer,
            tail: self.tokenizer.token_stream(text),
        }
    }
}

pub struct AsciiFoldingFilterTokenStream<'a, T> {
    buffer: &'a mut String,
    tail: T,
}

impl<T: TokenStream> TokenStream for AsciiFoldingFilterTokenStream<'_, T> {
    fn advance(&mut self) -> bool {
        if !self.tail.advance() {
            return false;
        }
        if !self.token_mut().text.is_ascii() {
            // fold the non-ASCII token text in place
            fold(&self.tail.token().text, self.buffer);
            mem::swap(&mut self.tail.token_mut().text, self.buffer);
        }
        true
    }
    fn token(&self) -> &Token {
        self.tail.token()
    }
    fn token_mut(&mut self) -> &mut Token {
        self.tail.token_mut()
    }
}
