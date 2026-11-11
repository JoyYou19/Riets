use crate::analyzer::small_tantivy::{Token, TokenFilter, TokenStream, Tokenizer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorelamoTokenizer {
    WordDelimiter,
    Keyword,
    Whitespace,
    Standard,
    Url,
    Code,
    Filepath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Arabic,
    Danish,
    Dutch,
    English,
    Finnish,
    French,
    German,
    Greek,
    Hungarian,
    Italian,
    Norwegian,
    Portuguese,
    Romanian,
    Russian,
    Spanish,
    Swedish,
    Tamil,
    Turkish,
}

impl Language {
    pub fn name(self) -> &'static str {
        use Language as L;
        match self {
            L::Arabic => "arabic",
            L::Danish => "danish",
            L::Dutch => "dutch",
            L::English => "english",
            L::Finnish => "finnish",
            L::French => "french",
            L::German => "german",
            L::Greek => "greek",
            L::Hungarian => "hungarian",
            L::Italian => "italian",
            L::Norwegian => "norwegian",
            L::Portuguese => "portuguese",
            L::Romanian => "romanian",
            L::Russian => "russian",
            L::Spanish => "spanish",
            L::Swedish => "swedish",
            L::Tamil => "tamil",
            L::Turkish => "turkish",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        use Language as L;
        Some(match s.to_ascii_lowercase().as_str() {
            "arabic" => L::Arabic,
            "danish" => L::Danish,
            "dutch" => L::Dutch,
            "english" => L::English,
            "finnish" => L::Finnish,
            "french" => L::French,
            "german" => L::German,
            "greek" => L::Greek,
            "hungarian" => L::Hungarian,
            "italian" => L::Italian,
            "norwegian" => L::Norwegian,
            "portuguese" => L::Portuguese,
            "romanian" => L::Romanian,
            "russian" => L::Russian,
            "spanish" => L::Spanish,
            "swedish" => L::Swedish,
            "tamil" => L::Tamil,
            "turkish" => L::Turkish,
            _ => return None,
        })
    }

    pub fn algorithm(self) -> rust_stemmers::Algorithm {
        use Language as L;
        use rust_stemmers::Algorithm as A;
        match self {
            L::Arabic => A::Arabic,
            L::Danish => A::Danish,
            L::Dutch => A::Dutch,
            L::English => A::English,
            L::Finnish => A::Finnish,
            L::French => A::French,
            L::German => A::German,
            L::Greek => A::Greek,
            L::Hungarian => A::Hungarian,
            L::Italian => A::Italian,
            L::Norwegian => A::Norwegian,
            L::Portuguese => A::Portuguese,
            L::Romanian => A::Romanian,
            L::Russian => A::Russian,
            L::Spanish => A::Spanish,
            L::Swedish => A::Swedish,
            L::Tamil => A::Tamil,
            L::Turkish => A::Turkish,
        }
    }
}

impl Serialize for Language {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Language::from_name(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown language '{s}'")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stopwords {
    Words(HashSet<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtmlStrip {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownStrip {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternReplace {
    pub pattern: String,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CharFilter {
    HtmlStrip { html_strip: HtmlStrip },
    MarkdownStrip { markdown_strip: MarkdownStrip },
    PatternReplace { pattern_replace: PatternReplace },
}

//main analyzer starting point with everything needed for the config
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyzerConfig {
    pub tokenizer: CorelamoTokenizer,
    pub stemming: Option<Language>,
    pub lowercase: bool,
    pub ascii_folding: bool,
    pub stopwords: Option<Stopwords>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub preserve_original: bool,
    pub literal_symbols: HashSet<char>,
    pub char_filters: Vec<CharFilter>,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            tokenizer: CorelamoTokenizer::WordDelimiter,
            stemming: None,
            lowercase: true,
            ascii_folding: false,
            stopwords: None,
            min_length: Some(1),
            max_length: Some(40),
            preserve_original: true,
            literal_symbols: HashSet::from(['_']),
            char_filters: Vec::new(),
        }
    }
}

impl AnalyzerConfig {
    //when a user removes $default we hardcode just in case
    pub fn builtin_default() -> Self {
        Self {
            stemming: Some(Language::English),
            ..Self::default()
        }
    }
}

// Drops tokens whose byte length falls outside [min, max].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LengthFilter {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

impl TokenFilter for LengthFilter {
    type Tokenizer<T: Tokenizer> = LengthFilterWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> LengthFilterWrapper<T> {
        LengthFilterWrapper {
            inner: tokenizer,
            min: self.min,
            max: self.max,
        }
    }
}

#[derive(Clone)]
pub struct LengthFilterWrapper<T> {
    inner: T,
    min: Option<usize>,
    max: Option<usize>,
}

impl<T: Tokenizer> Tokenizer for LengthFilterWrapper<T> {
    type TokenStream<'a> = LengthFilterStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        LengthFilterStream {
            tail: self.inner.token_stream(text),
            min: self.min,
            max: self.max,
        }
    }
}

pub struct LengthFilterStream<S> {
    tail: S,
    min: Option<usize>,
    max: Option<usize>,
}

impl<'a, S: TokenStream> TokenStream for LengthFilterStream<S> {
    fn advance(&mut self) -> bool {
        while self.tail.advance() {
            let text = &self.tail.token().text;
            let len = self.tail.token().text.chars().count();
            // numbers and roman numerals carry meaning at any length ("world war 2", "rocky v")
            let keeps_meaning = text.chars().all(|c| c.is_ascii_digit());
            let too_short = !keeps_meaning && self.min.is_some_and(|m| len < m);
            let too_long = self.max.is_some_and(|m| len > m);
            if !too_short && !too_long {
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
