//! The synonym dictionary: groups of interchangeable forms (`CaP` = calcium phosphate),
//! one-way entries (a query for `king` also searches `monarch`) and Roman numerals (`II` = 2).
//!
//! Entries are looked up by the form the index stores for each word (lowercased and stemmed),
//! so an entry for `king` also fires for `kings` and `King`. `expand_synonyms` rewrites a parsed
//! query with them: every span of words a dictionary knows becomes a `Query::Synonym`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use core_index::analyzer::Analyzer;
use core_timing::timed;

use crate::ast::{PhraseTerm, Query, SynonymAlternative};

/// Longest multi-word variant accepted (bounds the per-position trie walk).
pub const MAX_PHRASE_TOKENS: usize = 8;

/// Splits text into its alphanumeric words, keeping their case.
pub fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect()
}

/// The form a word is looked up by: what the analyzer makes of it (lowercased and stemmed), or
/// the lowercased word when the analyzer drops it (a stopword), so every word keeps its place.
pub fn lookup_key(analyzer: &Analyzer, token: &str) -> String {
    analyzer
        .analyze(token)
        .into_iter()
        .next()
        .map(|token| token.text)
        .unwrap_or_else(|| token.to_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictionaryError {
    EmptyVariant { text: String },
    PhraseTooLong { text: String, max: usize },
    GroupTooSmall { variants: usize },
}

impl fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyVariant { text } => {
                write!(f, "variant {text:?} contains no searchable words")
            }
            Self::PhraseTooLong { text, max } => {
                write!(f, "variant {text:?} has more than {max} words")
            }
            Self::GroupTooSmall { variants } => write!(
                f,
                "an entry needs at least 2 distinct forms, found {variants}"
            ),
        }
    }
}

impl std::error::Error for DictionaryError {}

/// Case-sensitive forms (anything written with a capital: `CaP`, `US`) only match when typed
/// exactly like that, and search the case-preserving exact index. Case-insensitive forms match
/// however they are typed and search the analyzed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaseMode {
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Variant {
    // alphanumeric words as written, for matching the query
    tokens: Vec<String>,
    // the text as written with its punctuation, split on whitespace only, the way the exact
    // index stores it ("U.S.")
    exact: String,
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
            return Err(DictionaryError::EmptyVariant {
                text: text.to_owned(),
            });
        }
        if tokens.len() > MAX_PHRASE_TOKENS {
            return Err(DictionaryError::PhraseTooLong {
                text: text.to_owned(),
                max: MAX_PHRASE_TOKENS,
            });
        }
        let exact = text.split_whitespace().collect::<Vec<_>>().join(" ");
        Ok(Self {
            tokens,
            exact,
            case,
        })
    }

    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    pub fn case(&self) -> CaseMode {
        self.case
    }

    /// A short form: one word written with a capital or a digit (`CaP`, `WW2`, `~NaCl`).
    fn is_short_form(&self) -> bool {
        self.tokens.len() == 1
            && self.tokens[0]
                .chars()
                .any(|c| c.is_uppercase() || c.is_ascii_digit())
    }

    fn key(&self, analyzer: &Analyzer) -> Vec<String> {
        self.tokens
            .iter()
            .map(|token| lookup_key(analyzer, token))
            .collect()
    }

    /// `surface` already matched this variant's lookup key in the trie.
    fn matches(&self, surface: &[&str]) -> bool {
        match self.case {
            CaseMode::Insensitive => true,
            CaseMode::Sensitive => self
                .tokens
                .iter()
                .zip(surface)
                .all(|(expected, actual)| expected == actual),
        }
    }

    fn to_node(&self) -> QueryNode {
        match self.case {
            CaseMode::Sensitive => QueryNode::Exact(self.exact.clone()),
            CaseMode::Insensitive => QueryNode::from_terms(
                self.tokens
                    .iter()
                    .map(|token| token.to_lowercase())
                    .collect(),
            ),
        }
    }
}

/// One form a matched span can be searched as. `Term`/`Phrase` hold lowercased words for the
/// analyzed fields, `Exact` the case-preserved text for the exact fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryNode {
    Term(String),
    Phrase(Vec<PhraseTerm>),
    Exact(String),
}

impl QueryNode {
    fn from_terms(terms: Vec<String>) -> Self {
        match <[String; 1]>::try_from(terms.clone()) {
            Ok([term]) => QueryNode::Term(term),
           Err(terms) => {
            let phrase_terms = terms
            .into_iter()
            .enumerate()
            .map(|(index,text)|PhraseTerm{
                text,
                offset: index as u32,
            })
            .collect();
            QueryNode::Phrase(phrase_terms)
           }
        }
    }
}

/// A form the dictionary offers for a span of the query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Alternative {
    pub node: QueryNode,
    /// A Roman numeral or the number it stands for: an exact equivalent, not a loose synonym.
    pub numeral: bool,
}

/// A dictionary match at the start of the query words given to `match_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Query words consumed.
    pub len: usize,
    /// Every form of every entry that matched, the matched forms included.
    pub alternatives: Vec<Alternative>,
    /// The exact text of a case-sensitive form the words were typed as (`CaP` typed as `CaP`).
    pub typed_exact: Option<String>,
    /// The words were typed as a short form (`CaP`, `NaCl`, `WW2`), so a spelled-out form names
    /// the one meaning they have here. `salt` is a word, not a short form of sodium chloride.
    pub typed_short_form: bool,
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

/// An entry. A two-way group is found by any of its forms; a one-way entry only by its first
/// form, and then also searches the others (`king` -> `monarch`, never `monarch` -> `king`).
#[derive(Debug, Clone)]
struct Group {
    variants: Vec<Variant>,
    one_way: bool,
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
    groups: Vec<Group>,
    roman: RomanConfig,
}

fn dedupe_variants(variants: Vec<Variant>) -> Result<Vec<Variant>, DictionaryError> {
    let mut seen = HashSet::with_capacity(variants.len());
    let unique: Vec<Variant> = variants
        .into_iter()
        .filter(|variant| seen.insert(variant.clone()))
        .collect();
    if unique.len() < 2 {
        return Err(DictionaryError::GroupTooSmall {
            variants: unique.len(),
        });
    }
    Ok(unique)
}

/// A form as written in the dictionary: `~` in front makes it case-insensitive even with
/// capitals (`~Hg` also matches `hg`).
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

    /// Forms that all mean the same: any of them finds the others.
    pub fn add_group(&mut self, variants: Vec<Variant>) -> Result<&mut Self, DictionaryError> {
        self.groups.push(Group {
            variants: dedupe_variants(variants)?,
            one_way: false,
        });
        Ok(self)
    }

    /// `variants[0]` also searches the rest; the rest do not search it.
    pub fn add_one_way(&mut self, variants: Vec<Variant>) -> Result<&mut Self, DictionaryError> {
        self.groups.push(Group {
            variants: dedupe_variants(variants)?,
            one_way: true,
        });
        Ok(self)
    }

    pub fn add(&mut self, variants: &[&str]) -> Result<&mut Self, DictionaryError> {
        let parsed = variants
            .iter()
            .map(|spec| parse_variant(spec))
            .collect::<Result<Vec<_>, _>>()?;
        self.add_group(parsed)
    }

    pub fn build(&self, analyzer: &Analyzer) -> SynonymDictionary {
        let mut nodes = vec![TrieNode::default()];
        for (group_index, group) in self.groups.iter().enumerate() {
            let findable = if group.one_way {
                &group.variants[..1]
            } else {
                &group.variants[..]
            };
            for (variant_index, variant) in findable.iter().enumerate() {
                let mut node = 0;
                for key in variant.key(analyzer) {
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
    groups: Vec<Group>,
    roman: RomanConfig,
}

impl SynonymDictionary {
    pub fn builder() -> DictionaryBuilder {
        DictionaryBuilder::default()
    }

    /// Longest match starting at the first word, whose length satisfies `accept_len` (e.g. "ends
    /// on a word boundary of the original query"). `surface` are the words as typed and `keys`
    /// their `lookup_key`s.
    pub fn match_at(
        &self,
        surface: &[&str],
        keys: &[String],
        accept_len: impl Fn(usize) -> bool,
    ) -> Option<Match> {
        let mut node = 0;
        let mut best: Option<(usize, Vec<VariantRef>)> = None;
        for (depth, key) in keys.iter().take(MAX_PHRASE_TOKENS).enumerate() {
            match self.nodes[node].children.get(key) {
                Some(&next) => node = next,
                None => break,
            }
            let len = depth + 1;
            if !accept_len(len) {
                continue;
            }
            let matched: Vec<VariantRef> = self.nodes[node]
                .terminals
                .iter()
                .copied()
                .filter(|r| self.groups[r.group].variants[r.variant].matches(&surface[..len]))
                .collect();
            if !matched.is_empty() {
                best = Some((len, matched));
            }
        }

        let (len, matched) = best?;
        let typed_exact = matched.iter().find_map(|r| {
            let variant = &self.groups[r.group].variants[r.variant];
            (variant.case == CaseMode::Sensitive).then(|| variant.exact.clone())
        });
        let typed_short_form = matched
            .iter()
            .any(|r| self.groups[r.group].variants[r.variant].is_short_form());
        let mut groups: Vec<usize> = matched.iter().map(|r| r.group).collect();
        groups.sort_unstable();
        groups.dedup();

        let mut alternatives: Vec<Alternative> = Vec::new();
        for &group in &groups {
            for variant in &self.groups[group].variants {
                let node = variant.to_node();
                let numeral = matches!(&node, QueryNode::Term(term) if is_number(term));
                push_unique(&mut alternatives, Alternative { node, numeral });
            }
        }
        //an entry for a number ("eleven | 11") keeps its Roman form too
        if len == 1 {
            for alternative in self.expand_number(surface[0]).unwrap_or_default() {
                push_unique(&mut alternatives, alternative);
            }
        }
        Some(Match {
            len,
            alternatives,
            typed_exact,
            typed_short_form,
        })
    }

    /// The other form of a Roman numeral or number (`II` -> 2, `2` -> `ii`), or `None` if the
    /// word isn't an accepted numeral or number.
    ///
    /// Only an uppercase numeral triggers this (`II`, not `ii` or `mix`), but the numeral is
    /// searched lowercase: the analyzed index stores `II` as `ii`.
    pub fn expand_number(&self, surface: &str) -> Option<Vec<Alternative>> {
        if !self.roman.enabled {
            return None;
        }
        let other = if let Some(value) = self.roman.roman_value(surface) {
            value.to_string()
        } else if self.roman.arabic_to_roman {
            self.roman.roman_for_arabic(surface)?.to_lowercase()
        } else {
            return None;
        };
        Some(vec![
            Alternative {
                node: QueryNode::Term(surface.to_lowercase()),
                numeral: true,
            },
            Alternative {
                node: QueryNode::Term(other),
                numeral: true,
            },
        ])
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn push_unique(out: &mut Vec<Alternative>, alternative: Alternative) {
    match out.iter_mut().find(|known| known.node == alternative.node) {
        Some(known) => known.numeral |= alternative.numeral,
        None => out.push(alternative),
    }
}

/// What one database searches with: the shared default dictionary when it uses it, then its
/// own. A match is the longest one any of them has; matches of the same length are merged.
#[derive(Debug, Clone, Default)]
pub struct Synonyms {
    layers: Vec<Arc<SynonymDictionary>>,
}

impl Synonyms {
    pub fn new(layers: Vec<Arc<SynonymDictionary>>) -> Self {
        Self { layers }
    }

    /// `SynonymDictionary::match_at` over every dictionary.
    pub fn match_at(
        &self,
        surface: &[&str],
        keys: &[String],
        accept_len: impl Fn(usize) -> bool,
    ) -> Option<Match> {
        let mut best: Option<Match> = None;
        for layer in &self.layers {
            let Some(found) = layer.match_at(surface, keys, &accept_len) else {
                continue;
            };
            best = match best {
                Some(mut known) if known.len == found.len => {
                    for alternative in found.alternatives {
                        push_unique(&mut known.alternatives, alternative);
                    }
                    known.typed_exact = known.typed_exact.or(found.typed_exact);
                    known.typed_short_form |= found.typed_short_form;
                    Some(known)
                }
                Some(known) if known.len > found.len => Some(known),
                _ => Some(found),
            };
        }
        best
    }

    /// `SynonymDictionary::expand_number` of the first dictionary that expands numbers.
    pub fn expand_number(&self, surface: &str) -> Option<Vec<Alternative>> {
        self.layers
            .iter()
            .find_map(|layer| layer.expand_number(surface))
    }
}

/// Hot-swappable handle. Readers hold the lock only to clone an `Arc`, so query threads never
/// block on a dictionary rebuild.
#[derive(Debug)]
pub struct SynonymStore {
    current: RwLock<Arc<Synonyms>>,
}

impl SynonymStore {
    pub fn new(synonyms: Synonyms) -> Self {
        Self {
            current: RwLock::new(Arc::new(synonyms)),
        }
    }

    pub fn snapshot(&self) -> Arc<Synonyms> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Installs new synonyms and returns the previous ones.
    pub fn replace(&self, synonyms: Synonyms) -> Arc<Synonyms> {
        let mut guard = self.current.write().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *guard, Arc::new(synonyms))
    }
}

// ---------------------------------------------------------------- query expansion

/// Weight of the typed form and of exact equivalents: the spelled-out form of a typed short form
/// (`NaCl` -> sodium chloride names the one meaning the short form has here).
pub const FULL_WEIGHT: u16 = 1000;

/// Weight of a Roman numeral for a typed number and the other way round (`II` <-> 2).
pub const NUMERAL_WEIGHT: u16 = 1000;

/// Weight of a loose equivalent: a synonym, a broader word (`king` -> monarch), or a short form
/// found for typed words. "united states" -> `US` would also reach US Yachts and every other
/// US title, so such a match must not count as much as the words that were typed.
pub const SYNONYM_WEIGHT: u16 = 700;

type Combinator = fn(Vec<Query>) -> Query;

#[timed(search)]
pub fn expand_synonyms(query: Query, dictionary: &Synonyms, analyzer: &Analyzer) -> Query {
    match query {
        Query::Term(text) if is_multi_word(&text) => expand_multi_word(text, dictionary, analyzer),
        Query::Term(word) => {
            let expanded = expand_run(vec![word], dictionary, analyzer, Query::Wand);
            match <[Query; 1]>::try_from(expanded) {
                Ok([only]) => only,
                Err(many) => Query::Wand(many),
            }
        }
        Query::Wand(subs) => {
            Query::Wand(expand_children(subs, dictionary, analyzer, Query::Wand))
        }
        Query::And(subs) => Query::And(expand_children(subs, dictionary, analyzer, Query::And)),
        Query::Or(subs) => Query::Or(expand_children(subs, dictionary, analyzer, Query::Or)),
        //children map 1:1 onto same_element bindings, so they are never merged
        Query::SameElement(children) => Query::SameElement(
            children
                .into_iter()
                .map(|child| expand_synonyms(child, dictionary, analyzer))
                .collect(),
        ),
        other => other,
    }
}

fn is_multi_word(text: &str) -> bool {
    text.split_whitespace().nth(1).is_some()
}

/// The parser hands a plain string query over as one `Term` holding the whole text, which
/// `analyze_query` later turns into an `And` of its words. Expand it as exactly that `And`, so a
/// match can target single words inside it. Text the dictionary doesn't touch is returned as it
/// was.
fn expand_multi_word(text: String, dictionary: &Synonyms, analyzer: &Analyzer) -> Query {
    let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    let expanded = expand_run(words, dictionary, analyzer, Query::And);
    if expanded.iter().all(|part| matches!(part, Query::Term(_))) {
        return Query::Term(text);
    }
    Query::And(expanded)
}

fn expand_children(
    subs: Vec<Query>,
    dictionary: &Synonyms,
    analyzer: &Analyzer,
    wrap: Combinator,
) -> Vec<Query> {
    let mut out = Vec::with_capacity(subs.len());
    let mut run: Vec<String> = Vec::new();
    for sub in subs {
        match sub {
            Query::Term(word) if !is_multi_word(&word) => run.push(word),
            other => {
                if !run.is_empty() {
                    out.extend(expand_run(std::mem::take(&mut run), dictionary, analyzer, wrap));
                }
                out.push(expand_synonyms(other, dictionary, analyzer));
            }
        }
    }
    if !run.is_empty() {
        out.extend(expand_run(run, dictionary, analyzer, wrap));
    }
    out
}

/// Expands consecutive sibling words, in query order. `wrap` is the parent combinator, used to
/// rebuild the typed words of a multi-word match as one form.
fn expand_run(
    words: Vec<String>,
    dictionary: &Synonyms,
    analyzer: &Analyzer,
    wrap: Combinator,
) -> Vec<Query> {
    let mut surface: Vec<&str> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    let mut word_start: Vec<usize> = Vec::with_capacity(words.len() + 1);
    for (index, word) in words.iter().enumerate() {
        word_start.push(surface.len());
        for token in tokenize(word) {
            surface.push(token);
            owner.push(index);
        }
    }
    word_start.push(surface.len());
    let keys: Vec<String> = surface
        .iter()
        .map(|token| lookup_key(analyzer, token))
        .collect();

    let mut boundary = vec![false; surface.len() + 1];
    for &start in &word_start {
        boundary[start] = true;
    }

    let mut out = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let start = word_start[index];
        let end = word_start[index + 1];
        if start == end {
            //pure punctuation: left to the analyzer exactly as before
            out.push(Query::Term(words[index].clone()));
            index += 1;
            continue;
        }

        if let Some(found) = dictionary.match_at(&surface[start..], &keys[start..], |len| {
            boundary[start + len]
        }) {
            let last = owner[start + found.len - 1];
            let covered = &words[index..=last];
            let several_words = covered.len() > 1 || found.len > 1;
            //the typed words stay exactly what they are without the dictionary, so every document
            //they find is still found and scores at least as much
            let typed = original_form(covered, wrap);
            let typed_words: Vec<String> = surface[start..start + found.len]
                .iter()
                .map(|token| token.to_lowercase())
                .collect();
            let short_form = !several_words && found.typed_short_form;
            let alternatives: Vec<SynonymAlternative> = found
                .alternatives
                .into_iter()
                .filter(|alternative| !is_typed_form(&alternative.node, &typed_words))
                .map(|alternative| {
                    //the form typed with its exact case (`CaP`) is the typed word, more precisely
                    let typed_exact = matches!(
                        (&alternative.node, &found.typed_exact),
                        (QueryNode::Exact(text), Some(typed)) if text == typed
                    );
                    SynonymAlternative {
                        weight: if typed_exact {
                            FULL_WEIGHT
                        } else {
                            weight_of(&alternative, short_form, several_words)
                        },
                        query: to_query(alternative.node),
                    }
                })
                .collect();
            out.push(synonym(typed, alternatives));
            index = last + 1;
            continue;
        }

        if end - start == 1
            && let Some(numbers) = dictionary.expand_number(surface[start])
        {
            let typed = Query::Term(words[index].clone());
            let typed_words = vec![surface[start].to_lowercase()];
            let alternatives: Vec<SynonymAlternative> = numbers
                .into_iter()
                .filter(|alternative| !is_typed_form(&alternative.node, &typed_words))
                .map(|alternative| SynonymAlternative {
                    weight: weight_of(&alternative, false, false),
                    query: to_query(alternative.node),
                })
                .collect();
            out.push(synonym(typed, alternatives));
            index += 1;
            continue;
        }

        out.push(Query::Term(words[index].clone()));
        index += 1;
    }
    out
}

/// The dictionary's lowercase spelling of what was typed only finds what the typed words
/// already find.
fn is_typed_form(node: &QueryNode, typed_words: &[String]) -> bool {
    match node {
        QueryNode::Term(term) => typed_words.len() == 1 && *term == typed_words[0],
        QueryNode::Phrase(phrase_terms) => {
            phrase_terms.len() == typed_words.len() && phrase_terms
            .iter()
            .zip(typed_words)
            .all(|(pt, word)| pt.text==*word)
        }
        QueryNode::Exact(_) => false
    }
}

/// A spelled-out form counts fully when the typed words were a short form (`NaCl` names one
/// meaning) or were themselves spelled out: "world war 2", "world war ii" and "second world war"
/// are full names of the same thing. A short form found for typed words stays a loose match.
fn weight_of(alternative: &Alternative, typed_short_form: bool, typed_several_words: bool) -> u16 {
    if alternative.numeral {
        return NUMERAL_WEIGHT;
    }
    let spelled_out = match &alternative.node {
        QueryNode::Phrase(words) => words.len() > 1,
        QueryNode::Exact(text) => is_multi_word(text),
        QueryNode::Term(_) => false,
    };
    if spelled_out && (typed_short_form || typed_several_words) {
        FULL_WEIGHT
    } else {
        SYNONYM_WEIGHT
    }
}

fn to_query(node: QueryNode) -> Query {
    match node {
        QueryNode::Term(word) => Query::Term(word),
        QueryNode::Phrase(words) => Query::Phrase(words),
        QueryNode::Exact(text) => Query::Exact(text),
    }
}

fn original_form(words: &[String], wrap: Combinator) -> Query {
    match words {
        [word] => Query::Term(word.clone()),
        _ => wrap(words.iter().cloned().map(Query::Term).collect()),
    }
}

/// The typed form first, then the others; just the typed form when there are no others.
fn synonym(typed: Query, alternatives: Vec<SynonymAlternative>) -> Query {
    if alternatives.is_empty() {
        return typed;
    }
    let mut forms = Vec::with_capacity(alternatives.len() + 1);
    forms.push(SynonymAlternative {
        query: typed,
        weight: FULL_WEIGHT,
    });
    forms.extend(alternatives);
    Query::Synonym(forms)
}
