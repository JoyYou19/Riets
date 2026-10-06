//biski vibe palidzeja

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

/// Longest multi-word variant accepted (bounds the per-position trie walk).
pub const MAX_PHRASE_TOKENS: usize = 8;

/// Splits text into alphanumeric tokens, preserving surface casing.
/// Use the same splitting rule as your index analyzer.
pub fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect()
}

fn normalize(token: &str) -> String {
    token.to_lowercase()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionaryError {
    EmptyVariant { text: String },
    PhraseTooLong { text: String, max: usize },
    GroupTooSmall { variants: usize },
    Line { line: usize, source: Box<DictionaryError> },
}
//Errors
impl fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyVariant { text } => {
                write!(f, "variant {text:?} contains no searchable tokens")
            }
            Self::PhraseTooLong { text, max } => {
                write!(f, "variant {text:?} exceeds the {max}-token limit")
            }
            Self::GroupTooSmall { variants } => write!(
                f,
                "a synonym group needs at least 2 distinct variants, found {variants}"
            ),
            Self::Line { line, source } => write!(f, "line {line}: {source}"),
        }
    }
}
impl std::error::Error for DictionaryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Line { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}



//Case mode Sensitive - important big letters insensitive not important 
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaseMode {
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Variant {
    tokens: Vec<String>,
    case: CaseMode,
}

impl Variant {
    /// Case-sensitive if the text contains any uppercase letter.
    pub fn new(text: &str) -> Result<Self, DictionaryError> {
        let case = if text.chars().any(char::is_uppercase) {
            CaseMode::Sensitive
        } else {
            CaseMode::Insensitive
        };
        Self::with_case(text, case)
    }

    pub fn with_case(text: &str, case: CaseMode) -> Result<Self, DictionaryError> {
        let tokens: Vec<String> = tokenize(text).into_iter().map(str::to_owned).collect();
        if tokens.is_empty() {
            return Err(DictionaryError::EmptyVariant { text: text.to_owned() });
        }
        if tokens.len() > MAX_PHRASE_TOKENS {
            return Err(DictionaryError::PhraseTooLong {
                text: text.to_owned(),
                max: MAX_PHRASE_TOKENS,
            });
        }
        Ok(Self { tokens, case })
    }

    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    pub fn case(&self) -> CaseMode {
        self.case
    }

    /// Lowercased token sequence used for matching.
    pub fn key(&self) -> Vec<String> {
        self.tokens.iter().map(|token| normalize(token)).collect()
    }

    /// `input` already matched this variant's normalized key in the trie.
    fn matches(&self, input: &[&str]) -> bool {
        match self.case {
            CaseMode::Insensitive => true,
            CaseMode::Sensitive => self
                .tokens
                .iter()
                .zip(input)
                .all(|(expected, actual)| expected == actual),
        }
    }

    fn to_node(&self) -> QueryNode {
        match self.case {
            CaseMode::Sensitive => QueryNode::Exact(self.tokens.clone()),
            CaseMode::Insensitive => {
                QueryNode::from_terms(self.tokens.iter().map(|token| normalize(token)).collect())
            }
        }
    }
}

/// Expanded query clause. `Term`/`Phrase` hold lowercased words for the
/// analyzed field, `Exact` holds case-preserved words for the exact field,
/// and `AnyOf` is a set of interchangeable alternatives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryNode {
    Term(String),
    Phrase(Vec<String>),
    Exact(Vec<String>),
    AnyOf(Vec<QueryNode>),
}

impl QueryNode {
    fn from_terms(terms: Vec<String>) -> Self {
        match <[String; 1]>::try_from(terms) {
            Ok([term]) => QueryNode::Term(term),
            Err(terms) => QueryNode::Phrase(terms),
        }
    }

    fn any_of(alternatives: Vec<QueryNode>) -> Self {
        let mut seen = HashSet::with_capacity(alternatives.len());
        let unique: Vec<QueryNode> = alternatives
            .into_iter()
            .filter(|node| seen.insert(node.clone()))
            .collect();
        match <[QueryNode; 1]>::try_from(unique) {
            Ok([only]) => only,
            Err(many) => QueryNode::AnyOf(many),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RomanConfig {
    pub enabled: bool,
    /// Default 2: excludes `I`, which is overwhelmingly the pronoun.
    pub min_value: u16,
    pub max_value: u16,
    /// Default 2: excludes `V`, `X`, `C`, `L`, `M`, `D` (vitamins, sizes, brands).
    pub min_len: usize,
    /// Also expand `2` to `II` so numeric queries find Roman-numbered docs.
    pub arabic_to_roman: bool,
    /// Canonical numerals that are far more often words or acronyms (uppercase).
    pub denylist: HashSet<String>,
}

impl Default for RomanConfig {
    fn default() -> Self {
        let denylist = [
            "MIX", "DIX", "LIV", "CIV", "DIV", "DC", "CD", "MD", "MC", "CC", "CL", "CV", "XL",
            "LI", "DI", "MI", "MM",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        Self {
            enabled: true,
            min_value: 2,
            max_value: 3999,
            min_len: 2,
            arabic_to_roman: true,
            denylist,
        }
    }
}

impl RomanConfig {
    fn accepts(&self, roman: &str, value: u16) -> bool {
        roman.len() >= self.min_len
            && (self.min_value..=self.max_value).contains(&value)
            && !self.denylist.contains(roman)
    }

    fn roman_value(&self, surface: &str) -> Option<u16> {
        let value = parse_roman(surface)?;
        self.accepts(surface, value).then_some(value)
    }

    fn roman_for_arabic(&self, surface: &str) -> Option<String> {
        if surface.is_empty()
            || surface.len() > 4
            || surface.starts_with('0')
            || !surface.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let value: u16 = surface.parse().ok()?;
        let roman = to_roman(value)?;
        self.accepts(&roman, value).then_some(roman)
    }
}

const ROMAN_TABLE: [(u16, &str); 13] = [
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (4, "IV"),
    (1, "I"),
];

fn to_roman(mut value: u16) -> Option<String> {
    if value == 0 || value > 3999 {
        return None;
    }
    let mut out = String::with_capacity(15);
    for &(unit, symbol) in &ROMAN_TABLE {
        while value >= unit {
            out.push_str(symbol);
            value -= unit;
        }
    }
    Some(out)
}

/// Strict parser: uppercase only, canonical form only (`IIII`, `VX`, `IL` rejected).
fn parse_roman(surface: &str) -> Option<u16> {
    // "MMMDCCCLXXXVIII" (3888) is the longest canonical numeral.
    if surface.is_empty() || surface.len() > 15 {
        return None;
    }
    let mut total: u32 = 0;
    let mut largest_seen: u32 = 0;
    for c in surface.chars().rev() {
        let value = match c {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1000,
            _ => return None,
        };
        if value < largest_seen {
            total = total.checked_sub(value)?;
        } else {
            total += value;
            largest_seen = value;
        }
    }
    let value = u16::try_from(total).ok()?;
    (to_roman(value)?.as_str() == surface).then_some(value)
}

#[derive(Debug, Clone, Copy)]
struct VariantRef {
    group: usize,
    variant: usize,
}

#[derive(Debug, Default)]
struct TrieNode {
    children: HashMap<String, usize>,
    terminals: Vec<VariantRef>,
}

#[derive(Debug, Clone, Default)]
pub struct DictionaryBuilder {
    groups: Vec<Vec<Variant>>,
    roman: RomanConfig,
}

fn dedupe_group(variants: Vec<Variant>) -> Result<Vec<Variant>, DictionaryError> {
    let mut seen = HashSet::with_capacity(variants.len());
    let unique: Vec<Variant> = variants
        .into_iter()
        .filter(|variant| seen.insert(variant.clone()))
        .collect();
    if unique.len() < 2 {
        return Err(DictionaryError::GroupTooSmall { variants: unique.len() });
    }
    Ok(unique)
}

pub fn parse_variant(spec: &str) -> Result<Variant, DictionaryError> {
    match spec.strip_prefix('~') {
        Some(rest) => Variant::with_case(rest, CaseMode::Insensitive),
        None => Variant::new(spec),
    }
}

impl DictionaryBuilder {
    pub fn roman(&mut self, config: RomanConfig) -> &mut Self {
        self.roman = config;
        self
    }

    pub fn add_group(&mut self, variants: Vec<Variant>) -> Result<&mut Self, DictionaryError> {
        self.groups.push(dedupe_group(variants)?);
        Ok(self)
    }

    pub fn add(&mut self, variants: &[&str]) -> Result<&mut Self, DictionaryError> {
        let parsed = variants
            .iter()
            .map(|spec| parse_variant(spec))
            .collect::<Result<Vec<_>, _>>()?;
        self.add_group(parsed)
    }

   
    pub fn parse(&mut self, source: &str) -> Result<&mut Self, DictionaryError> {
        let mut parsed = Vec::new();
        for (index, raw) in source.lines().enumerate() {
            let content = raw.find('#').map_or(raw, |pos| &raw[..pos]).trim();
            if content.is_empty() {
                continue;
            }
            let group = content
                .split('|')
                .map(str::trim)
                .filter(|spec| !spec.is_empty())
                .map(parse_variant)
                .collect::<Result<Vec<_>, _>>()
                .and_then(dedupe_group)
                .map_err(|err| DictionaryError::Line {
                    line: index + 1,
                    source: Box::new(err),
                })?;
            parsed.push(group);
        }
        self.groups.extend(parsed);
        Ok(self)
    }

    pub fn build(&self) -> SynonymDictionary {
        let mut nodes = vec![TrieNode::default()];
        for (group_index, variants) in self.groups.iter().enumerate() {
            for (variant_index, variant) in variants.iter().enumerate() {
                let mut node = 0;
                for key in variant.key() {
                    let existing = nodes[node].children.get(&key).copied();
                    node = match existing {
                        Some(next) => next,
                        None => {
                            let id = nodes.len();
                            nodes.push(TrieNode::default());
                            nodes[node].children.insert(key, id);
                            id
                        }
                    };
                }
                nodes[node].terminals.push(VariantRef {
                    group: group_index,
                    variant: variant_index,
                });
            }
        }
        SynonymDictionary {
            nodes,
            groups: self.groups.clone(),
            roman: self.roman.clone(),
        }
    }
}

/// Immutable, cheaply shareable expansion dictionary.
#[derive(Debug)]
pub struct SynonymDictionary {
    nodes: Vec<TrieNode>,
    groups: Vec<Vec<Variant>>,
    roman: RomanConfig,
}

impl SynonymDictionary {
    pub fn builder() -> DictionaryBuilder {
        DictionaryBuilder::default()
    }
    
    pub fn from_source(source: &str) -> Result<Self, DictionaryError> {
        Ok(Self::builder().parse(source)?.build())
    }

    pub fn expand_query(&self, text: &str) -> Vec<QueryNode> {
        self.expand_tokens(&tokenize(text))
    }

    /// One clause per consumed span, in query order.
    pub fn expand_tokens(&self, tokens: &[&str]) -> Vec<QueryNode> {
        let mut clauses = Vec::with_capacity(tokens.len());
        let mut position = 0;
        while position < tokens.len() {
            if let Some((len, node)) = self.match_at(&tokens[position..], |_| true) {
                clauses.push(node);
                position += len;
            } else {
                let surface = tokens[position];
                clauses.push(
                    self.expand_number(surface)
                        .unwrap_or_else(|| QueryNode::Term(normalize(surface))),
                );
                position += 1;
            }
        }
        clauses
    }

    /// Longest dictionary match starting at `tokens[0]` whose length satisfies
    /// `accept_len` (e.g. "ends on a word boundary of the original query").
    /// Returns the number of tokens consumed and all alternatives of every
    /// matching group, the matched variant included.
    pub fn match_at(
        &self,
        tokens: &[&str],
        accept_len: impl Fn(usize) -> bool,
    ) -> Option<(usize, QueryNode)> {
        let mut node = 0;
        let mut best: Option<(usize, Vec<usize>)> = None;
        for (depth, surface) in tokens.iter().take(MAX_PHRASE_TOKENS).enumerate() {
            match self.nodes[node].children.get(&normalize(surface)) {
                Some(&next) => node = next,
                None => break,
            }
            let len = depth + 1;
            if !accept_len(len) {
                continue;
            }
            let mut groups: Vec<usize> = self.nodes[node]
                .terminals
                .iter()
                .filter(|r| self.groups[r.group][r.variant].matches(&tokens[..len]))
                .map(|r| r.group)
                .collect();
            if !groups.is_empty() {
                groups.sort_unstable();
                groups.dedup();
                best = Some((len, groups));
            }
        }
        best.map(|(len, groups)| {
            let mut alternatives: Vec<QueryNode> = groups
                .iter()
                .flat_map(|&group| self.groups[group].iter())
                .map(Variant::to_node)
                .collect();
            //a dictionary entry for a number ("two | 2") keeps its Roman form too
            if len == 1 {
                if let Some(QueryNode::AnyOf(numbers)) = self.expand_number(tokens[0]) {
                    alternatives.extend(numbers);
                }
            }
            (len, QueryNode::any_of(alternatives))
        })
    }

    /// Roman <-> Arabic expansion of one token, or `None` if it isn't an
    /// accepted numeral or number.
    ///
    /// Only an uppercase numeral in the query triggers expansion (`II`, not
    /// `ii` or `mix`), but the numeral is searched as a lowercase `Term`:
    /// most fields only have the lowercased index, where `II` is stored as
    /// `ii`, so an exact-case lookup would never match. In indexed text a
    /// standalone `ii`/`viii` is almost always the numeral, and the
    /// denylist covers the ones that are common words.
    pub fn expand_number(&self, surface: &str) -> Option<QueryNode> {
        if !self.roman.enabled {
            return None;
        }
        if let Some(value) = self.roman.roman_value(surface) {
            return Some(QueryNode::any_of(vec![
                QueryNode::Term(normalize(surface)),
                QueryNode::Term(value.to_string()),
            ]));
        }
        if self.roman.arabic_to_roman {
            if let Some(roman) = self.roman.roman_for_arabic(surface) {
                return Some(QueryNode::any_of(vec![
                    QueryNode::Term(normalize(surface)),
                    QueryNode::Term(normalize(&roman)),
                ]));
            }
        }
        None
    }
}

/// Hot-swappable handle. Readers hold the lock only to clone an `Arc`, so
/// query threads never block on a dictionary rebuild.
#[derive(Debug)]
pub struct SynonymStore {
    current: RwLock<Arc<SynonymDictionary>>,
}

impl SynonymStore {
    pub fn new(dictionary: SynonymDictionary) -> Self {
        Self::from_arc(Arc::new(dictionary))
    }

    /// Starts from an already shared dictionary (e.g. the built-in default
    /// that every database without its own file uses).
    pub fn from_arc(dictionary: Arc<SynonymDictionary>) -> Self {
        Self { current: RwLock::new(dictionary) }
    }

    pub fn snapshot(&self) -> Arc<SynonymDictionary> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Installs a new dictionary and returns the previous one.
    pub fn replace(&self, dictionary: SynonymDictionary) -> Arc<SynonymDictionary> {
        self.replace_arc(Arc::new(dictionary))
    }

    /// Installs an already shared dictionary and returns the previous one.
    pub fn replace_arc(&self, dictionary: Arc<SynonymDictionary>) -> Arc<SynonymDictionary> {
        let mut guard = self.current.write().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *guard, dictionary)
    }
}

