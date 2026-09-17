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
        4..=6 => 1,
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

//All strings within one edit (Damerau-Levenshtein) of `term`.
//Lowercase ASCII only, matching the lowercased index.
pub fn candidates_within_one(term: &str) -> Vec<String> {
    let chars: Vec<char> = term.chars().collect();
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
