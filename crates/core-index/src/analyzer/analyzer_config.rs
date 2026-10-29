use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use tantivy::tokenizer::Language as TantivyLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tokenizer {
    WordDelimiter,
    Keyword,
    Whitespace,
    Standard,
    Url,
    Code,
    Filepath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language(pub TantivyLanguage);

impl Language {
    pub fn name(self) -> &'static str {
        use tantivy::tokenizer::Language as L;
        match self.0 {
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
        use tantivy::tokenizer::Language as L;
        Some(Language(match s.to_ascii_lowercase().as_str() {
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
        }))
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
    LanguageDefaults(Language),
    Words(HashSet<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtmlStrip {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternReplace {
    pub pattern: String,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CharFilter {
    HtmlStrip { html_strip: HtmlStrip },
    PatternReplace { pattern_replace: PatternReplace },
}

//main analyzer starting point with everything needed for the config
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyzerConfig {
    pub tokenizer: Tokenizer,
    pub stemming: Option<Language>,
    pub lowercase: bool,
    pub ascii_folding: bool,
    pub stopwords: Option<Stopwords>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub literal_symbols: HashSet<char>,
    pub char_filters: Vec<CharFilter>,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            tokenizer: Tokenizer::WordDelimiter,
            stemming: None,
            lowercase: true,
            ascii_folding: false,
            stopwords: None,
            min_length: None,
            max_length: None,
            literal_symbols: HashSet::from(['_']),
            char_filters: Vec::new(),
        }
    }
}

impl AnalyzerConfig {
    //when a user removes $default we hardcode just in case
    pub fn builtin_default() -> Self {
        Self {
            stemming: Some(Language(TantivyLanguage::English)),
            ..Self::default()
        }
    }
}
