use crate::analyzer::analyzer_config::{
    AnalyzerConfig, CharFilter, CorelamoTokenizer, LengthFilter, Stopwords,
};
use crate::analyzer::cached_stemmer::CachedStemmer;
use crate::analyzer::filepath_tokenizer::FilepathTokenizer;
use crate::analyzer::small_tantivy::{
    AsciiFoldingFilter, BoxTokenStream, LowerCaser, RawTokenizer, RemoveLongFilter,
    SimpleTokenizer, StopWordFilter, TextAnalyzer, Tokenizer, WhitespaceTokenizer,
};
use crate::analyzer::stip_html::strip_html;
use crate::analyzer::strip_markdown::strip_markdown;
use crate::analyzer::url_tokenizer::UrlTokenizer;
use crate::analyzer::{token::Token, word_delimiter::WordDelimiterTokenizer};
use core_timing::timed;
use indexmap::IndexMap;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub struct Analyzer {
    id: u64,
    pipeline: TextAnalyzer,
    literal_symbols: HashSet<char>,
    lowercase: bool,
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

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl CompiledCharFilter {
    fn apply(&self, text: &str) -> String {
        match self {
            Self::StripHtml => strip_html(text),
            Self::StripMarkdown => strip_markdown(text),
            Self::PatternReplace { regex, replacement } => {
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
            Self::WordDelimiter(t) => t.token_stream(text),
            Self::Keyword(t) => BoxTokenStream::new(t.token_stream(text)),
            Self::Whitespace(t) => BoxTokenStream::new(t.token_stream(text)),
            Self::Standard(t) => BoxTokenStream::new(t.token_stream(text)),
            Self::Url(t) => t.token_stream(text),
            Self::Filepath(t) => t.token_stream(text),
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

/// The stopwords the config gives (tantivy's built-in per-language lists are huge, so they are not used).
fn stopword_filter(stop: &Stopwords) -> StopWordFilter {
    let Stopwords::Words(words) = stop;
    StopWordFilter::remove(words.iter().cloned())
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
const fn is_unicode_punctuation(c: char) -> bool {
    matches!(
        c,
        '\u{00A1}'..='\u{00BF}'
            | '\u{2010}'..='\u{2027}'
            | '\u{2030}'..='\u{205E}'
            | '\u{3000}'..='\u{303F}'
    )
}

impl Analyzer {
    #[must_use]
    pub fn new() -> Self {
        Self::with_literal_symbols(['_'])
    }

    #[timed(indexing_documents)]
    pub fn with_literal_symbols(symbols: impl IntoIterator<Item = char>) -> Self {
        let literal_symbols: HashSet<char> = symbols.into_iter().collect();

        let stopwords: HashSet<String> = ["the", "a", "of", "in", "to", "and", "is"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let pipeline = TextAnalyzer::builder(WordDelimiterTokenizer::new(literal_symbols.clone()))
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
            pipeline,
            literal_symbols,
            lowercase: true,
            char_filters: Vec::new(),
        }
    }

    /// Builds an analyzer from a config: tokenizer + ordered filter chain.
    #[must_use]
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
            builder = builder.filter_dynamic(stopword_filter(stop));
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
        let pipeline = builder.build();
        Self {
            id: NEXT_ANALYZER_ID.fetch_add(1, Ordering::Relaxed),
            pipeline,
            literal_symbols,
            lowercase: config.lowercase,
            char_filters,
        }
    }

    fn apply_char_filters(&self, input: &str) -> String {
        let mut text = input.to_owned();
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
                    position: to_u32(token.position),
                    position_length: to_u32(token.position_length),
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
                .or_insert_with(|| self.pipeline.clone());
            f(analyzer)
        })
    }

    #[must_use]
    pub const fn has_char_filters(&self) -> bool {
        !self.char_filters.is_empty()
    }

    #[must_use]
    pub const fn lowercases(&self) -> bool {
        self.lowercase
    }

    #[must_use]
    pub fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.literal_symbols.contains(&c)
    }

    /// A short text's words, analyzed and joined by spaces: the same for the typed
    /// query and the stored value, so "the penguins" and "Penguin" both give "penguin".
    /// A dropped stopword inside the text stays as `_` ("king of the hill" -> "king _ hill"), and
    /// so does one at the start unless it is an article: "In New York" is not "New York". Only a
    /// word's parts are kept, not its joined form: "Spider-Man" gives "spider man".
    #[must_use]
    pub fn name_key(&self, text: &str) -> Option<String> {
        let mut words: Vec<(u32, String)> = Vec::new();
        self.for_each_token(text, |term, position, length| {
            if length == 1 && words.last().is_none_or(|(last, _)| *last < position) {
                words.push((position, term.to_owned()));
            }
        });
        let first_position = words.first()?.0;
        if words.len() > MAX_NAME_WORDS {
            return None;
        }
        let leading_article = text
            .split_whitespace()
            .next()
            .is_some_and(|first| matches!(first.to_lowercase().as_str(), "the" | "a" | "an"));
        let mut key = String::with_capacity(text.len() + 4);
        let mut next = u32::from(first_position == 1 && leading_article);
        for (position, word) in &words {
            if *position > next {
                key.push_str("_ ");
            }
            key.push_str(word);
            key.push(' ');
            next = position.saturating_add(1);
        }
        key.pop();
        Some(key)
    }

    #[must_use]
    pub fn analyze_text(&self, input: &str) -> (String, Vec<Token>) {
        let filtered = self.apply_char_filters(input);
        let tokens = self.tokenize(&filtered);
        (filtered, tokens)
    }

    /// The name terms a field value is filed under: the whole value, and the value without a
    /// trailing "(qualifier)" or ", qualifier". Long values get none.
    #[must_use]
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
            if let Some(stripped_key) = self.name_key(stripped) {
                let term = format!("{QUALIFIED_NAME_KEY}{stripped_key}");
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
                    to_u32(token.position),
                    to_u32(token.position_length.max(1)),
                    to_u32(token.offset_from),
                    to_u32(token.offset_to),
                );
                if token.position >= next_unseen {
                    count = count.saturating_add(1);
                    next_unseen = token.position.saturating_add(1);
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
    #[must_use]
    pub fn analyze(&self, input: &str) -> Vec<Token> {
        let filtered = self.apply_char_filters(input);
        self.tokenize(&filtered)
    }

    #[timed(search)]
    #[must_use]
    pub fn analyze_query(&self, input: &str) -> Vec<Token> {
        let filtered = self.apply_char_filters(input);
        let canonical: String = filtered
            .chars()
            .filter(|c| {
                c.is_alphanumeric()
                    || c.is_whitespace()
                    || self.literal_symbols.contains(c)
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
        Self::from_policy(&IndexMap::new())
    }
}

impl AnalyzerSet {
    #[must_use]
    pub fn from_policy(configs: &IndexMap<String, AnalyzerConfig>) -> Self {
        let mut analyzers = HashMap::new();
        for (name, config) in configs {
            analyzers.insert(name.clone(), Analyzer::from_config(config));
        }
        let default = configs.get("$default").map_or_else(
            || Analyzer::from_config(&AnalyzerConfig::builtin_default()),
            Analyzer::from_config,
        );
        Self { analyzers, default }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Analyzer> {
        self.analyzers.get(name)
    }

    #[must_use]
    pub const fn default_analyzer(&self) -> &Analyzer {
        &self.default
    }
}

/// `value` without a trailing "(...)" and without everything from its first ", ".
fn unqualified(value: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(2);
    if value.ends_with(')')
        && let Some(open) = value.rfind(" (")
        && let Some(head) = value.get(..open).map(str::trim_end)
        && !head.is_empty()
    {
        out.push(head);
    }
    if let Some(comma) = value.find(", ")
        && let Some(head) = value.get(..comma).map(str::trim_end)
        && !head.is_empty()
        && !out.contains(&head)
    {
        out.push(head);
    }
    out
}