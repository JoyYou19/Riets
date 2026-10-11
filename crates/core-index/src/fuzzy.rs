//WARN: stuff like bobin in "robin" wouldnt match if default prefix is set to 1, but it would
//drastically improve the peformance since we wouldnt need to "guess" the first character, left at 0
//for now

pub const DEFAULT_PREFIX_LENGTH: usize = 1;
pub const DEFAULT_MAX_EXPANSIONS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzyOptions {
    pub max_edits: u8,
    pub prefix_length: usize,
    pub max_expansions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzySpec {
    pub prefix_length: usize,
    pub max_expansions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyExpansion {
    pub term: String,
    pub edits: u8,
    pub doc_freq: u32,
}

impl FuzzyExpansion {
    pub fn new(term: impl Into<String>, edits: u8, doc_freq: u32) -> Self {
        Self {
            term: term.into(),
            edits,
            doc_freq,
        }
    }
}

pub fn default_max_edits(term: &str) -> u8 {
    match term.chars().count() {
        0..=3 => 0,
        4..=5 => 1,
        _ => 2,
    }
}

//prefixword -> (prefix word) based on prefix_chars
pub fn split_prefix(term: &str, prefix_chars: usize) -> (&str, &str) {
    let byte = term
        .char_indices()
        .nth(prefix_chars)
        .map(|(i, _)| i)
        .unwrap_or(term.len());
    term.split_at(byte)
}

//every possible strging within one edit distance of our fuzzed suffix
//utman -> atman , utman, batman,
pub fn candidates_within_one(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
    //WARN: mos sito vajag vairak kaa a-z kip ieklaut kkadus - / . ?
    let alphabet: Vec<char> = ('a'..='z').collect();
    let mut out = Vec::new();

    for i in 0..chars.len() {
        let mut c = chars.clone();
        c.remove(i);
        out.push(c.into_iter().collect());
    }

    for i in 0..=chars.len() {
        for &ch in &alphabet {
            let mut ins = chars.clone();
            ins.insert(i, ch);
            out.push(ins.into_iter().collect());

            if i < chars.len() {
                let mut sub = chars.clone();
                sub[i] = ch;
                out.push(sub.into_iter().collect());
            }
        }
    }

    for i in 0..chars.len().saturating_sub(1) {
        let mut t = chars.clone();
        t.swap(i, i + 1);
        out.push(t.into_iter().collect());
    }

    out
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[derive(Debug, Clone)]
pub struct FuzzyMatcher {
    prefix: String,
    suffix: String,
    max_edits: u8,
    candidates: Vec<String>,
}

impl FuzzyMatcher {
    pub fn new(term: &str, max_edits: u8, prefix_length: usize) -> Self {
        let term = term.to_lowercase();
        let (prefix, suffix) = split_prefix(&term, prefix_length);
        //INFO: for highlighting fuzzy resutls we sioply cant afford to check edits>1
        let candidates = if max_edits == 1 {
            candidates_within_one(suffix)
        } else {
            Vec::new()
        };
        Self {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
            max_edits,
            candidates,
        }
    }

    pub fn matches(&self, word: &str) -> bool {
        if !word.starts_with(&self.prefix) {
            return false;
        }
        let rest = &word[self.prefix.len()..];
        match self.max_edits {
            0 => rest == self.suffix,
            1 => self.candidates.iter().any(|c| c == rest),
            _ => levenshtein(rest, &self.suffix) <= self.max_edits as usize,
        }
    }
}
