use crate::analyzer::analyzer_config::{
    AnalyzerConfig, CharFilter, CorelamoTokenizer, LengthFilter, Stopwords,
};
use crate::analyzer::cached_stemmer::CachedStemmer;
use crate::analyzer::filepath_tokenizer::FilepathTokenizer;
use crate::analyzer::stip_html::strip_html;
use crate::analyzer::strip_markdown::strip_markdown;
use crate::analyzer::url_tokenizer::UrlTokenizer;
use crate::analyzer::{token::Token, word_delimiter::WordDelimiterTokenizer};
use core_timing::timed;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, BoxTokenStream, LowerCaser, RawTokenizer, RemoveLongFilter,
    SimpleTokenizer, StopWordFilter, TextAnalyzer, TokenStream, Tokenizer, WhitespaceTokenizer,
};

#[derive(Clone)]
pub struct Analyzer {
    id: u64,
    analyzer: TextAnalyzer,
    literal_symbols: HashSet<char>,
    char_filters: Vec<CompiledCharFilter>,
}

#[derive(Clone)]
enum CompiledCharFilter {
    StripHtml,
    StripMarkdown,
    PatternReplace {
        regex: regex::Regex,
        replacement: String,
    },
}

impl CompiledCharFilter {
    fn apply(&self, text: &str) -> String {
        match self {
            CompiledCharFilter::StripHtml => strip_html(text),
            CompiledCharFilter::StripMarkdown => strip_markdown(text),
            CompiledCharFilter::PatternReplace { regex, replacement } => {
                regex.replace_all(text, replacement).into_owned()
            }
        }
    }
}

/// Per-thread stem cache size; cleared when full.
const STEM_CACHE_CAPACITY: usize = 100_000;
// back to local analyzers
const MAX_CACHED_ANALYZERS_PER_THREAD: usize = 64;
static NEXT_ANALYZER_ID: AtomicU64 = AtomicU64::new(0);
/// Index term that marks a whole field value (a title, one redirect name): its analyzed words
/// joined by spaces, behind this character. "The Penguins" is filed as `\u{1}penguin`.
pub const NAME_KEY: char = '\u{1}';
/// The same for a value without its qualifier: "Tom Holland (author)" and "Kansas City, Missouri"
/// are also filed as `\u{2}tom holland` and `\u{2}kansa citi`.
pub const QUALIFIED_NAME_KEY: char = '\u{2}';

/// Only values this short are names; longer text (article bodies) gets no key.
pub const MAX_NAME_WORDS: usize = 12;
const MAX_NAME_BYTES: usize = 200;

thread_local! {
    // tantivy's token_stream needs &mut TextAnalyzer, so each thread keeps its own
    // instance per analyzer configuration (keyed by Analyzer::id).
    static LOCAL_ANALYZERS: RefCell<HashMap<u64, TextAnalyzer>> = RefCell::new(HashMap::new());
}

// The tokenizer a config maps to. url/filepath fall back to word_delimiter
// until Steps 11/12 build the real ones.
#[derive(Clone)]
enum ConfigTokenizer {
    WordDelimiter(WordDelimiterTokenizer),
    Keyword(RawTokenizer),
    Whitespace(WhitespaceTokenizer),
    Standard(SimpleTokenizer),
    Url(UrlTokenizer),
    Filepath(FilepathTokenizer),
}

impl Tokenizer for ConfigTokenizer {
    type TokenStream<'a> = BoxTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> BoxTokenStream<'a> {
        match self {
            ConfigTokenizer::WordDelimiter(t) => t.token_stream(text),
            ConfigTokenizer::Keyword(t) => BoxTokenStream::new(t.token_stream(text)),
            ConfigTokenizer::Whitespace(t) => BoxTokenStream::new(t.token_stream(text)),
            ConfigTokenizer::Standard(t) => BoxTokenStream::new(t.token_stream(text)),
            ConfigTokenizer::Url(t) => t.token_stream(text),
            ConfigTokenizer::Filepath(t) => t.token_stream(text),
        }
    }
}

// AnalyzerConfig -> tokenizer
fn build_tokenizer(config: &AnalyzerConfig) -> ConfigTokenizer {
    let symbols: HashSet<char> = config.literal_symbols.iter().copied().collect();
    match config.tokenizer {
        CorelamoTokenizer::WordDelimiter => ConfigTokenizer::WordDelimiter(
            WordDelimiterTokenizer::with_options(symbols, false, false, config.preserve_original),
        ),
        CorelamoTokenizer::Code => ConfigTokenizer::WordDelimiter(
            WordDelimiterTokenizer::with_options(symbols, true, true, config.preserve_original),
        ),
        CorelamoTokenizer::Keyword => ConfigTokenizer::Keyword(RawTokenizer::default()),
        CorelamoTokenizer::Whitespace => {
            ConfigTokenizer::Whitespace(WhitespaceTokenizer::default())
        }
        CorelamoTokenizer::Standard => ConfigTokenizer::Standard(SimpleTokenizer::default()),

        CorelamoTokenizer::Filepath => {
            ConfigTokenizer::Filepath(FilepathTokenizer::new(symbols, config.preserve_original))
        }

        CorelamoTokenizer::Url => {
            ConfigTokenizer::Url(UrlTokenizer::new(symbols, config.preserve_original))
        }
    }
}

//either use the given or tantivy has built-in per-language stopwords too they have A LOT
fn stopword_filter(stop: &Stopwords) -> Option<StopWordFilter> {
    match stop {
        Stopwords::LanguageDefaults(lang) => StopWordFilter::new(lang.0),
        Stopwords::Words(words) => Some(StopWordFilter::remove(words.iter().cloned())),
    }
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

//ok bro...
// eliminates problem with - or other signs that are not from ASCII like AI -
fn is_unicode_punctuation(c: char) -> bool {
    matches!(
        c,
        '\u{00A1}'..='\u{00BF}'
            | '\u{2010}'..='\u{2027}'
            | '\u{2030}'..='\u{205E}'
            | '\u{3000}'..='\u{303F}'
    )
}

impl Analyzer {
    pub fn new() -> Self {
        Self::with_literal_symbols(['_'])
    }

    #[timed(indexing_documents)]
    pub fn with_literal_symbols(symbols: impl IntoIterator<Item = char>) -> Self {
        let literal_symbols: HashSet<char> = symbols.into_iter().collect();

        let stopwords: HashSet<String> = ["the", "a", "of", "in", "to", "and", "is"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let analyzer = TextAnalyzer::builder(WordDelimiterTokenizer::new(literal_symbols.clone()))
            .filter(RemoveLongFilter::limit(40))
            .filter(LowerCaser)
            .filter(StopWordFilter::remove(stopwords))
            .filter(CachedStemmer::new(
                rust_stemmers::Algorithm::English,
                STEM_CACHE_CAPACITY,
            ))
            .build();
        Self {
            id: NEXT_ANALYZER_ID.fetch_add(1, Ordering::Relaxed),
            analyzer,
            literal_symbols,
            char_filters: Vec::new(),
        }
    }

    /// Build an analyzer from a config: tokenizer + ordered filter chain.
    pub fn from_config(config: &AnalyzerConfig) -> Self {
        let literal_symbols: HashSet<char> = config.literal_symbols.iter().copied().collect();

        let mut builder = TextAnalyzer::builder(build_tokenizer(config)).dynamic();

        //min max token length
        if config.min_length.is_some() || config.max_length.is_some() {
            builder = builder.filter_dynamic(LengthFilter {
                min: config.min_length,
                max: config.max_length,
            });
        }

        //ABCda -> abcda
        if config.lowercase {
            builder = builder.filter_dynamic(LowerCaser);
        }

        //Jānis -> Janis
        if config.ascii_folding {
            builder = builder.filter_dynamic(AsciiFoldingFilter);
        }

        //stopwordi
        if let Some(stop) = &config.stopwords {
            if let Some(filter) = stopword_filter(stop) {
                builder = builder.filter_dynamic(filter);
            }
        }

        //stemming
        if let Some(lang) = &config.stemming {
            builder =
                builder.filter_dynamic(CachedStemmer::new(lang.algorithm(), STEM_CACHE_CAPACITY));
        }

        //<title>penis</title> -> penis
        let char_filters: Vec<CompiledCharFilter> = config
            .char_filters
            .iter()
            .filter_map(|f| match f {
                CharFilter::HtmlStrip { .. } => Some(CompiledCharFilter::StripHtml),
                CharFilter::MarkdownStrip { .. } => Some(CompiledCharFilter::StripMarkdown),
                CharFilter::PatternReplace { pattern_replace } => {
                    regex::Regex::new(&pattern_replace.pattern)
                        .ok()
                        .map(|regex| CompiledCharFilter::PatternReplace {
                            regex,
                            replacement: pattern_replace.replacement.clone(),
                        })
                }
            })
            .collect();

        //final-final
        let analyzer = builder.build();
        Self {
            id: NEXT_ANALYZER_ID.fetch_add(1, Ordering::Relaxed),
            analyzer,
            literal_symbols,
            char_filters,
        }
    }

    fn apply_char_filters(&self, input: &str) -> String {
        let mut text = input.to_string();
        for filter in &self.char_filters {
            text = filter.apply(&text);
        }
        text
    }

    fn tokenize(&self, input: &str) -> Vec<Token> {
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

    pub fn has_char_filters(&self) -> bool {
        !self.char_filters.is_empty()
    }

    pub fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.literal_symbols.contains(&c)
    }

    /// A short text's words, analyzed and joined by spaces: the same for the typed
    /// query and the stored value, so "the penguins" and "Penguin" both give "penguin".
    /// A dropped stopword inside the text stays as `_` ("king of the hill" -> "king _ hill"), and
    /// so does one at the start unless it is an article: "In New York" is not "New York". Only a
    /// word's parts are kept, not its joined form: "Spider-Man" gives "spider man".
    pub fn name_key(&self, text: &str) -> Option<String> {
        let mut words: Vec<(u32, String)> = Vec::new();
        self.for_each_token(text, |term, position, length| {
            if length == 1 && words.last().is_none_or(|(last, _)| *last < position) {
                words.push((position, term.to_owned()));
            }
        });
        if words.is_empty() || words.len() > MAX_NAME_WORDS {
            return None;
        }
        let leading_article = text
            .split_whitespace()
            .next()
            .is_some_and(|first| matches!(first.to_lowercase().as_str(), "the" | "a" | "an"));
        let mut key = String::with_capacity(text.len() + 4);
        let mut next = match words[0].0 {
            1 if leading_article => 1,
            _ => 0,
        };
        for (position, word) in &words {
            if *position > next {
                key.push_str("_ ");
            }
            key.push_str(word);
            key.push(' ');
            next = position + 1;
        }
        key.pop();
        Some(key)
    }

    pub fn analyze_text(&self, input: &str) -> (String, Vec<Token>) {
        let filtered = self.apply_char_filters(input);
        let tokens = self.tokenize(&filtered);
        (filtered, tokens)
    }

    /// The name terms a field value is filed under: the whole value, and the value without a
    /// trailing "(qualifier)" or ", qualifier". Long values get none.
    pub fn name_terms(&self, value: &str) -> Vec<String> {
        let value = value.trim();
        if value.len() > MAX_NAME_BYTES {
            return Vec::new();
        }
        let Some(key) = self.name_key(value) else {
            return Vec::new();
        };
        let mut terms = vec![format!("{NAME_KEY}{key}")];
        for stripped in unqualified(value) {
            if let Some(key) = self.name_key(stripped) {
                let term = format!("{QUALIFIED_NAME_KEY}{key}");
                if !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }
        terms
    }

    //now with byte offsets, //WARN: only words when !has_char_filters() cause it removes like <title> Hello </title> -> Hello
    pub fn for_each_token_with_offsets(
        &self,
        input: &str,
        mut f: impl FnMut(&str, u32, u32, u32, u32),
    ) -> u32 {
        let filtered = self.apply_char_filters(input);
        self.with_text_analyzer(|analyzer| {
            let mut stream = analyzer.token_stream(&filtered);
            let mut count: u32 = 0;
            let mut next_unseen: usize = 0;
            while let Some(token) = stream.next() {
                f(
                    &token.text,
                    token.position as u32,
                    token.position_length.max(1) as u32,
                    token.offset_from as u32,
                    token.offset_to as u32,
                );
                if token.position >= next_unseen {
                    count = count.saturating_add(1);
                    next_unseen = token.position + 1;
                }
            }
            count
        })
    }

    //es godigi sakot hujpis kapec sis helper ir vajadzigs, bet nu nevajag renamot pusi codebase
    pub fn for_each_token(&self, input: &str, mut f: impl FnMut(&str, u32, u32)) -> u32 {
        self.for_each_token_with_offsets(input, |term, pos, len, _start, _end| f(term, pos, len))
    }

    #[timed(indexing_documents)]
    pub fn analyze(&self, input: &str) -> Vec<Token> {
        let filtered = self.apply_char_filters(input);
        self.tokenize(&filtered)
    }

    #[timed(search)]
    pub fn analyze_query(&self, input: &str) -> Vec<Token> {
        let filtered = self.apply_char_filters(input);
        let canonical: String = filtered
            .chars()
            .filter(|c| {
                c.is_alphanumeric()
                    || c.is_whitespace()
                    || self.literal_symbols.contains(&c)
                    || !(c.is_ascii() || is_unicode_punctuation(*c))
            })
            .collect();
        self.tokenize(&canonical)
    }
}

/// Named analyzers + a default, built once from a policy's `analyzers` block.
#[derive(Clone)]
pub struct AnalyzerSet {
    analyzers: HashMap<String, Analyzer>,
    default: Analyzer,
}

impl Default for AnalyzerSet {
    fn default() -> Self {
        Self::from_policy(&BTreeMap::new())
    }
}

impl AnalyzerSet {
    pub fn from_policy(configs: &BTreeMap<String, AnalyzerConfig>) -> Self {
        let mut analyzers = HashMap::new();
        for (name, config) in configs {
            analyzers.insert(name.clone(), Analyzer::from_config(config));
        }
        let default = configs
            .get("$default")
            .map(Analyzer::from_config)
            .unwrap_or_else(|| Analyzer::from_config(&AnalyzerConfig::builtin_default()));
        Self { analyzers, default }
    }

    pub fn get(&self, name: &str) -> Option<&Analyzer> {
        self.analyzers.get(name)
    }

    pub fn default_analyzer(&self) -> &Analyzer {
        &self.default
    }
}

/// `value` without a trailing "(...)" and without everything from its first ", ".
fn unqualified(value: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(2);
    if value.ends_with(')') {
        if let Some(open) = value.rfind(" (") {
            let head = value[..open].trim_end();
            if !head.is_empty() {
                out.push(head);
            }
        }
    }
    if let Some(comma) = value.find(", ") {
        let head = value[..comma].trim_end();
        if !head.is_empty() && !out.contains(&head) {
            out.push(head);
        }
    }
    out
}

#[cfg(test)]
mod name_keys {
    use super::*;

    #[test]
    fn whole_value_and_unqualified_value() {
        let analyzer = Analyzer::new();
        assert_eq!(
            analyzer.name_terms("The Penguins"),
            vec![format!("{NAME_KEY}penguin")]
        );
        assert_eq!(analyzer.name_key("penguins").as_deref(), Some("penguin"));
        let terms = analyzer.name_terms("Tom Holland (author)");
        assert_eq!(terms.len(), 2);
        assert_eq!(
            terms[1],
            format!(
                "{QUALIFIED_NAME_KEY}{}",
                analyzer.name_key("tom holland").unwrap()
            )
        );
        assert_eq!(analyzer.name_terms("Kansas City, Missouri").len(), 2);
        assert_eq!(analyzer.name_terms("1965"), vec![format!("{NAME_KEY}1965")]);
        assert!(analyzer.name_terms(&"word ".repeat(40)).is_empty());
        assert_eq!(
            analyzer.name_key("Spider-Man"),
            analyzer.name_key("spider man")
        );
        assert_eq!(
            analyzer.name_key("King of the Hill").as_deref(),
            Some("king _ hill")
        );
        assert_ne!(
            analyzer.name_key("In New York"),
            analyzer.name_key("new york")
        );
        assert_eq!(
            analyzer.name_key("The Beatles"),
            analyzer.name_key("beatles")
        );
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
            .filter(CachedStemmer::new(rust_stemmers::Algorithm::English, 8)) // tiny capacity also exercises eviction
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

        for pass in 0..2 {
            assert_eq!(
                pipeline_tokens(&mut cached, &text),
                pipeline_tokens(&mut reference, &text),
                "mismatch on pass {pass}"
            );
        }
    }
}
