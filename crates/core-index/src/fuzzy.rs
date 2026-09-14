//! Edit-distance fuzzy matching (distance 1 for now).

pub const DEFAULT_PREFIX_LENGTH: usize = 0;
pub const DEFAULT_MAX_EXPANSIONS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzyOptions {
    pub max_edits: u8,
    pub prefix_length: usize,
    pub max_expansions: usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_prefix_chars() {
        assert_eq!(split_prefix("schwarzenegger", 3), ("sch", "warzenegger"));
    }
    #[test]
    fn insertion_covers_wiliam() {
        assert!(candidates_within_one("wiliam").contains(&"william".to_string()));
    }
    #[test]
    fn transposition_covers_teh() {
        assert!(candidates_within_one("teh").contains(&"the".to_string()));
    }
    #[test]
    fn substitution_covers_kat() {
        assert!(candidates_within_one("kat").contains(&"cat".to_string()));
    }
    #[test]
    fn deletion_covers_hell() {
        assert!(candidates_within_one("hell").contains(&"hel".to_string()));
    }
}
