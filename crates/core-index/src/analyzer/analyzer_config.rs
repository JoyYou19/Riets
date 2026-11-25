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
    #[must_use]
    pub const fn name(self) -> &'static str {
        
        match self {
            Self::Arabic => "arabic",
            Self::Danish => "danish",
            Self::Dutch => "dutch",
            Self::English => "english",
            Self::Finnish => "finnish",
            Self::French => "french",
            Self::German => "german",
            Self::Greek => "greek",
            Self::Hungarian => "hungarian",
            Self::Italian => "italian",
            Self::Norwegian => "norwegian",
            Self::Portuguese => "portuguese",
            Self::Romanian => "romanian",
            Self::Russian => "russian",
            Self::Spanish => "spanish",
            Self::Swedish => "swedish",
            Self::Tamil => "tamil",
            Self::Turkish => "turkish",
        }
    }

    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        
        Some(match s.to_ascii_lowercase().as_str() {
            "arabic" => Self::Arabic,
            "danish" =>Self::Danish,
            "dutch" => Self::Dutch,
            "english" => Self::English,
            "finnish" => Self::Finnish,
            "french" => Self::French,
            "german" => Self::German,
            "greek" => Self::Greek,
            "hungarian" => Self::Hungarian,
            "italian" => Self::Italian,
            "norwegian" => Self::Norwegian,
            "portuguese" => Self::Portuguese,
            "romanian" => Self::Romanian,
            "russian" => Self::Russian,
            "spanish" => Self::Spanish,
            "swedish" => Self::Swedish,
            "tamil" => Self::Tamil,
            "turkish" => Self::Turkish,
            _ => return None,
        })
    }
    #[must_use]
    pub const fn algorithm(self) -> rust_stemmers::Algorithm {
        use rust_stemmers::Algorithm as A;
        match self {
            Self::Arabic => A::Arabic,
            Self::Danish => A::Danish,
            Self::Dutch => A::Dutch,
            Self::English => A::English,
            Self::Finnish => A::Finnish,
            Self::French => A::French,
            Self::German => A::German,
            Self::Greek => A::Greek,
            Self::Hungarian => A::Hungarian,
            Self::Italian => A::Italian,
            Self::Norwegian => A::Norwegian,
            Self::Portuguese => A::Portuguese,
            Self::Romanian => A::Romanian,
            Self::Russian => A::Russian,
            Self::Spanish => A::Spanish,
            Self::Swedish => A::Swedish,
            Self::Tamil => A::Tamil,
            Self::Turkish => A::Turkish,
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
        Self::from_name(&s)
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
    #[must_use]
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

impl<S: TokenStream> TokenStream for LengthFilterStream<S> {
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
